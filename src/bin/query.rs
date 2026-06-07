use anyhow::{Context, Result};
use duckdb::Connection;
use std::env;

#[path = "../privacy.rs"]
mod privacy;
use privacy::redact_personal_path;

fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        println!("Usage: cargo run --bin query <parquet_file>");
        return Ok(());
    }
    let parquet_file = &args[1];
    let parquet_file_redacted = redact_personal_path(parquet_file).replace("'", "''");

    let conn = Connection::open_in_memory()?;

    println!("--- Analyzing {} with DuckDB ---", parquet_file_redacted);

    // Basic stats
    println!("\n[Summary Statistics]");
    let mut stmt = conn.prepare(&format!(
        "SELECT
            avg(power_usage_mw) as avg_power,
            max(power_usage_mw) as max_power,
            avg(temperature_c) as avg_temp,
            max(pcie_rx_kbps) as max_pcie_rx,
            max(pcie_tx_kbps) as max_pcie_tx,
            avg(encoder_util_perc) as avg_enc,
            avg(decoder_util_perc) as avg_dec,
            sum(CASE WHEN mangohud_active THEN 1 ELSE 0 END) * 100.0 / count(*) as mangohud_presence_pct,
            count(*) as sample_count,
            avg(cpu_tctl_c) as avg_cpu_temp,
            max(cpu_tctl_c) as max_cpu_temp,
            avg(cpu_ccd1_c) as avg_cpu_ccd1,
            max(cpu_ccd1_c) as max_cpu_ccd1,
            avg(cpu_ccd2_c) as avg_cpu_ccd2,
            max(cpu_ccd2_c) as max_cpu_ccd2
         FROM read_parquet('{}')",
        parquet_file_redacted
    )).with_context(|| format!("Failed to prepare summary statistics query for {}", parquet_file_redacted))?;

    let mut rows = stmt.query([]).with_context(|| format!("Failed to execute summary statistics query for {}", parquet_file_redacted))?;
    if let Some(row) = rows.next()? {
        let avg_power: f64 = row.get(0)?;
        let max_power: u32 = row.get(1)?;
        let avg_temp: f64 = row.get(2)?;
        let max_rx: u32 = row.get(3)?;
        let max_tx: u32 = row.get(4)?;
        let avg_enc: f64 = row.get(5)?;
        let avg_dec: f64 = row.get(6)?;
        let mangohud_pct: f64 = row.get(7)?;
        let count: i64 = row.get(8)?;
        let avg_cpu_temp: f64 = row.get(9)?;
        let max_cpu_temp: f64 = row.get(10)?;
        let avg_cpu_ccd1: f64 = row.get(11)?;
        let max_cpu_ccd1: f64 = row.get(12)?;
        let avg_cpu_ccd2: f64 = row.get(13)?;
        let max_cpu_ccd2: f64 = row.get(14)?;

        println!("Samples: {}", count);
        println!("Avg Power: {:.2} W", avg_power / 1000.0);
        println!("Max Power: {:.2} W", max_power as f64 / 1000.0);
        println!("Avg Temp:  {:.1} C", avg_temp);
        println!("Max PCIe RX: {:.2} MB/s", max_rx as f64 / 1024.0);
        println!("Max PCIe TX: {:.2} MB/s", max_tx as f64 / 1024.0);
        println!("Avg Encoder: {:.1}%", avg_enc);
        println!("Avg Decoder: {:.1}%", avg_dec);
        println!("MangoHud Active: {:.1}% of samples", mangohud_pct);
        println!("\n--- CPU Telemetry ---");
        println!("Avg CPU Temp (Tctl): {:.1} C", avg_cpu_temp);
        println!("Max CPU Temp (Tctl): {:.1} C", max_cpu_temp);
        println!("Avg CCD1 Temp: {:.1} C", avg_cpu_ccd1);
        println!("Max CCD1 Temp: {:.1} C", max_cpu_ccd1);
        println!("Avg CCD2 Temp: {:.1} C", avg_cpu_ccd2);
        println!("Max CCD2 Temp: {:.1} C", max_cpu_ccd2);
    }

    // Detecting "Inhibitory" Signals (Throttling)
    println!("\n[Throttling / Inhibitory Signals]");
    let mut stmt = conn.prepare(&format!(
        "SELECT timestamp_ms, throttle_reasons_bitmask
         FROM read_parquet('{}')
         WHERE throttle_reasons_bitmask != 0
         LIMIT 5",
        parquet_file_redacted
    )).with_context(|| format!("Failed to prepare throttling query for {}", parquet_file_redacted))?;

    let mut rows = stmt.query([]).with_context(|| format!("Failed to execute throttling query for {}", parquet_file_redacted))?;
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
    let mut stmt = conn.prepare(&format!(
        "SELECT timestamp_ms, pcie_rx_kbps, power_usage_mw
         FROM read_parquet('{}')
         ORDER BY pcie_rx_kbps DESC
         LIMIT 5",
        parquet_file_redacted
    )).with_context(|| format!("Failed to prepare PCIe spikes query for {}", parquet_file_redacted))?;

    let mut rows = stmt.query([]).with_context(|| format!("Failed to execute PCIe spikes query for {}", parquet_file_redacted))?;
    while let Some(row) = rows.next()? {
        let ts: i64 = row.get(0)?;
        let rx: u32 = row.get(1)?;
        let pwr: u32 = row.get(2)?;
        println!("TS: {} | PCIe RX: {:6} KB/s | Power: {:5} mW", ts, rx, pwr);
    }

    // CPU Temperature Spikes
    println!("\n[CPU Temperature Spikes (Tctl > 80C)]");
    let mut stmt = conn.prepare(&format!(
        "SELECT timestamp_ms, cpu_tctl_c, cpu_ccd1_c, cpu_ccd2_c, power_usage_mw
         FROM read_parquet('{}')
         WHERE cpu_tctl_c > 80.0
         ORDER BY cpu_tctl_c DESC
         LIMIT 5",
        parquet_file_redacted
    )).with_context(|| format!("Failed to prepare CPU temperature spikes query for {}", parquet_file_redacted))?;

    let mut rows = stmt.query([]).with_context(|| format!("Failed to execute CPU temperature spikes query for {}", parquet_file_redacted))?;
    let mut found = false;
    while let Some(row) = rows.next()? {
        found = true;
        let ts: i64 = row.get(0)?;
        let tctl: f32 = row.get(1)?;
        let ccd1: f32 = row.get(2)?;
        let ccd2: f32 = row.get(3)?;
        let pwr: u32 = row.get(4)?;
        println!(
            "TS: {} | Tctl: {:5.1} C | CCD1: {:5.1} C | CCD2: {:5.1} C | Power: {:5} mW",
            ts, tctl, ccd1, ccd2, pwr
        );
    }
    if !found {
        println!("No CPU thermal spikes detected.");
    }

    Ok(())
}
