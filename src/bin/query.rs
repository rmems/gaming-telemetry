// SPDX-License-Identifier: MIT OR Apache-2.0

use anyhow::{Context, Result};
use duckdb::Connection;
use gaming_telemetry::privacy::redact_personal_path;
use std::env;

/// Render an optional temperature at a fixed width so the columns stay aligned
/// whether or not the sensor was available.
fn format_celsius(value: Option<f32>) -> String {
    match value {
        Some(value) => format!("{value:5.1} C"),
        None => "  n/a  ".to_owned(),
    }
}

fn render_optional_stat(
    label: &str,
    value: Option<f64>,
    formatted: impl FnOnce(f64) -> String,
) -> String {
    match value {
        Some(value) => format!("{label}: {}", formatted(value)),
        None => format!("{label}: unavailable (sensor not readable during capture)"),
    }
}

fn print_optional_stat(label: &str, value: Option<f64>, formatted: impl FnOnce(f64) -> String) {
    println!("{}", render_optional_stat(label, value, formatted));
}

fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        println!("Usage: cargo run --bin query <parquet_file>");
        return Ok(());
    }
    let parquet_file = &args[1];
    let parquet_file_display = redact_personal_path(parquet_file);
    let parquet_file_sql = parquet_file.replace("'", "''");

    let conn = Connection::open_in_memory()?;

    println!("--- Analyzing {} with DuckDB ---", parquet_file_display);

    // Basic stats
    println!("\n[Summary Statistics]");
    let mut stmt = conn
        .prepare(&format!(
            "SELECT
            avg(power_usage_mw) as avg_power,
            max(power_usage_mw) as max_power,
            avg(temperature_c) as avg_temp,
            max(pcie_rx_kbps) as max_pcie_rx,
            max(pcie_tx_kbps) as max_pcie_tx,
            avg(encoder_util_perc) as avg_enc,
            avg(decoder_util_perc) as avg_dec,
            count(*) as sample_count,
            avg(cpu_tctl_c) as avg_cpu_temp,
            max(cpu_tctl_c) as max_cpu_temp,
            avg(cpu_ccd1_c) as avg_cpu_ccd1,
            max(cpu_ccd1_c) as max_cpu_ccd1,
            avg(cpu_ccd2_c) as avg_cpu_ccd2,
            max(cpu_ccd2_c) as max_cpu_ccd2
         FROM read_parquet('{}')",
            parquet_file_sql
        ))
        .with_context(|| {
            format!(
                "Failed to prepare summary statistics query for {}",
                parquet_file_display
            )
        })?;

    let mut rows = stmt.query([]).with_context(|| {
        format!(
            "Failed to execute summary statistics query for {}",
            parquet_file_display
        )
    })?;
    if let Some(row) = rows.next()? {
        let avg_power: Option<f64> = row.get(0)?;
        let max_power: Option<u32> = row.get(1)?;
        let avg_temp: Option<f64> = row.get(2)?;
        let max_rx: Option<u32> = row.get(3)?;
        let max_tx: Option<u32> = row.get(4)?;
        let avg_enc: Option<f64> = row.get(5)?;
        let avg_dec: Option<f64> = row.get(6)?;
        let count: i64 = row.get(7)?;
        let avg_cpu_temp: Option<f64> = row.get(8)?;
        let max_cpu_temp: Option<f64> = row.get(9)?;
        let avg_cpu_ccd1: Option<f64> = row.get(10)?;
        let max_cpu_ccd1: Option<f64> = row.get(11)?;
        let avg_cpu_ccd2: Option<f64> = row.get(12)?;
        let max_cpu_ccd2: Option<f64> = row.get(13)?;

        println!("Samples: {}", count);
        print_optional_stat("Avg Power", avg_power, |mw| format!("{:.2} W", mw / 1000.0));
        print_optional_stat("Max Power", max_power.map(f64::from), |mw| {
            format!("{:.2} W", mw / 1000.0)
        });
        print_optional_stat("Avg Temp", avg_temp, |c| format!("{c:.1} C"));
        print_optional_stat("Max PCIe RX", max_rx.map(f64::from), |kbps| {
            format!("{:.2} MB/s", kbps / 1024.0)
        });
        print_optional_stat("Max PCIe TX", max_tx.map(f64::from), |kbps| {
            format!("{:.2} MB/s", kbps / 1024.0)
        });
        print_optional_stat("Avg Encoder", avg_enc, |pct| format!("{pct:.1}%"));
        print_optional_stat("Avg Decoder", avg_dec, |pct| format!("{pct:.1}%"));
        println!("\n--- CPU Telemetry ---");
        // A CPU column is null for every row when the sensor was unavailable, so
        // these aggregates are themselves NULL. Report that, rather than failing
        // the whole query or printing a fabricated 0.0.
        for (label, value) in [
            ("Avg CPU Temp (Tctl)", avg_cpu_temp),
            ("Max CPU Temp (Tctl)", max_cpu_temp),
            ("Avg CCD1 Temp", avg_cpu_ccd1),
            ("Max CCD1 Temp", max_cpu_ccd1),
            ("Avg CCD2 Temp", avg_cpu_ccd2),
            ("Max CCD2 Temp", max_cpu_ccd2),
        ] {
            match value {
                Some(value) => println!("{label}: {value:.1} C"),
                None => println!("{label}: unavailable (sensor not readable during capture)"),
            }
        }
    }

    // Detecting "Inhibitory" Signals (Throttling)
    println!("\n[Throttling / Inhibitory Signals]");
    let mut stmt = conn
        .prepare(&format!(
            "SELECT timestamp_ms, throttle_reasons_bitmask
         FROM read_parquet('{}')
         WHERE throttle_reasons_bitmask IS NOT NULL AND throttle_reasons_bitmask != 0
         LIMIT 5",
            parquet_file_sql
        ))
        .with_context(|| {
            format!(
                "Failed to prepare throttling query for {}",
                parquet_file_display
            )
        })?;

    let mut rows = stmt.query([]).with_context(|| {
        format!(
            "Failed to execute throttling query for {}",
            parquet_file_display
        )
    })?;
    let mut found = false;
    while let Some(row) = rows.next()? {
        found = true;
        let ts: i64 = row.get(0)?;
        let mask: u64 = row.get(1)?;
        println!("TS: {} | Throttle Mask: {:016b}", ts, mask);
    }
    if !found {
        println!("No throttling events found in this batch.");
    }

    // Spikes (Excitatory)
    println!("\n[Potential PCIe Data Spikes]");
    let mut stmt = conn
        .prepare(&format!(
            "SELECT timestamp_ms, pcie_rx_kbps, power_usage_mw
         FROM read_parquet('{}')
         ORDER BY pcie_rx_kbps DESC NULLS LAST
         LIMIT 5",
            parquet_file_sql
        ))
        .with_context(|| {
            format!(
                "Failed to prepare PCIe spikes query for {}",
                parquet_file_display
            )
        })?;

    let mut rows = stmt.query([]).with_context(|| {
        format!(
            "Failed to execute PCIe spikes query for {}",
            parquet_file_display
        )
    })?;
    while let Some(row) = rows.next()? {
        let ts: i64 = row.get(0)?;
        let rx: Option<u32> = row.get(1)?;
        let pwr: Option<u32> = row.get(2)?;
        println!(
            "TS: {} | PCIe RX: {:>6} KB/s | Power: {:>5} mW",
            ts,
            rx.map(|v| v.to_string())
                .unwrap_or_else(|| "n/a".to_owned()),
            pwr.map(|v| v.to_string())
                .unwrap_or_else(|| "n/a".to_owned()),
        );
    }

    // CPU Temperature Spikes
    println!("\n[CPU Temperature Spikes (Tctl > 80C)]");
    let mut stmt = conn
        .prepare(&format!(
            "SELECT timestamp_ms, cpu_tctl_c, cpu_ccd1_c, cpu_ccd2_c, power_usage_mw
         FROM read_parquet('{}')
         WHERE cpu_tctl_c > 80.0
         ORDER BY cpu_tctl_c DESC
         LIMIT 5",
            parquet_file_sql
        ))
        .with_context(|| {
            format!(
                "Failed to prepare CPU temperature spikes query for {}",
                parquet_file_display
            )
        })?;

    let mut rows = stmt.query([]).with_context(|| {
        format!(
            "Failed to execute CPU temperature spikes query for {}",
            parquet_file_display
        )
    })?;
    let mut found = false;
    while let Some(row) = rows.next()? {
        found = true;
        let ts: i64 = row.get(0)?;
        // `cpu_tctl_c` cannot be NULL here: the `> 80.0` filter excludes NULL rows.
        // The CCD sensors are unfiltered and absent on single-CCD parts, so reading
        // them as `f32` aborts the whole query on the first spike.
        let tctl: f32 = row.get(1)?;
        let ccd1: Option<f32> = row.get(2)?;
        let ccd2: Option<f32> = row.get(3)?;
        let pwr: Option<u32> = row.get(4)?;
        println!(
            "TS: {} | Tctl: {:5.1} C | CCD1: {} | CCD2: {} | Power: {:>5} mW",
            ts,
            tctl,
            format_celsius(ccd1),
            format_celsius(ccd2),
            pwr.map(|v| v.to_string())
                .unwrap_or_else(|| "n/a".to_owned()),
        );
    }
    if !found {
        println!("No CPU thermal spikes detected.");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_celsius_renders_available_and_missing_values() {
        assert_eq!(format_celsius(Some(81.25)), " 81.2 C");
        assert_eq!(format_celsius(None), "  n/a  ");
    }

    #[test]
    fn render_optional_stat_distinguishes_unavailable_from_zero() {
        assert_eq!(
            render_optional_stat("Avg Power", Some(0.0), |mw| format!("{mw:.2} W")),
            "Avg Power: 0.00 W"
        );
        assert_eq!(
            render_optional_stat("Avg Power", None, |_| "unused".to_owned()),
            "Avg Power: unavailable (sensor not readable during capture)"
        );
    }
}
