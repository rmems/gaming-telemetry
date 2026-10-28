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
//!   possibly the wrong unit), a non-finite reading, a null or negative
//!   timestamp, a timestamp that steps *backward*, or zero usable rows.
//! - A complete row whose timestamp *equals* the last written row's (two polls
//!   inside one wall-clock millisecond, e.g. after a late tick or at
//!   `POLL_INTERVAL_MS=1`) is not corruption: it is excluded and counted in
//!   [`ExportReport::excluded_duplicate_timestamp`], never shifted. A tie with
//!   a row that was itself excluded does not count, so no valid row is lost.
//!   The upstream contract itself has no ordering rule; the backward-step and
//!   tie rules are producer-side so the published series is strictly
//!   increasing.
//! - Every reading is checked for finiteness before any row is excluded, so a
//!   NaN anywhere in the source fails the export.
//! - The rendered CSV is re-parsed line by line with the [`parse_csv`] rules
//!   (the upstream reader semantics) and must round-trip exactly, with zero
//!   skipped rows and strictly increasing timestamps, before it is returned.
//!
//! The session-labelled six-column export in [`crate::export`] is a different
//! artifact and is **not** corinth-ingestible: the trailing `session_label`
//! column is a header mismatch under this contract.

use anyhow::{Context, Result};
use polars::prelude::{
    DataFrame, DataType, Expr, Float32Chunked, Int64Chunked, LazyFrame, PlRefPath, ScanArgsParquet,
    UInt32Chunked, col,
};
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
    /// Complete rows dropped because their timestamp equalled the last
    /// written row's.
    pub excluded_duplicate_timestamp: usize,
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
    let frame = select_source_columns(path)?;
    check_source_types(&frame, path)?;
    Ok(frame)
}

/// Scan `path` and keep only the columns the contract reads.
fn select_source_columns(path: &Path) -> Result<DataFrame> {
    let scanned = LazyFrame::scan_parquet(
        PlRefPath::try_from_path(path)
            .with_context(|| format!("not a usable Parquet path: {}", redact(path)))?,
        ScanArgsParquet::default(),
    )
    .map_err(redacted)
    .with_context(|| format!("failed to scan {}", redact(path)))?;

    let wanted: Vec<Expr> = SOURCE_COLUMNS.iter().map(|(name, _)| col(*name)).collect();
    scanned
        .select(wanted)
        .collect()
        .map_err(redacted)
        .with_context(|| {
            let names: Vec<&str> = SOURCE_COLUMNS.iter().map(|(name, _)| *name).collect();
            format!(
                "batch {} is missing a column the corinth contract needs (one of {})",
                redact(path),
                names.join(", ")
            )
        })
}

/// Refuse any source column whose dtype differs from the pinned one.
fn check_source_types(frame: &DataFrame, path: &Path) -> Result<()> {
    for (name, expected) in &SOURCE_COLUMNS {
        let actual = frame.column(name).map_err(redacted)?.dtype();
        anyhow::ensure!(
            actual == expected,
            "batch {}: column {name} has type {actual}, expected {expected}; refusing to              reinterpret it (the type pins the unit)",
            redact(path)
        );
    }
    Ok(())
}

/// The typed source columns of one validated batch.
struct Columns<'a> {
    timestamps: &'a Int64Chunked,
    temps: &'a UInt32Chunked,
    power_mw: &'a UInt32Chunked,
    tctl: &'a Float32Chunked,
    package: &'a Float32Chunked,
}

impl<'a> Columns<'a> {
    fn new(frame: &'a DataFrame) -> Result<Self> {
        let column = |name: &str| frame.column(name).map_err(redacted);
        Ok(Self {
            timestamps: column("timestamp_ms")?.i64().map_err(redacted)?,
            temps: column("temperature_c")?.u32().map_err(redacted)?,
            power_mw: column("power_usage_mw")?.u32().map_err(redacted)?,
            tctl: column("cpu_tctl_c")?.f32().map_err(redacted)?,
            package: column("cpu_package_power_w")?.f32().map_err(redacted)?,
        })
    }

    /// The four sensor readings of `row`; NaN / ±inf is a hard error.
    fn readings(&self, row: usize, path: &Path) -> Result<Readings> {
        Ok((
            // u32 -> f32 is exact below 2^24, far above any die temperature.
            self.temps.get(row).map(|t| t as f32),
            // The only unit conversion: integer milliwatts to watts, in f64
            // before narrowing so the f32 is the nearest one to the exact value.
            self.power_mw
                .get(row)
                .map(|mw| (f64::from(mw) / 1000.0) as f32),
            finite(self.tctl.get(row), "cpu_tctl_c", path, row)?,
            finite(self.package.get(row), "cpu_package_power_w", path, row)?,
        ))
    }
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

/// The four sensor readings of one source row; `None` means not measured.
type Readings = (Option<f32>, Option<f32>, Option<f32>, Option<f32>);

/// Accumulates contract rows across batches, enforcing the per-row gates.
///
/// Per row, in this order: the timestamp is present and non-negative; every
/// reading is finite (so a NaN fails the export whatever would happen to the
/// row next); there is no backward step against `last_seen`; then the
/// keep/exclude decision. Two cursors keep the two ordering rules independent:
///
/// - `last_seen` — every row with a valid timestamp, kept or not, so a backward
///   step cannot hide behind an excluded row.
/// - `last_kept` — only rows actually written, so a complete row that shares a
///   millisecond with an *excluded* row is still kept, while one that ties a
///   *kept* row is dropped as a duplicate and the output stays strictly
///   increasing.
///
/// A row that is both unmeasured and tied is counted as unmeasured.
#[derive(Default)]
struct Collector {
    rows: Vec<CorinthRow>,
    excluded: ExcludedRows,
    duplicate_timestamps: usize,
    source_rows: usize,
    batches: usize,
    last_seen: Option<i64>,
    last_kept: Option<u64>,
}

impl Collector {
    /// Where the current row is, without any path: `row N (batch #B, row R)`.
    fn locate(&self, row: usize) -> String {
        format!(
            "row {} (batch #{}, row {row})",
            self.source_rows, self.batches
        )
    }

    /// Reject null and negative timestamps, then a backward step against
    /// `last_seen`, then advance `last_seen`.
    ///
    /// Null and negative are checked *before* ordering, so a negative value is
    /// always reported as negative (not as a backward step) and never becomes
    /// the cursor. Neither error names a path.
    fn timestamp(&mut self, value: Option<i64>, row: usize) -> Result<u64> {
        let at = self.locate(row);
        let Some(timestamp) = value else {
            anyhow::bail!("{at}: timestamp_ms is null; it is required on every row");
        };
        let timestamp_ms = u64::try_from(timestamp).map_err(|_| {
            anyhow::anyhow!(
                "{at}: timestamp_ms {timestamp} is negative and not a valid u64 \
                 epoch-millisecond value"
            )
        })?;
        if let Some(previous) = self.last_seen {
            anyhow::ensure!(
                timestamp >= previous,
                "{at}: timestamp_ms {timestamp} steps backward by {} ms from the previous \
                 {previous}; the timeline is broken, usually by an NTP or wall-clock step \
                 during capture. Split the session at this point before exporting",
                previous - timestamp
            );
        }
        self.last_seen = Some(timestamp);
        Ok(timestamp_ms)
    }

    /// Keep a fully measured row unless it ties the last kept one; count a
    /// partially measured one per column.
    fn record(&mut self, timestamp_ms: u64, readings: Readings) {
        match readings {
            (Some(_), Some(_), Some(_), Some(_)) if self.last_kept == Some(timestamp_ms) => {
                self.duplicate_timestamps += 1;
            }
            (Some(gpu_temp_c), Some(gpu_power_w), Some(cpu_tctl_c), Some(cpu_package_power_w)) => {
                self.last_kept = Some(timestamp_ms);
                self.rows.push(CorinthRow {
                    timestamp_ms,
                    gpu_temp_c,
                    gpu_power_w,
                    cpu_tctl_c,
                    cpu_package_power_w,
                });
            }
            (a, b, c, d) => {
                let excluded = &mut self.excluded;
                excluded.rows += 1;
                excluded.gpu_temp_c += usize::from(a.is_none());
                excluded.gpu_power_w += usize::from(b.is_none());
                excluded.cpu_tctl_c += usize::from(c.is_none());
                excluded.cpu_package_power_w += usize::from(d.is_none());
            }
        }
    }

    /// Read one batch and feed every row through the gates.
    fn batch(&mut self, path: &Path) -> Result<()> {
        let frame = read_batch(path)?;
        self.batches += 1;
        let columns = Columns::new(&frame)?;
        for row in 0..frame.height() {
            let timestamp_ms = self.timestamp(columns.timestamps.get(row), row)?;
            self.source_rows += 1;
            // Every reading is validated before any exclusion decision.
            let readings = columns.readings(row, path)?;
            self.record(timestamp_ms, readings);
        }
        Ok(())
    }

    /// Refuse an export in which every row was excluded.
    fn ensure_usable(&self) -> Result<()> {
        let excluded = &self.excluded;
        anyhow::ensure!(
            !self.rows.is_empty(),
            "no usable rows: all {} source row(s) were excluded (duplicate timestamps={}; \
             nulls: gpu_temp_c={}, gpu_power_w={}, cpu_tctl_c={}, cpu_package_power_w={}); \
             the corinth contract requires every field and missing readings are never \
             zero-filled",
            self.source_rows,
            self.duplicate_timestamps,
            excluded.gpu_temp_c,
            excluded.gpu_power_w,
            excluded.cpu_tctl_c,
            excluded.cpu_package_power_w
        );
        Ok(())
    }

    fn report(self, batches: usize) -> ExportReport {
        ExportReport {
            contract_source: CONTRACT_SOURCE,
            contract_revision: CONTRACT_REVISION,
            validator: VALIDATOR_VERSION,
            header: HEADER,
            batches,
            source_rows: self.source_rows,
            rows_written: self.rows.len(),
            excluded_unmeasured: self.excluded,
            excluded_duplicate_timestamp: self.duplicate_timestamps,
        }
    }
}

/// Render rows and prove the result round-trips through the contract reader.
///
/// Verification streams: each rendered line is parsed with the contract
/// reader's line rules and compared with the row it came from, so no second
/// row vector is held next to `rows` and the CSV string.
fn render_verified(rows: &[CorinthRow]) -> Result<String> {
    let csv = format_csv(rows);
    verify_round_trip(&csv, rows)?;
    Ok(csv)
}

/// Check that `csv` parses back, line for line, to exactly `rows`, with no
/// skipped line and strictly increasing timestamps.
fn verify_round_trip(csv: &str, rows: &[CorinthRow]) -> Result<()> {
    let mut lines = csv.lines();
    anyhow::ensure!(
        lines.next().map(str::trim) == Some(HEADER),
        "self-check: rendered CSV failed the contract header"
    );
    let mut expected = rows.iter();
    let mut previous: Option<u64> = None;
    for (index, line) in lines.map(str::trim).filter(|l| !l.is_empty()).enumerate() {
        let parsed = parse_data_line(line);
        anyhow::ensure!(
            parsed.is_some() && parsed.as_ref() == expected.next(),
            "self-check: rendered data line {index} does not round-trip through the \
             contract reader"
        );
        let timestamp = parsed.map(|r| r.timestamp_ms);
        anyhow::ensure!(
            previous < timestamp,
            "self-check: rendered CSV timestamps are not strictly increasing at data line \
             {index}"
        );
        previous = timestamp;
    }
    anyhow::ensure!(
        expected.next().is_none(),
        "self-check: rendered CSV has fewer rows than were collected"
    );
    Ok(())
}

/// Build the corinth CSV from Parquet batches, already in batch order.
pub fn export(paths: &[PathBuf]) -> Result<CorinthExport> {
    anyhow::ensure!(!paths.is_empty(), "no Parquet batches to export");
    let mut collector = Collector::default();
    for path in paths {
        collector.batch(path)?;
    }
    collector.ensure_usable()?;
    let csv = render_verified(&collector.rows)?;
    Ok(CorinthExport {
        csv,
        report: collector.report(paths.len()),
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

    fn row(timestamp_ms: u64) -> CorinthRow {
        CorinthRow {
            timestamp_ms,
            gpu_temp_c: 60.0,
            gpu_power_w: 250.0,
            cpu_tctl_c: 70.0,
            cpu_package_power_w: 120.0,
        }
    }

    /// The streaming verifier must catch everything the collect-and-compare
    /// version did: tampered values, missing/extra rows, malformed lines,
    /// header drift, and non-increasing order.
    #[test]
    fn streaming_verifier_rejects_every_kind_of_divergence() {
        let rows = [row(1), row(2)];
        let good = format_csv(&rows);
        verify_round_trip(&good, &rows).unwrap();

        let cases = [
            ("tampered", good.replace("2,60.0", "2,61.0"), &rows[..]),
            ("missing row", format_csv(&rows[..1]), &rows[..]),
            ("extra row", good.clone(), &rows[..1]),
            (
                "malformed",
                format!("{good}3,60.0,,70.0,120.0\n"),
                &rows[..],
            ),
            ("header", good.replacen("timestamp_ms", "ts", 1), &rows[..]),
        ];
        for (name, csv, expected) in cases {
            assert!(verify_round_trip(&csv, expected).is_err(), "{name}");
        }
        let tied = [row(2), row(2)];
        let error = verify_round_trip(&format_csv(&tied), &tied).unwrap_err();
        assert!(error.to_string().contains("strictly increasing"), "{error}");
    }

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
