// SPDX-License-Identifier: MIT OR Apache-2.0

use anyhow::Result;
use chrono::{DateTime, Utc};
use gaming_telemetry::cpu::CpuMonitor;
use gaming_telemetry::manifest::{HostInfo, SessionManifest};
use gaming_telemetry::privacy;
use gaming_telemetry::session;
use gaming_telemetry::timing::TimingStats;
use nvml_wrapper::Nvml;
use nvml_wrapper::enum_wrappers::device::{Clock, TemperatureSensor};
use polars::prelude::*;
use std::fs::{File, remove_file, rename};
use std::path::{Path, PathBuf};
use tokio::task::JoinSet;
use tokio::time::{Duration, MissedTickBehavior, interval};

#[derive(Debug, Clone)]
struct GpuSample {
    timestamp: DateTime<Utc>,
    session_label: String,
    // `None` means the NVML/CPU sensor was unavailable, not that it read zero.
    power_usage_mw: Option<u32>,
    temperature_c: Option<u32>,
    graphics_clock_mhz: Option<u32>,
    memory_clock_mhz: Option<u32>,
    pcie_rx_throughput_kbps: Option<u32>,
    pcie_tx_throughput_kbps: Option<u32>,
    pstate: Option<u32>,
    throttle_reasons: Option<u64>,
    fan_speed_perc: Option<u32>,
    memory_used_mb: Option<u64>,
    memory_total_mb: Option<u64>,
    encoder_util_perc: Option<u32>,
    decoder_util_perc: Option<u32>,
    cpu_tctl_c: Option<f32>,
    cpu_ccd1_c: Option<f32>,
    cpu_ccd2_c: Option<f32>,
    cpu_package_power_w: Option<f32>,
}

const BUFFER_SIZE: usize = 2000; // ~10 seconds of data at default 5ms intervals
/// Cap outstanding async Parquet writes so a slow disk cannot queue unbounded batches.
const MAX_IN_FLIGHT_WRITES: usize = 2;

fn next_batch_id(batch_id: u32) -> Result<u32> {
    batch_id
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("batch ID namespace exhausted; start a new SESSION_DIR"))
}

/// Build the batch columns from one `"name": type = accessor` row per column.
///
/// Keeping the three together means a new or reordered `GpuSample` field cannot
/// silently transpose two columns that happen to share a type — the failure mode
/// of a long `df!` whose value list sits far from its column names.
macro_rules! batch_columns {
    ($samples:expr, $($name:literal: $ty:ty = $value:expr),+ $(,)?) => {
        vec![$(
            Column::new($name.into(), $samples.iter().map($value).collect::<Vec<$ty>>())
        ),+]
    };
}

/// Pack samples into the canonical batch schema.
///
/// `session_label` is borrowed, not cloned per row: it is invariant for a run.
fn build_batch_frame(samples: &[GpuSample]) -> Result<DataFrame> {
    let columns = batch_columns!(samples,
        "timestamp_ms": i64 = |s| s.timestamp.timestamp_millis(),
        "session_label": &str = |s| s.session_label.as_str(),
        "power_usage_mw": Option<u32> = |s| s.power_usage_mw,
        "temperature_c": Option<u32> = |s| s.temperature_c,
        "graphics_clock_mhz": Option<u32> = |s| s.graphics_clock_mhz,
        "memory_clock_mhz": Option<u32> = |s| s.memory_clock_mhz,
        "pcie_rx_kbps": Option<u32> = |s| s.pcie_rx_throughput_kbps,
        "pcie_tx_kbps": Option<u32> = |s| s.pcie_tx_throughput_kbps,
        "pstate": Option<u32> = |s| s.pstate,
        "throttle_reasons_bitmask": Option<u64> = |s| s.throttle_reasons,
        "fan_speed_perc": Option<u32> = |s| s.fan_speed_perc,
        "memory_used_mb": Option<u64> = |s| s.memory_used_mb,
        "memory_total_mb": Option<u64> = |s| s.memory_total_mb,
        "encoder_util_perc": Option<u32> = |s| s.encoder_util_perc,
        "decoder_util_perc": Option<u32> = |s| s.decoder_util_perc,
        "cpu_tctl_c": Option<f32> = |s| s.cpu_tctl_c,
        "cpu_ccd1_c": Option<f32> = |s| s.cpu_ccd1_c,
        "cpu_ccd2_c": Option<f32> = |s| s.cpu_ccd2_c,
        "cpu_package_power_w": Option<f32> = |s| s.cpu_package_power_w,
    );
    Ok(DataFrame::new(samples.len(), columns)?)
}

/// Map one NVML call into a Parquet-nullable reading.
///
/// `Err` is "not measured" (`None`). `Ok(0)` is a legitimate idle/zero reading
/// and must stay `Some(0)` — collapsing miss into `0` is what the ETL
/// `UNAVAILABLE_ZERO_FIELDS` gate refuses, and it is indistinguishable from a real
/// idle encoder or a silent PCIe link once it reaches a training set.
fn nvml_optional<T, E>(result: Result<T, E>) -> Option<T> {
    result.ok()
}

/// Convert NVML `memory_info` bytes into megabytes, or null both columns on fail.
fn memory_mb<E>(info: Result<(u64, u64), E>) -> (Option<u64>, Option<u64>) {
    match info {
        Ok((used_bytes, total_bytes)) => (
            Some(used_bytes / 1024 / 1024),
            Some(total_bytes / 1024 / 1024),
        ),
        Err(_) => (None, None),
    }
}

fn write_to_parquet(samples: Vec<GpuSample>, batch_id: u32, output_dir: &Path) -> Result<()> {
    let mut df = build_batch_frame(&samples)?;

    let filename = output_dir.join(format!(
        "{}{}{}",
        session::BATCH_PREFIX,
        batch_id,
        session::BATCH_SUFFIX
    ));
    // A failed writer must never publish an incomplete file under the final batch
    // name: restart scanning treats final names as complete batches. The temporary
    // name cannot match `highest_batch_id` and is published only after a complete,
    // flushed write.
    let temporary = output_dir.join(format!(
        ".{}{}{}.{}.tmp",
        session::BATCH_PREFIX,
        batch_id,
        session::BATCH_SUFFIX,
        std::process::id()
    ));
    let mut file = File::create(&temporary)?;
    if let Err(error) = (|| -> Result<()> {
        ParquetWriter::new(&mut file).finish(&mut df)?;
        file.sync_all()?;
        Ok(())
    })() {
        drop(file);
        let _ = remove_file(&temporary);
        return Err(error);
    }
    drop(file);
    if let Err(error) = rename(&temporary, &filename) {
        let _ = remove_file(&temporary);
        return Err(error.into());
    }
    File::open(output_dir)?.sync_all()?;

    println!(
        "Wrote batch {} to {}",
        batch_id,
        privacy::redact_personal_path(&filename.display().to_string())
    );
    Ok(())
}

fn spawn_parquet_write(
    in_flight: &mut JoinSet<Result<()>>,
    samples: Vec<GpuSample>,
    batch_id: u32,
    output_dir: PathBuf,
) {
    in_flight.spawn(async move {
        match tokio::task::spawn_blocking(move || write_to_parquet(samples, batch_id, &output_dir))
            .await
        {
            Ok(result) => result,
            Err(join_err) => Err(anyhow::anyhow!("parquet write task panicked: {}", join_err)),
        }
    });
}

/// Report a failure once, with personal paths stripped.
///
/// Session directories and manifests get shared with downstream pipelines, so
/// `$HOME` is stripped from anything the collector prints.
fn report_failure(context: &str, detail: &str) {
    eprintln!("{context}: {}", privacy::redact_personal_path(detail));
}

fn record_write_result(res: Result<Result<()>, tokio::task::JoinError>, write_failures: &mut u32) {
    match res {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            *write_failures += 1;
            report_failure("Failed to write to Parquet", &format!("{:?}", e));
        }
        Err(e) => {
            // A panicked write loses a batch just as an I/O error does; it must be
            // reported through the same path, not only to stderr.
            *write_failures += 1;
            report_failure("In-flight parquet write task failed", &e.to_string());
        }
    }
}

/// Reap finished writes; if still at capacity, await the next one to finish (backpressure).
/// Returns `true` if Ctrl+C arrived while waiting so the outer loop can shut down.
async fn reclaim_in_flight(in_flight: &mut JoinSet<Result<()>>, write_failures: &mut u32) -> bool {
    while let Some(res) = in_flight.try_join_next() {
        record_write_result(res, write_failures);
    }
    if in_flight.len() < MAX_IN_FLIGHT_WRITES {
        return false;
    }
    tokio::select! {
        res = in_flight.join_next() => {
            if let Some(res) = res {
                record_write_result(res, write_failures);
            }
            false
        }
        _ = tokio::signal::ctrl_c() => true,
    }
}

async fn drain_in_flight(in_flight: &mut JoinSet<Result<()>>, write_failures: &mut u32) {
    while let Some(res) = in_flight.join_next().await {
        record_write_result(res, write_failures);
    }
}

/// Flush the tail buffer, drain outstanding writes, then finalize the manifest.
///
/// Both shutdown paths (Ctrl+C, and Ctrl+C during write backpressure) converge
/// here, so the manifest is finalized and rewritten exactly once per run.
async fn perform_shutdown(
    buffer: Vec<GpuSample>,
    batch_counter: u32,
    output_dir: &Path,
    in_flight: &mut JoinSet<Result<()>>,
    write_failures: &mut u32,
    manifest: &mut SessionManifest,
    timing: &TimingStats,
) -> Result<()> {
    // This is the last instant the collector may have produced telemetry. Storage
    // drain can take much longer and must not inflate the capture's end time.
    let capture_ended_at_utc = Utc::now();
    if !buffer.is_empty() {
        // Losing the tail batch is a write failure, not a reason to skip the drain
        // and manifest finalize below: an unclean run still deserves an accurate
        // record of what it captured.
        match next_batch_id(batch_counter) {
            Ok(new_batch_id) => {
                if let Err(e) = write_to_parquet(buffer, new_batch_id, output_dir) {
                    *write_failures += 1;
                    report_failure("Failed to write final batch", &format!("{:?}", e));
                }
            }
            Err(e) => {
                *write_failures += 1;
                report_failure("Cannot number the final batch", &format!("{:?}", e));
            }
        }
    }
    drain_in_flight(in_flight, write_failures).await;

    // Finalize before any failure bail-out: a run that lost batches still deserves
    // an accurate manifest describing what it acquired and what failed to persist.
    manifest.finalize(capture_ended_at_utc, timing.summary(), *write_failures);
    if let Err(e) = manifest.write_atomic(output_dir) {
        report_failure("Failed to finalize session manifest", &format!("{:?}", e));
        // A missing on-disk finalize (`ended_at_utc: null`) is an unclean exit;
        // do not report graceful success or a zero exit status.
        return Err(e);
    }

    if *write_failures > 0 {
        anyhow::bail!("Graceful shutdown finished with {write_failures} parquet write failure(s)");
    }
    println!("Graceful shutdown complete.");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut session_label = session::resolve_label();

    let nvml = Nvml::init()?;
    let device = nvml.device_by_index(0)?; // Target first GPU

    // Configurable poll interval via environment variable
    let poll_interval_ms =
        session::parse_poll_interval_ms(std::env::var("POLL_INTERVAL_MS").ok().as_deref())?;

    let output_dir = session::resolve_dir()?;
    let _session_lock = session::acquire_exclusive(&output_dir)?;
    let started_at = Utc::now();
    let session_id = session::session_id(
        &session_label,
        &started_at.format("%Y%m%dT%H%M%S%.fZ").to_string(),
    );

    // Do not attach a new manifest to legacy batches that have no manifest to
    // describe their provenance. Operators must migrate or separate that data.
    let mut batch_counter = session::highest_batch_id(&output_dir)?;
    anyhow::ensure!(
        batch_counter == 0 || SessionManifest::load(&output_dir)?.is_some(),
        "SESSION_DIR contains existing telemetry batches but no valid session manifest; use a new directory or restore the manifest"
    );
    anyhow::ensure!(
        batch_counter < u32::MAX,
        "batch ID namespace exhausted; start a new SESSION_DIR"
    );

    let host = HostInfo::new(device.name().ok(), nvml.sys_driver_version().ok());
    let mut manifest = SessionManifest::load_or_new(
        &output_dir,
        session_id,
        session_label.clone(),
        started_at,
        poll_interval_ms,
        host,
    )?;
    // A restarted directory remains one labeled workload/session. Preserve the
    // established identity instead of writing new batches with a conflicting tag.
    session_label = manifest.session_label.clone();
    let mut buffer = Vec::with_capacity(BUFFER_SIZE);
    // Publish only after the fallible resume scan succeeds, so an unreadable
    // existing directory cannot overwrite its prior completed manifest.
    manifest.write_atomic(&output_dir)?;
    let mut cpu_monitor = CpuMonitor::new();
    let mut timing = TimingStats::new(poll_interval_ms);
    let mut in_flight: JoinSet<Result<()>> = JoinSet::new();
    let mut write_failures: u32 = 0;

    println!(
        "Starting GPU telemetry: poll_interval_ms={} session_label={:?} session_id={:?}",
        poll_interval_ms, session_label, manifest.session_id
    );
    println!(
        "Session directory: {}",
        privacy::redact_personal_path(&output_dir.display().to_string())
    );
    if manifest.restart_count > 0 {
        println!(
            "Resuming session (restart #{}) from batch {}",
            manifest.restart_count, batch_counter
        );
    }
    println!("Press Ctrl+C to stop gracefully.");

    // Start the schedule only when telemetry polling begins, so startup work
    // cannot be reported as skipped collection ticks.
    let mut interval = interval(Duration::from_millis(poll_interval_ms));
    // After write backpressure, do not burst-catch every missed 5ms tick.
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            tick = interval.tick() => {
                timing.record_scheduled_tick(tick.into_std());

                let (memory_used_mb, memory_total_mb) =
                    memory_mb(device.memory_info().map(|m| (m.used, m.total)));

                // CPU telemetry (poll for time-delta power calculation)
                let cpu = cpu_monitor.poll();

                let sample = GpuSample {
                    timestamp: Utc::now(),
                    session_label: session_label.clone(),
                    power_usage_mw: nvml_optional(device.power_usage()),
                    temperature_c: nvml_optional(device.temperature(TemperatureSensor::Gpu)),
                    graphics_clock_mhz: nvml_optional(device.clock_info(Clock::Graphics)),
                    memory_clock_mhz: nvml_optional(device.clock_info(Clock::Memory)),
                    pcie_rx_throughput_kbps: nvml_optional(device.pcie_throughput(
                        nvml_wrapper::enum_wrappers::device::PcieUtilCounter::Receive,
                    )),
                    pcie_tx_throughput_kbps: nvml_optional(device.pcie_throughput(
                        nvml_wrapper::enum_wrappers::device::PcieUtilCounter::Send,
                    )),
                    pstate: nvml_optional(device.performance_state()).map(|p| p as u32),
                    throttle_reasons: nvml_optional(device.current_throttle_reasons())
                        .map(|t| t.bits()),
                    fan_speed_perc: nvml_optional(device.fan_speed(0)),
                    memory_used_mb,
                    memory_total_mb,
                    encoder_util_perc: nvml_optional(device.encoder_utilization())
                        .map(|u| u.utilization),
                    decoder_util_perc: nvml_optional(device.decoder_utilization())
                        .map(|u| u.utilization),
                    cpu_tctl_c: cpu.tctl_c,
                    cpu_ccd1_c: cpu.ccd1_c,
                    cpu_ccd2_c: cpu.ccd2_c,
                    cpu_package_power_w: cpu.package_power_w,
                };

                // Measure when telemetry was actually obtained, not the Tokio
                // scheduler's nominal deadline. This includes collection stalls.
                timing.record(std::time::Instant::now());

                buffer.push(sample);

                if buffer.len() >= BUFFER_SIZE {
                    if reclaim_in_flight(&mut in_flight, &mut write_failures).await {
                        println!("\nShutdown signal received during write backpressure...");
                        perform_shutdown(
                            std::mem::take(&mut buffer),
                            batch_counter,
                            &output_dir,
                            &mut in_flight,
                            &mut write_failures,
                            &mut manifest,
                            &timing,
                        ).await?;
                        break;
                    }
                    let samples_to_write =
                        std::mem::replace(&mut buffer, Vec::with_capacity(BUFFER_SIZE));
                    let next_id = match next_batch_id(batch_counter) {
                        Ok(id) => id,
                        Err(e) => {
                            // Returning here would drop the JoinSet, aborting writes
                            // still in flight. Converge on the same shutdown path
                            // Ctrl+C uses so nothing already captured is lost.
                            report_failure("Cannot start a new batch", &format!("{:?}", e));
                            perform_shutdown(
                                samples_to_write,
                                batch_counter,
                                &output_dir,
                                &mut in_flight,
                                &mut write_failures,
                                &mut manifest,
                                &timing,
                            )
                            .await?;
                            break;
                        }
                    };
                    batch_counter = next_id;
                    spawn_parquet_write(
                        &mut in_flight,
                        samples_to_write,
                        batch_counter,
                        output_dir.clone(),
                    );
                }
            }
            _ = tokio::signal::ctrl_c() => {
                println!("\nShutdown signal received. Finalizing last batch...");
                perform_shutdown(
                    buffer,
                    batch_counter,
                    &output_dir,
                    &mut in_flight,
                    &mut write_failures,
                    &mut manifest,
                    &timing,
                ).await?;
                break;
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_fixture(label: &str) -> GpuSample {
        GpuSample {
            timestamp: Utc::now(),
            session_label: label.to_owned(),
            power_usage_mw: Some(120_000),
            temperature_c: Some(65),
            graphics_clock_mhz: Some(2500),
            memory_clock_mhz: Some(10000),
            pcie_rx_throughput_kbps: Some(100),
            pcie_tx_throughput_kbps: Some(50),
            pstate: Some(0),
            throttle_reasons: Some(0),
            fan_speed_perc: Some(40),
            memory_used_mb: Some(8_000),
            memory_total_mb: Some(16_000),
            // Idle encoder/decoder is a real 0, not a miss.
            encoder_util_perc: Some(0),
            decoder_util_perc: Some(0),
            cpu_tctl_c: Some(55.0),
            cpu_ccd1_c: Some(50.0),
            cpu_ccd2_c: Some(51.0),
            cpu_package_power_w: Some(80.0),
        }
    }

    fn unique_fixture_dir(tag: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-fixtures")
            .join(format!(
                "{tag}_{}_{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_and_reload_batch(
        samples: Vec<GpuSample>,
        tag: &str,
        batch_id: u32,
    ) -> (PathBuf, DataFrame) {
        let tmp = unique_fixture_dir(tag);
        write_to_parquet(samples, batch_id, &tmp).expect("parquet write");
        let path = tmp.join(format!(
            "{}{}{}",
            session::BATCH_PREFIX,
            batch_id,
            session::BATCH_SUFFIX
        ));
        let df = LazyFrame::scan_parquet(
            PlRefPath::try_from_path(&path).unwrap(),
            ScanArgsParquet::default(),
        )
        .unwrap()
        .collect()
        .unwrap();
        (tmp, df)
    }

    fn assert_first_u32(df: &DataFrame, column: &str, expected: Option<u32>) {
        let values = df.column(column).unwrap().u32().unwrap();
        assert_eq!(values.get(0), expected, "{column}");
        assert_eq!(
            values.null_count(),
            usize::from(expected.is_none()),
            "{column} null_count"
        );
    }

    fn assert_first_u64(df: &DataFrame, column: &str, expected: Option<u64>) {
        let values = df.column(column).unwrap().u64().unwrap();
        assert_eq!(values.get(0), expected, "{column}");
        assert_eq!(
            values.null_count(),
            usize::from(expected.is_none()),
            "{column} null_count"
        );
    }

    #[test]
    fn write_to_parquet_emits_labeled_batch_file() {
        let base = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-fixtures");
        let tmp = base.join(format!(
            "gt_parquet_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let batch_id = 99_001;
        let path = tmp.join(format!(
            "{}{}{}",
            session::BATCH_PREFIX,
            batch_id,
            session::BATCH_SUFFIX
        ));
        let _ = std::fs::remove_file(&path);
        write_to_parquet(
            vec![sample_fixture("kcd2"), sample_fixture("kcd2")],
            batch_id,
            &tmp,
        )
        .expect("parquet write");
        assert!(path.is_file());
        assert!(
            std::fs::read_dir(&tmp)
                .unwrap()
                .flatten()
                .all(|entry| !entry.file_name().to_string_lossy().ends_with(".tmp")),
            "a successful write publishes only the final batch name"
        );

        // Verify the session_label column is written for every row.
        let df = LazyFrame::scan_parquet(
            PlRefPath::try_from_path(&path).unwrap(),
            ScanArgsParquet::default(),
        )
        .unwrap()
        .select(&[col("session_label")])
        .collect()
        .unwrap();
        let labels = df.column("session_label").unwrap().str().unwrap();
        assert_eq!(labels.len(), 2);
        assert!(labels.iter().all(|opt| opt == Some("kcd2")));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// The batch filename the collector writes must be the one `highest_batch_id`
    /// parses, or restart numbering silently breaks.
    #[test]
    fn written_batch_filename_is_discoverable_by_session_scan() {
        let tmp = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-fixtures")
            .join(format!(
                "gt_batch_scan_{}_{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
        std::fs::create_dir_all(&tmp).unwrap();

        write_to_parquet(vec![sample_fixture("kcd2")], 4, &tmp).expect("parquet write");
        assert_eq!(session::highest_batch_id(&tmp).unwrap(), 4);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn record_write_result_counts_io_failures() {
        let mut fails = 0u32;
        record_write_result(Ok(Ok(())), &mut fails);
        assert_eq!(fails, 0);
        record_write_result(Ok(Err(anyhow::anyhow!("disk full"))), &mut fails);
        assert_eq!(fails, 1);
        // JoinError is hard to construct without panicking a task; skip Err arm here.
    }

    /// The batch schema is a contract with `export_csv`, `query` and the
    /// downstream SNN pipeline: a dropped or reordered column would corrupt
    /// training data without failing anything.
    #[test]
    fn batch_frame_emits_the_canonical_column_set_in_order() {
        let df = build_batch_frame(&[sample_fixture("re4r")]).expect("frame");
        let names: Vec<&str> = df
            .get_column_names()
            .iter()
            .map(|name| name.as_str())
            .collect();
        assert_eq!(
            names,
            vec![
                "timestamp_ms",
                "session_label",
                "power_usage_mw",
                "temperature_c",
                "graphics_clock_mhz",
                "memory_clock_mhz",
                "pcie_rx_kbps",
                "pcie_tx_kbps",
                "pstate",
                "throttle_reasons_bitmask",
                "fan_speed_perc",
                "memory_used_mb",
                "memory_total_mb",
                "encoder_util_perc",
                "decoder_util_perc",
                "cpu_tctl_c",
                "cpu_ccd1_c",
                "cpu_ccd2_c",
                "cpu_package_power_w",
            ]
        );
    }

    /// One column per width, so a transposition between same-typed fields shows up.
    #[test]
    fn batch_frame_preserves_sample_values() {
        let fixture = sample_fixture("re4r");
        let df = build_batch_frame(std::slice::from_ref(&fixture)).expect("frame");
        assert_eq!(df.height(), 1);
        assert_eq!(
            df.column("power_usage_mw").unwrap().u32().unwrap().get(0),
            fixture.power_usage_mw
        );
        assert_eq!(
            df.column("memory_used_mb").unwrap().u64().unwrap().get(0),
            fixture.memory_used_mb
        );
        assert_eq!(
            df.column("encoder_util_perc")
                .unwrap()
                .u32()
                .unwrap()
                .get(0),
            Some(0),
            "idle encoder util must remain 0, not become null"
        );
        assert_eq!(
            df.column("cpu_package_power_w")
                .unwrap()
                .f32()
                .unwrap()
                .get(0),
            fixture.cpu_package_power_w
        );
        assert_eq!(
            df.column("session_label").unwrap().str().unwrap().get(0),
            Some("re4r")
        );
    }

    /// The whole point of the `Option` columns: an unreadable sensor must reach
    /// Parquet as a null, never as a plausible 0.0 that a model would learn from.
    #[test]
    fn unavailable_cpu_sensors_round_trip_as_nulls_not_zeros() {
        let mut sample = sample_fixture("kcd2");
        sample.cpu_tctl_c = None;
        sample.cpu_ccd1_c = None;
        sample.cpu_ccd2_c = None;
        sample.cpu_package_power_w = None;

        // Go through the real storage boundary: an in-memory frame cannot catch a
        // null-encoding regression in the Parquet writer.
        let tmp = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-fixtures")
            .join(format!(
                "gt_null_cpu_{}_{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
        std::fs::create_dir_all(&tmp).unwrap();
        let batch_id = 7;
        write_to_parquet(vec![sample], batch_id, &tmp).expect("parquet write");

        let path = tmp.join(format!(
            "{}{}{}",
            session::BATCH_PREFIX,
            batch_id,
            session::BATCH_SUFFIX
        ));
        let df = LazyFrame::scan_parquet(
            PlRefPath::try_from_path(&path).unwrap(),
            ScanArgsParquet::default(),
        )
        .unwrap()
        .collect()
        .unwrap();

        for column in [
            "cpu_tctl_c",
            "cpu_ccd1_c",
            "cpu_ccd2_c",
            "cpu_package_power_w",
        ] {
            let values = df.column(column).unwrap().f32().unwrap();
            assert_eq!(values.get(0), None, "{column} must be null, not 0.0");
            assert_eq!(values.null_count(), 1, "{column} must record a null");
        }

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn nvml_miss_is_none_but_observed_zero_is_kept() {
        let miss: Result<u32, &str> = Err("nvml unavailable");
        assert_eq!(nvml_optional(miss), None);
        let idle: Result<u32, &str> = Ok(0);
        assert_eq!(nvml_optional(idle), Some(0));
        let pstate_p0: Result<u32, &str> = Ok(0);
        assert_eq!(
            nvml_optional(pstate_p0),
            Some(0),
            "P0 is a real performance state, not a miss"
        );
    }

    #[test]
    fn memory_info_fail_is_null_but_used_zero_is_kept() {
        assert_eq!(memory_mb(Err::<(u64, u64), _>(())), (None, None));
        assert_eq!(
            memory_mb(Ok::<_, ()>((0, 16 * 1024 * 1024))),
            (Some(0), Some(16)),
            "idle VRAM used is a real 0; total still records capacity"
        );
    }

    fn nvml_all_miss(mut sample: GpuSample) -> GpuSample {
        sample.power_usage_mw = None;
        sample.temperature_c = None;
        sample.graphics_clock_mhz = None;
        sample.memory_clock_mhz = None;
        sample.pcie_rx_throughput_kbps = None;
        sample.pcie_tx_throughput_kbps = None;
        sample.pstate = None;
        sample.throttle_reasons = None;
        sample.fan_speed_perc = None;
        sample.memory_used_mb = None;
        sample.memory_total_mb = None;
        sample.encoder_util_perc = None;
        sample.decoder_util_perc = None;
        sample
    }

    /// NVML miss path: every GPU sensor column must reach Parquet as a null,
    /// never as a fabricated 0 that ETL #21 refuses on UNAVAILABLE_ZERO_FIELDS
    /// (or that a model would treat as idle PCIe / no-throttle / P0).
    #[test]
    fn unavailable_nvml_sensors_round_trip_as_nulls_not_zeros() {
        let (tmp, df) = write_and_reload_batch(
            vec![nvml_all_miss(sample_fixture("kcd2"))],
            "gt_null_nvml",
            8,
        );
        for column in [
            "power_usage_mw",
            "temperature_c",
            "graphics_clock_mhz",
            "memory_clock_mhz",
            "pcie_rx_kbps",
            "pcie_tx_kbps",
            "pstate",
            "fan_speed_perc",
            "encoder_util_perc",
            "decoder_util_perc",
        ] {
            assert_first_u32(&df, column, None);
        }
        for column in [
            "throttle_reasons_bitmask",
            "memory_used_mb",
            "memory_total_mb",
        ] {
            assert_first_u64(&df, column, None);
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// A successful NVML read of 0 (idle encoder, idle PCIe, no throttle) must
    /// remain 0 through Parquet — null is only for a failed call.
    #[test]
    fn observed_nvml_zeros_round_trip_as_zero_not_null() {
        let mut sample = sample_fixture("re4r");
        sample.pcie_rx_throughput_kbps = Some(0);
        sample.pcie_tx_throughput_kbps = Some(0);
        sample.pstate = Some(0);
        sample.throttle_reasons = Some(0);
        sample.fan_speed_perc = Some(0);
        sample.encoder_util_perc = Some(0);
        sample.decoder_util_perc = Some(0);
        sample.memory_used_mb = Some(0);

        let (tmp, df) = write_and_reload_batch(vec![sample], "gt_zero_nvml", 9);
        for column in [
            "pcie_rx_kbps",
            "pcie_tx_kbps",
            "pstate",
            "fan_speed_perc",
            "encoder_util_perc",
            "decoder_util_perc",
        ] {
            assert_first_u32(&df, column, Some(0));
        }
        assert_first_u64(&df, "throttle_reasons_bitmask", Some(0));
        assert_first_u64(&df, "memory_used_mb", Some(0));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// `export_csv` divides power and aliases temp; a missing NVML read must
    /// stay empty in that 5-column projection, not become 0.0 W / 0 C.
    #[test]
    fn export_csv_projection_preserves_null_gpu_sensors() {
        let mut sample = sample_fixture("kcd2");
        sample.power_usage_mw = None;
        sample.temperature_c = None;

        let df = build_batch_frame(&[sample]).expect("frame");
        let exported = df
            .lazy()
            .select([
                col("timestamp_ms"),
                col("temperature_c").alias("gpu_temp_c"),
                (col("power_usage_mw") / lit(1000.0)).alias("gpu_power_w"),
                col("cpu_tctl_c"),
                col("cpu_package_power_w"),
            ])
            .collect()
            .unwrap();

        assert_eq!(
            exported.column("gpu_temp_c").unwrap().u32().unwrap().get(0),
            None
        );
        let power = exported.column("gpu_power_w").unwrap();
        assert!(
            power.get(0).unwrap().is_null(),
            "missing GPU power must stay null after /1000, not 0.0"
        );
        assert_eq!(
            exported.column("cpu_tctl_c").unwrap().f32().unwrap().get(0),
            Some(55.0),
            "CPU columns must be unchanged by the GPU null projection"
        );
    }

    #[test]
    fn next_batch_id_rejects_exhausted_namespace() {
        assert_eq!(next_batch_id(5).unwrap(), 6);
        assert!(next_batch_id(u32::MAX).is_err());
    }
}
