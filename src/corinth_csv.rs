// SPDX-License-Identifier: MIT OR Apache-2.0

//! The corinth-ingest CSV artifact: the frozen five-column hardware-telemetry
//! interchange contract hosted by `rmems/vahtisiru`.
//!
//! Contract source (one-way copy, **no** Cargo dependency in either direction):
//! `rmems/vahtisiru` `docs/telemetry_csv.md` and `src/telemetry_csv.rs`, at
//! [`CONTRACT_REVISION`]. Upstream publishes no separate schema-version string;
//! the schema is identified by its frozen [`HEADER`] and pinned here by the
//! upstream commit. If the upstream header ever changes, [`HEADER`] stops
//! matching and the vendored conformance fixtures under
//! `tests/fixtures/vahtisiru_telemetry_csv/` must be re-synced deliberately.
//!
//! The upstream *reader* is permissive: malformed data rows are skipped and
//! counted. This module is the *producer*, so it is strict in the opposite
//! direction — it never writes a row that reader would skip:
//!
//! - A sensor that was **not measured** (a Parquet null) cannot be represented
//!   in this contract, and must never be zero-filled. Such rows are excluded and
//!   counted per column in [`ExportReport`].
//! - Anything that indicates corrupt or mis-specified input is a hard error:
//!   a missing source column, a source column with the wrong type (and therefore
//!   possibly the wrong unit), a non-finite reading, a negative timestamp,
//!   timestamps that are not strictly increasing, or zero usable rows.
//! - The rendered CSV is re-parsed with [`parse_csv`] (the upstream reader
//!   semantics) and must round-trip with zero skipped rows before it is
//!   returned.
//!
//! The session-labelled six-column export in [`crate::export`] is a different
//! artifact and is **not** corinth-ingestible: the trailing `session_label`
//! column is a header mismatch under this contract.

use anyhow::{Context, Result};
use polars::prelude::{DataFrame, DataType, LazyFrame, PlRefPath, ScanArgsParquet, col};
use serde::Serialize;
use std::path::{Path, PathBuf};

use crate::privacy::redact_personal_path;

/// Where the contract lives upstream.
pub const CONTRACT_SOURCE: &str = "rmems/vahtisiru:docs/telemetry_csv.md";
/// Upstream commit the header, semantics, and vendored fixtures were copied at.
pub const CONTRACT_REVISION: &str = "4feb196b53509461ceb9b1e7f9a6dd35e67db280";
/// Version of the upstream standalone validator crate
/// (`vahtisiru-telemetry-csv`) whose semantics [`parse_csv`] mirrors.
pub const VALIDATOR_VERSION: &str = "vahtisiru-telemetry-csv 0.1.0";

/// Frozen CSV header. Exact match after trim; extra columns fail upstream.
pub const HEADER: &str = "timestamp_ms,gpu_temp_c,gpu_power_w,cpu_tctl_c,cpu_package_power_w";
/// Contract columns, in order.
pub const COLUMNS: [&str; 5] = [
    "timestamp_ms",
    "gpu_temp_c",
    "gpu_power_w",
    "cpu_tctl_c",
    "cpu_package_power_w",
];
/// Number of comma-separated fields in a valid data row.
pub const FIELD_COUNT: usize = COLUMNS.len();

/// The Parquet batch columns this artifact reads, with the exact type each must
/// have. The type pins the unit: `power_usage_mw` is integer milliwatts from
/// NVML and `temperature_c` integer °C. A batch that carries any of these with a
/// different type is refused rather than reinterpreted.
const SOURCE_COLUMNS: [(&str, DataType); 5] = [
    ("timestamp_ms", DataType::Int64),
    ("temperature_c", DataType::UInt32),
    ("power_usage_mw", DataType::UInt32),
    ("cpu_tctl_c", DataType::Float32),
    ("cpu_package_power_w", DataType::Float32),
];

/// One contract row. Field names and order are the contract's.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CorinthRow {
    /// Unix-epoch milliseconds.
    pub timestamp_ms: u64,
    /// GPU die temperature, °C.
    pub gpu_temp_c: f32,
    /// GPU board power, watts.
    pub gpu_power_w: f32,
    /// CPU Tctl, °C.
    pub cpu_tctl_c: f32,
    /// CPU package power, watts.
    pub cpu_package_power_w: f32,
}

impl CorinthRow {
    /// One data line, no trailing newline.
    ///
    /// Floats use Rust's shortest round-trip representation (`{:?}`), so every
    /// value parses back to the identical `f32` and whole numbers keep their
    /// `.0` (`250.0`, matching the upstream fixtures byte-for-byte).
    #[must_use]
    pub fn to_csv_line(self) -> String {
        format!(
            "{},{:?},{:?},{:?},{:?}",
            self.timestamp_ms,
            self.gpu_temp_c,
            self.gpu_power_w,
            self.cpu_tctl_c,
            self.cpu_package_power_w
        )
    }
}

/// Rows dropped because a sensor was not measured, per contract column.
///
/// A row with several unmeasured sensors counts once in [`rows`] and once under
/// each column that was null.
///
/// [`rows`]: ExcludedRows::rows
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ExcludedRows {
    pub rows: usize,
    pub gpu_temp_c: usize,
    pub gpu_power_w: usize,
    pub cpu_tctl_c: usize,
    pub cpu_package_power_w: usize,
}

/// What the export did. Contains no paths or labels, so it is safe to publish
/// beside the CSV.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExportReport {
    pub contract_source: &'static str,
    pub contract_revision: &'static str,
    pub validator: &'static str,
    pub header: &'static str,
    pub batches: usize,
    pub source_rows: usize,
    pub rows_written: usize,
    pub excluded_unmeasured: ExcludedRows,
}

/// A rendered, self-validated corinth CSV plus its report.
#[derive(Debug, Clone)]
pub struct CorinthExport {
    pub csv: String,
    pub report: ExportReport,
}

fn redact(path: &Path) -> String {
    redact_personal_path(&path.display().to_string())
}

/// Flatten a foreign error through the redactor (polars embeds absolute paths).
fn redacted(error: impl std::fmt::Display) -> anyhow::Error {
    anyhow::anyhow!(redact_personal_path(&error.to_string()))
}

/// Read one batch's source columns, refusing any column with the wrong type.
fn read_batch(path: &Path) -> Result<DataFrame> {
    let scanned = LazyFrame::scan_parquet(
        PlRefPath::try_from_path(path)
            .with_context(|| format!("not a usable Parquet path: {}", redact(path)))?,
        ScanArgsParquet::default(),
    )
    .map_err(redacted)
    .with_context(|| format!("failed to scan {}", redact(path)))?;

    let frame = scanned
        .select(
            SOURCE_COLUMNS
                .iter()
                .map(|(name, _)| col(*name))
                .collect::<Vec<_>>(),
        )
        .collect()
        .map_err(redacted)
        .with_context(|| {
            format!(
                "batch {} is missing a column the corinth contract needs (one of {})",
                redact(path),
                SOURCE_COLUMNS
                    .iter()
                    .map(|(name, _)| *name)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;

    for (name, expected) in &SOURCE_COLUMNS {
        let actual = frame.column(name).map_err(redacted)?.dtype();
        anyhow::ensure!(
            actual == expected,
            "batch {}: column {name} has type {actual}, expected {expected}; refusing to \
             reinterpret it (the type pins the unit)",
            redact(path)
        );
    }
    Ok(frame)
}

/// A finite reading, a null (`Ok(None)`), or a hard error for NaN / ±inf.
fn finite(value: Option<f32>, column: &str, path: &Path, row: usize) -> Result<Option<f32>> {
    match value {
        Some(v) if !v.is_finite() => anyhow::bail!(
            "batch {} row {row}: {column} is {v}, which is not a measurement; the collector \
             must write null for an unmeasured sensor",
            redact(path)
        ),
        other => Ok(other),
    }
}

/// Build the corinth CSV from Parquet batches, already in batch order.
pub fn export(paths: &[PathBuf]) -> Result<CorinthExport> {
    anyhow::ensure!(!paths.is_empty(), "no Parquet batches to export");

    let mut rows: Vec<CorinthRow> = Vec::new();
    let mut excluded = ExcludedRows::default();
    let mut source_rows = 0usize;
    let mut previous: Option<i64> = None;

    for path in paths {
        let frame = read_batch(path)?;
        let column = |name: &str| frame.column(name).map_err(redacted);
        let timestamps = column("timestamp_ms")?.i64().map_err(redacted)?;
        let temps = column("temperature_c")?.u32().map_err(redacted)?;
        let power_mw = column("power_usage_mw")?.u32().map_err(redacted)?;
        let tctl = column("cpu_tctl_c")?.f32().map_err(redacted)?;
        let package = column("cpu_package_power_w")?.f32().map_err(redacted)?;

        for row in 0..frame.height() {
            source_rows += 1;

            let Some(timestamp) = timestamps.get(row) else {
                anyhow::bail!(
                    "batch {} row {row}: timestamp_ms is null; it is required on every row",
                    redact(path)
                );
            };
            let timestamp_ms = u64::try_from(timestamp).map_err(|_| {
                anyhow::anyhow!(
                    "batch {} row {row}: timestamp_ms {timestamp} is negative and not a valid \
                     u64 epoch-millisecond value",
                    redact(path)
                )
            })?;
            if let Some(previous) = previous {
                anyhow::ensure!(
                    timestamp > previous,
                    "batch {} row {row}: timestamp_ms {timestamp} does not follow the previous \
                     {previous}; timestamps must be strictly increasing across the export",
                    redact(path)
                );
            }
            previous = Some(timestamp);

            // u32 -> f32 is exact below 2^24, far above any die temperature.
            let gpu_temp_c = temps.get(row).map(|t| t as f32);
            // The only unit conversion: integer milliwatts to watts, in f64
            // before narrowing so the f32 is the nearest one to the exact value.
            let gpu_power_w = power_mw.get(row).map(|mw| (f64::from(mw) / 1000.0) as f32);
            let cpu_tctl_c = finite(tctl.get(row), "cpu_tctl_c", path, row)?;
            let cpu_package_power_w = finite(package.get(row), "cpu_package_power_w", path, row)?;

            match (gpu_temp_c, gpu_power_w, cpu_tctl_c, cpu_package_power_w) {
                (
                    Some(gpu_temp_c),
                    Some(gpu_power_w),
                    Some(cpu_tctl_c),
                    Some(cpu_package_power_w),
                ) => {
                    rows.push(CorinthRow {
                        timestamp_ms,
                        gpu_temp_c,
                        gpu_power_w,
                        cpu_tctl_c,
                        cpu_package_power_w,
                    });
                }
                (a, b, c, d) => {
                    excluded.rows += 1;
                    excluded.gpu_temp_c += usize::from(a.is_none());
                    excluded.gpu_power_w += usize::from(b.is_none());
                    excluded.cpu_tctl_c += usize::from(c.is_none());
                    excluded.cpu_package_power_w += usize::from(d.is_none());
                }
            }
        }
    }

    anyhow::ensure!(
        !rows.is_empty(),
        "no usable rows: all {source_rows} source row(s) have at least one unmeasured sensor \
         (nulls: gpu_temp_c={}, gpu_power_w={}, cpu_tctl_c={}, cpu_package_power_w={}); the \
         corinth contract requires every field and missing readings are never zero-filled",
        excluded.gpu_temp_c,
        excluded.gpu_power_w,
        excluded.cpu_tctl_c,
        excluded.cpu_package_power_w
    );

    let csv = format_csv(&rows);
    let parsed = parse_csv(&csv).context("self-check: rendered CSV failed the contract header")?;
    anyhow::ensure!(
        parsed.skipped == 0 && parsed.rows == rows,
        "self-check: rendered CSV does not round-trip through the contract reader \
         ({} of {} rows parsed back, {} skipped)",
        parsed.rows.len(),
        rows.len(),
        parsed.skipped
    );

    Ok(CorinthExport {
        csv,
        report: ExportReport {
            contract_source: CONTRACT_SOURCE,
            contract_revision: CONTRACT_REVISION,
            validator: VALIDATOR_VERSION,
            header: HEADER,
            batches: paths.len(),
            source_rows,
            rows_written: rows.len(),
            excluded_unmeasured: excluded,
        },
    })
}

/// Serialize rows under the frozen header, one `\n` per record.
#[must_use]
pub fn format_csv(rows: &[CorinthRow]) -> String {
    let mut out = String::from(HEADER);
    out.push('\n');
    for row in rows {
        out.push_str(&row.to_csv_line());
        out.push('\n');
    }
    out
}

/// Rows accepted by [`parse_csv`] plus how many malformed lines it skipped.
#[derive(Debug, Clone, PartialEq)]
pub struct LoadResult {
    pub rows: Vec<CorinthRow>,
    pub skipped: usize,
}

/// Mirror of upstream `vahtisiru::telemetry_csv::parse_csv` reader semantics,
/// used to self-check every export (no dependency on vahtisiru is taken).
///
/// Empty input and a header that is not exactly [`HEADER`] after trimming are
/// errors. Whitespace-only lines are ignored. A data line (trimmed as a whole;
/// fields are not trimmed) is malformed — skipped and counted — unless it has
/// exactly five fields, a `u64` timestamp, and four finite `f32` values.
pub fn parse_csv(contents: &str) -> Result<LoadResult> {
    let mut lines = contents.lines();
    let header = lines
        .next()
        .ok_or_else(|| anyhow::anyhow!("telemetry CSV is empty"))?
        .trim();
    anyhow::ensure!(
        header == HEADER,
        "telemetry CSV header mismatch: expected '{HEADER}', got '{header}'"
    );

    let mut rows = Vec::new();
    let mut skipped = 0usize;
    for raw in lines {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        match parse_data_line(line) {
            Some(row) => rows.push(row),
            None => skipped += 1,
        }
    }
    Ok(LoadResult { rows, skipped })
}

fn parse_data_line(line: &str) -> Option<CorinthRow> {
    let fields: Vec<&str> = line.split(',').collect();
    if fields.len() != FIELD_COUNT {
        return None;
    }
    let float = |value: &str| value.parse::<f32>().ok().filter(|v| v.is_finite());
    Some(CorinthRow {
        timestamp_ms: fields[0].parse::<u64>().ok()?,
        gpu_temp_c: float(fields[1])?,
        gpu_power_w: float(fields[2])?,
        cpu_tctl_c: float(fields[3])?,
        cpu_package_power_w: float(fields[4])?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_and_columns_agree() {
        assert_eq!(HEADER, COLUMNS.join(","));
        assert_eq!(HEADER.split(',').count(), FIELD_COUNT);
    }

    #[test]
    fn floats_render_with_a_decimal_and_round_trip() {
        let row = CorinthRow {
            timestamp_ms: 1,
            gpu_temp_c: 65.0,
            gpu_power_w: 250.123,
            cpu_tctl_c: 0.1,
            cpu_package_power_w: 0.00005,
        };
        let line = row.to_csv_line();
        assert_eq!(line, "1,65.0,250.123,0.1,5e-5");
        let parsed = parse_csv(&format_csv(&[row])).unwrap();
        assert_eq!(parsed.skipped, 0);
        assert_eq!(parsed.rows, vec![row]);
    }

    #[test]
    fn parse_rejects_the_six_column_labelled_header() {
        let error = parse_csv(&format!("{HEADER},session_label\n1,1.0,1.0,1.0,1.0,x\n"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("header mismatch"), "{error}");
    }

    #[test]
    fn parse_skips_empty_fields_rather_than_zeroing_them() {
        let parsed = parse_csv(&format!("{HEADER}\n1,60.0,100.0,55.0,\n")).unwrap();
        assert!(parsed.rows.is_empty());
        assert_eq!(parsed.skipped, 1);
    }

    #[test]
    fn fields_are_not_trimmed_individually() {
        let parsed = parse_csv(&format!("{HEADER}\n1, 60.0,100.0,55.0,40.0\n")).unwrap();
        assert_eq!(parsed.skipped, 1);
    }
}
