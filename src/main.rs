mod cpu;
mod privacy;

use anyhow::Result;
use chrono::{DateTime, Utc};
use cpu::CpuMonitor;
use nvml_wrapper::Nvml;
use nvml_wrapper::enum_wrappers::device::{Clock, TemperatureSensor};
use polars::prelude::*;
use sentry::ClientInitGuard;
use std::borrow::Cow;
use std::fs::File;
use std::process::Command;
use std::sync::Arc;
use tokio::time::{Duration, interval};

fn git_sha() -> String {
    if let Some(value) = std::env::var("AGENTOS_GIT_SHA")
        .ok()
        .filter(|value| !value.trim().is_empty())
    {
        return value;
    }
    if let Ok(output) = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        && output.status.success()
    {
        let sha = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if !sha.is_empty() {
            return sha;
        }
    }
    "unknown".to_owned()
}

fn resolve_sentry_release(git_sha: &str) -> String {
    if let Some(release) = std::env::var("SENTRY_RELEASE")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
    {
        return release;
    }

    if !git_sha.trim().is_empty() && git_sha != "unknown" {
        return format!("gaming-telemetry@{git_sha}");
    }

    if let Some(release) = sentry::release_name!() {
        let release = release.into_owned();
        if !release.trim().is_empty() {
            return release;
        }
    }

    "gaming-telemetry@unknown".to_owned()
}

fn init_sentry() -> Option<ClientInitGuard> {
    let dsn = std::env::var("SENTRY_DSN")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())?;
    let git_sha = git_sha();
    let release = resolve_sentry_release(&git_sha);
    let environment = std::env::var("SENTRY_ENVIRONMENT")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "local".to_owned());
    let parsed_dsn = match dsn.parse() {
        Ok(dsn) => dsn,
        Err(error) => {
            eprintln!("Sentry disabled: invalid SENTRY_DSN ({error})");
            return None;
        }
    };

    let guard = sentry::init(sentry::ClientOptions {
        dsn: Some(parsed_dsn),
        release: Some(Cow::Owned(release)),
        environment: Some(Cow::Owned(environment)),
        sample_rate: 1.0,
        traces_sample_rate: 0.0,
        default_integrations: true,
        ..Default::default()
    });

    Some(guard)
}

#[derive(Debug, Clone)]
struct GpuSample {
    timestamp: DateTime<Utc>,
    power_usage_mw: u32,
    temperature_c: u32,
    graphics_clock_mhz: u32,
    memory_clock_mhz: u32,
    pcie_rx_throughput_kbps: u32,
    pcie_tx_throughput_kbps: u32,
    pstate: u32,
    throttle_reasons: u64,
    fan_speed_perc: u32,
    memory_used_mb: u64,
    memory_total_mb: u64,
    encoder_util_perc: u32,
    decoder_util_perc: u32,
    mangohud_active: bool,
    // CPU telemetry
    cpu_tctl_c: f32,
    cpu_ccd1_c: f32,
    cpu_ccd2_c: f32,
    cpu_package_power_w: f32,
}

const BUFFER_SIZE: usize = 2000; // ~10 seconds of data at default 5ms intervals

async fn write_to_parquet(samples: Vec<GpuSample>, batch_id: u32) -> Result<()> {
    let timestamps: Vec<i64> = samples
        .iter()
        .map(|s| s.timestamp.timestamp_millis())
        .collect();
    let power: Vec<u32> = samples.iter().map(|s| s.power_usage_mw).collect();
    let temp: Vec<u32> = samples.iter().map(|s| s.temperature_c).collect();
    let graphics_clock: Vec<u32> = samples.iter().map(|s| s.graphics_clock_mhz).collect();
    let memory_clock: Vec<u32> = samples.iter().map(|s| s.memory_clock_mhz).collect();
    let pcie_rx: Vec<u32> = samples.iter().map(|s| s.pcie_rx_throughput_kbps).collect();
    let pcie_tx: Vec<u32> = samples.iter().map(|s| s.pcie_tx_throughput_kbps).collect();
    let pstate: Vec<u32> = samples.iter().map(|s| s.pstate).collect();
    let throttle: Vec<u64> = samples.iter().map(|s| s.throttle_reasons).collect();
    let fan: Vec<u32> = samples.iter().map(|s| s.fan_speed_perc).collect();
    let mem_used: Vec<u64> = samples.iter().map(|s| s.memory_used_mb).collect();
    let mem_total: Vec<u64> = samples.iter().map(|s| s.memory_total_mb).collect();
    let enc_util: Vec<u32> = samples.iter().map(|s| s.encoder_util_perc).collect();
    let dec_util: Vec<u32> = samples.iter().map(|s| s.decoder_util_perc).collect();
    let mangohud: Vec<bool> = samples.iter().map(|s| s.mangohud_active).collect();
    let cpu_tctl: Vec<f32> = samples.iter().map(|s| s.cpu_tctl_c).collect();
    let cpu_ccd1: Vec<f32> = samples.iter().map(|s| s.cpu_ccd1_c).collect();
    let cpu_ccd2: Vec<f32> = samples.iter().map(|s| s.cpu_ccd2_c).collect();
    let cpu_power: Vec<f32> = samples.iter().map(|s| s.cpu_package_power_w).collect();

    let mut df = df!(
        "timestamp_ms" => timestamps,
        "power_usage_mw" => power,
        "temperature_c" => temp,
        "graphics_clock_mhz" => graphics_clock,
        "memory_clock_mhz" => memory_clock,
        "pcie_rx_kbps" => pcie_rx,
        "pcie_tx_kbps" => pcie_tx,
        "pstate" => pstate,
        "throttle_reasons_bitmask" => throttle,
        "fan_speed_perc" => fan,
        "memory_used_mb" => mem_used,
        "memory_total_mb" => mem_total,
        "encoder_util_perc" => enc_util,
        "decoder_util_perc" => dec_util,
        "mangohud_active" => mangohud,
        "cpu_tctl_c" => cpu_tctl,
        "cpu_ccd1_c" => cpu_ccd1,
        "cpu_ccd2_c" => cpu_ccd2,
        "cpu_package_power_w" => cpu_power,
    )?;

    let filename = format!("gpu_telemetry_v1_batch_{}.parquet", batch_id);
    let file = File::create(&filename)?;
    ParquetWriter::new(file).finish(&mut df)?;

    println!("Wrote batch {} to {}", batch_id, filename);
    Ok(())
}

fn is_mangohud_running() -> bool {
    Command::new("pgrep")
        .arg("-x")
        .arg("mangohud")
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[tokio::main]
async fn main() -> Result<()> {
    let _sentry_guard = init_sentry();

    let nvml = Arc::new(Nvml::init()?);
    let device = nvml.device_by_index(0)?; // Target first GPU

    // Configurable poll interval via environment variable
    let poll_interval_ms = std::env::var("POLL_INTERVAL_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5);

    let mut buffer = Vec::with_capacity(BUFFER_SIZE);
    let mut interval = interval(Duration::from_millis(poll_interval_ms));
    let mut batch_counter = 0;
    let mut cpu_monitor = CpuMonitor::new();

    println!(
        "Starting enhanced GPU telemetry polling every {}ms...",
        poll_interval_ms
    );
    println!("Press Ctrl+C to stop gracefully.");

    loop {
        tokio::select! {
            _ = interval.tick() => {
                let power_usage = device.power_usage().unwrap_or(0);
                let temperature = device.temperature(TemperatureSensor::Gpu).unwrap_or(0);
                let graphics_clock = device.clock_info(Clock::Graphics).unwrap_or(0);
                let memory_clock = device.clock_info(Clock::Memory).unwrap_or(0);

                let pcie_rx = device.pcie_throughput(nvml_wrapper::enum_wrappers::device::PcieUtilCounter::Receive).unwrap_or(0);
                let pcie_tx = device.pcie_throughput(nvml_wrapper::enum_wrappers::device::PcieUtilCounter::Send).unwrap_or(0);
                let pstate = device.performance_state().map(|p| p as u32).unwrap_or(0);
                let throttle = device.current_throttle_reasons().map(|t| t.bits()).unwrap_or(0);
                let fan = device.fan_speed(0).unwrap_or(0);
                let mem_info = device.memory_info();

                // Encoder/Decoder utilization
                let encoder_util = device.encoder_utilization().map(|u| u.utilization).unwrap_or(0);
                let decoder_util = device.decoder_utilization().map(|u| u.utilization).unwrap_or(0);

                // MangoHud integration
                let mangohud_active = is_mangohud_running();

                // CPU telemetry (poll for time-delta power calculation)
                let (cpu_tctl_c, cpu_package_power_w) = cpu_monitor.poll();
                let cpu_ccd1_c = cpu_monitor.read_ccd1();
                let cpu_ccd2_c = cpu_monitor.read_ccd2();

                let sample = GpuSample {
                    timestamp: Utc::now(),
                    power_usage_mw: power_usage,
                    temperature_c: temperature,
                    graphics_clock_mhz: graphics_clock,
                    memory_clock_mhz: memory_clock,
                    pcie_rx_throughput_kbps: pcie_rx,
                    pcie_tx_throughput_kbps: pcie_tx,
                    pstate,
                    throttle_reasons: throttle,
                    fan_speed_perc: fan,
                    memory_used_mb: mem_info.as_ref().map(|m| m.used / 1024 / 1024).unwrap_or(0),
                    memory_total_mb: mem_info.as_ref().map(|m| m.total / 1024 / 1024).unwrap_or(0),
                    encoder_util_perc: encoder_util,
                    decoder_util_perc: decoder_util,
                    mangohud_active,
                    cpu_tctl_c,
                    cpu_ccd1_c,
                    cpu_ccd2_c,
                    cpu_package_power_w,
                };

                buffer.push(sample);

                if buffer.len() >= BUFFER_SIZE {
                    let samples_to_write = std::mem::replace(&mut buffer, Vec::with_capacity(BUFFER_SIZE));
                    batch_counter += 1;

                    // Write asynchronously to avoid blocking the polling loop
                    tokio::spawn(async move {
                        if let Err(e) = write_to_parquet(samples_to_write, batch_counter).await {
                            eprintln!("Failed to write to Parquet: {:?}", e);
                            sentry::capture_error(&*e);
                        }
                    });
                }
            }
            _ = tokio::signal::ctrl_c() => {
                println!("\nShutdown signal received. Finalizing last batch...");
                if !buffer.is_empty() {
                    batch_counter += 1;
                    if let Err(e) = write_to_parquet(buffer, batch_counter).await {
                        eprintln!("Failed to write final batch: {:?}", e);
                        sentry::capture_error(&*e);
                    }
                }
                println!("Graceful shutdown complete.");
                break;
            }
        }
    }

    Ok(())
}
