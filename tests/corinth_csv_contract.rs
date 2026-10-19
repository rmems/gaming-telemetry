// SPDX-License-Identifier: MIT OR Apache-2.0

//! Conformance of the corinth-ingest CSV against the frozen vahtisiru telemetry
//! CSV contract (`rmems/vahtisiru` `docs/telemetry_csv.md`, RM-629).
//!
//! Three layers:
//! 1. Golden: Parquet batches in, `tests/fixtures/corinth_golden.csv` out,
//!    byte-for-byte.
//! 2. Upstream fixtures: our contract reader reproduces every expected result in
//!    the vendored vahtisiru fixture table, and our writer reproduces
//!    `canonical.csv` byte-for-byte.
//! 3. Fail-loud gates: every corrupt-input case errors instead of coercing.

use gaming_telemetry::corinth_csv::{
    self, COLUMNS, CONTRACT_REVISION, CorinthRow, HEADER, format_csv, parse_csv,
};
use gaming_telemetry::export::{labelled_frame, resolve_inputs, to_csv};
use gaming_telemetry::session::{BATCH_PREFIX, BATCH_SUFFIX};
use polars::prelude::{Column, DataFrame, ParquetWriter};
use std::path::{Path, PathBuf};

const GOLDEN: &str = include_str!("fixtures/corinth_golden.csv");
const UPSTREAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/vahtisiru_telemetry_csv"
);

fn scratch(tag: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "corinth_{tag}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// One source row, in collector types (the units the batch schema pins).
#[derive(Clone, Copy)]
struct Src {
    ts: i64,
    temp_c: Option<u32>,
    power_mw: Option<u32>,
    tctl_c: Option<f32>,
    package_w: Option<f32>,
}

const fn src(ts: i64, temp_c: u32, power_mw: u32, tctl_c: f32, package_w: f32) -> Src {
    Src {
        ts,
        temp_c: Some(temp_c),
        power_mw: Some(power_mw),
        tctl_c: Some(tctl_c),
        package_w: Some(package_w),
    }
}

fn frame(rows: &[Src]) -> DataFrame {
    let n = rows.len();
    DataFrame::new(
        n,
        vec![
            Column::new(
                "timestamp_ms".into(),
                rows.iter().map(|r| r.ts).collect::<Vec<_>>(),
            ),
            Column::new("session_label".into(), vec!["kcd2"; n]),
            Column::new(
                "power_usage_mw".into(),
                rows.iter().map(|r| r.power_mw).collect::<Vec<_>>(),
            ),
            Column::new(
                "temperature_c".into(),
                rows.iter().map(|r| r.temp_c).collect::<Vec<_>>(),
            ),
            Column::new(
                "cpu_tctl_c".into(),
                rows.iter().map(|r| r.tctl_c).collect::<Vec<_>>(),
            ),
            Column::new(
                "cpu_package_power_w".into(),
                rows.iter().map(|r| r.package_w).collect::<Vec<_>>(),
            ),
        ],
    )
    .unwrap()
}

fn write_frame(dir: &Path, id: u32, mut df: DataFrame) -> PathBuf {
    let path = dir.join(format!("{BATCH_PREFIX}{id}{BATCH_SUFFIX}"));
    let mut file = std::fs::File::create(&path).unwrap();
    ParquetWriter::new(&mut file).finish(&mut df).unwrap();
    path
}

fn write_batch(dir: &Path, id: u32, rows: &[Src]) -> PathBuf {
    write_frame(dir, id, frame(rows))
}

fn export_err(dir: &Path) -> String {
    let error = corinth_csv::export(&resolve_inputs(dir).unwrap())
        .expect_err("corrupt input must fail the export");
    format!("{error:#}")
}

// ---------------------------------------------------------------- golden

#[test]
fn golden_header_and_rows_match_byte_for_byte() {
    let dir = scratch("golden");
    // Two batches, numbered so a lexical sort would misorder them.
    write_batch(&dir, 10, &[src(2000, 61, 252_500, 70.5, 121.5)]);
    write_batch(&dir, 2, &[src(1000, 60, 250_000, 70.0, 120.0)]);

    let export = corinth_csv::export(&resolve_inputs(&dir).unwrap()).unwrap();
    assert_eq!(export.csv, GOLDEN);
    assert_eq!(
        export.csv.lines().next().unwrap(),
        "timestamp_ms,gpu_temp_c,gpu_power_w,cpu_tctl_c,cpu_package_power_w"
    );
    assert_eq!(export.report.rows_written, 2);
    assert_eq!(export.report.batches, 2);
    assert_eq!(export.report.contract_revision, CONTRACT_REVISION);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn golden_fixture_itself_satisfies_the_contract_reader() {
    let parsed = parse_csv(GOLDEN).unwrap();
    assert_eq!(parsed.skipped, 0);
    assert_eq!(parsed.rows.len(), 2);
    assert_eq!(HEADER, COLUMNS.join(","));
}

#[test]
fn power_converts_from_milliwatts_exactly_once() {
    let dir = scratch("power");
    write_batch(&dir, 1, &[src(1, 65, 120_000, 50.0, 40.0)]);
    let export = corinth_csv::export(&resolve_inputs(&dir).unwrap()).unwrap();
    let row = parse_csv(&export.csv).unwrap().rows[0];
    assert_eq!(
        row.gpu_power_w, 120.0,
        "120_000 mW is 120 W, not 120000 or 0.12"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn unmeasured_sensors_are_excluded_and_counted_never_zero_filled() {
    let dir = scratch("nulls");
    let mut cpu_power_missing = src(2, 60, 200_000, 50.0, 0.0);
    cpu_power_missing.package_w = None; // first RAPL poll: no delta yet
    let mut gpu_missing = src(3, 60, 200_000, 50.0, 30.0);
    gpu_missing.temp_c = None;
    gpu_missing.power_mw = None;
    write_batch(
        &dir,
        1,
        &[
            cpu_power_missing,
            gpu_missing,
            src(4, 61, 201_000, 51.0, 31.0),
        ],
    );

    let export = corinth_csv::export(&resolve_inputs(&dir).unwrap()).unwrap();
    let parsed = parse_csv(&export.csv).unwrap();
    assert_eq!(parsed.skipped, 0, "no line the contract reader would skip");
    assert_eq!(parsed.rows.len(), 1);
    assert_eq!(parsed.rows[0].timestamp_ms, 4);
    assert!(
        export.csv.lines().all(|l| !l.split(',').any(str::is_empty)),
        "no empty cells: {}",
        export.csv
    );

    let excluded = &export.report.excluded_unmeasured;
    assert_eq!(excluded.rows, 2);
    assert_eq!(excluded.cpu_package_power_w, 1);
    assert_eq!(excluded.gpu_temp_c, 1);
    assert_eq!(excluded.gpu_power_w, 1);
    assert_eq!(export.report.source_rows, 3);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The labelled artifact stays available and stays distinguishable: the
/// contract reader must refuse it outright.
#[test]
fn labelled_export_is_preserved_but_rejected_by_the_contract() {
    let dir = scratch("labelled");
    write_batch(&dir, 1, &[src(1, 60, 250_000, 70.0, 120.0)]);
    let mut df = labelled_frame(&resolve_inputs(&dir).unwrap()).unwrap();
    let labelled = to_csv(&mut df).unwrap();
    assert!(labelled.starts_with(&format!("{HEADER},session_label\n")));
    let error = parse_csv(&labelled).unwrap_err().to_string();
    assert!(error.contains("header mismatch"), "{error}");
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------- upstream fixtures

fn upstream(name: &str) -> String {
    std::fs::read_to_string(Path::new(UPSTREAM).join(name)).unwrap()
}

fn upstream_canonical_rows() -> Vec<CorinthRow> {
    vec![
        CorinthRow {
            timestamp_ms: 1000,
            gpu_temp_c: 60.5,
            gpu_power_w: 250.0,
            cpu_tctl_c: 70.0,
            cpu_package_power_w: 120.0,
        },
        CorinthRow {
            timestamp_ms: 2000,
            gpu_temp_c: 61.0,
            gpu_power_w: 252.5,
            cpu_tctl_c: 70.5,
            cpu_package_power_w: 121.5,
        },
    ]
}

#[test]
fn writer_reproduces_upstream_canonical_fixture_byte_for_byte() {
    assert_eq!(
        format_csv(&upstream_canonical_rows()),
        upstream("canonical.csv")
    );
}

#[test]
fn reader_matches_the_upstream_fixture_table() {
    for name in [
        "canonical.csv",
        "canonical_crlf.csv",
        "no_trailing_newline.csv",
    ] {
        let parsed = parse_csv(&upstream(name)).unwrap();
        assert_eq!(parsed.rows, upstream_canonical_rows(), "{name}");
        assert_eq!(parsed.skipped, 0, "{name}");
    }

    for name in ["header_extra_column.csv", "header_mismatch.csv"] {
        let error = parse_csv(&upstream(name)).unwrap_err().to_string();
        assert!(error.contains("header mismatch"), "{name}: {error}");
    }

    let error = parse_csv(&upstream("empty.csv")).unwrap_err().to_string();
    assert!(error.contains("is empty"), "{error}");

    let header_only = parse_csv(&upstream("header_only.csv")).unwrap();
    assert!(header_only.rows.is_empty());
    assert_eq!(header_only.skipped, 0);

    let malformed = parse_csv(&upstream("malformed_rows.csv")).unwrap();
    let stamps: Vec<u64> = malformed.rows.iter().map(|r| r.timestamp_ms).collect();
    assert_eq!(stamps, vec![1000, 4000, 7000]);
    assert_eq!(malformed.skipped, 7);

    let whitespace = parse_csv(&upstream("whitespace_lines.csv")).unwrap();
    assert_eq!(whitespace.rows.len(), 2);
    assert_eq!(whitespace.skipped, 0);
}

// ------------------------------------------------------- fail-loud gates

#[test]
fn rejects_a_batch_missing_a_contract_column() {
    let dir = scratch("missing_col");
    let df = frame(&[src(1, 60, 250_000, 70.0, 120.0)])
        .drop("cpu_tctl_c")
        .unwrap();
    write_frame(&dir, 1, df);
    let error = export_err(&dir);
    assert!(error.contains("missing a column"), "{error}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A float `power_usage_mw` may already be watts. Refuse rather than divide.
#[test]
fn rejects_a_source_column_with_the_wrong_type_or_unit() {
    let dir = scratch("wrong_unit");
    let mut df = frame(&[src(1, 60, 250_000, 70.0, 120.0)]);
    df.replace(
        "power_usage_mw",
        Column::new("power_usage_mw".into(), vec![250.0f64]),
    )
    .unwrap();
    write_frame(&dir, 1, df);
    let error = export_err(&dir);
    assert!(error.contains("power_usage_mw has type"), "{error}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn rejects_non_finite_readings() {
    for (tag, bad) in [
        ("nan", f32::NAN),
        ("inf", f32::INFINITY),
        ("ninf", f32::NEG_INFINITY),
    ] {
        let dir = scratch(tag);
        write_batch(
            &dir,
            1,
            &[
                src(1, 60, 250_000, 70.0, 120.0),
                src(2, 60, 250_000, bad, 120.0),
            ],
        );
        let error = export_err(&dir);
        assert!(error.contains("cpu_tctl_c is"), "{tag}: {error}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn rejects_non_monotonic_timestamps_within_a_batch() {
    let dir = scratch("backwards");
    write_batch(
        &dir,
        1,
        &[
            src(2000, 60, 250_000, 70.0, 120.0),
            src(1000, 60, 250_000, 70.0, 120.0),
        ],
    );
    let error = export_err(&dir);
    assert!(error.contains("strictly increasing"), "{error}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn rejects_duplicate_timestamps_across_batches() {
    let dir = scratch("dup");
    write_batch(&dir, 1, &[src(1000, 60, 250_000, 70.0, 120.0)]);
    write_batch(&dir, 2, &[src(1000, 60, 250_000, 70.0, 120.0)]);
    let error = export_err(&dir);
    assert!(error.contains("strictly increasing"), "{error}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Monotonicity is checked even on excluded rows: an out-of-order null row is
/// still evidence of a corrupt series.
#[test]
fn monotonicity_covers_excluded_rows_too() {
    let dir = scratch("backwards_null");
    let mut null_row = src(500, 60, 250_000, 70.0, 0.0);
    null_row.package_w = None;
    write_batch(&dir, 1, &[src(1000, 60, 250_000, 70.0, 120.0), null_row]);
    let error = export_err(&dir);
    assert!(error.contains("strictly increasing"), "{error}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn rejects_negative_timestamps() {
    let dir = scratch("negative");
    write_batch(&dir, 1, &[src(-1, 60, 250_000, 70.0, 120.0)]);
    let error = export_err(&dir);
    assert!(error.contains("not a valid u64"), "{error}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn rejects_null_timestamps() {
    let dir = scratch("null_ts");
    let mut df = frame(&[src(1, 60, 250_000, 70.0, 120.0)]);
    df.replace(
        "timestamp_ms",
        Column::new("timestamp_ms".into(), vec![None::<i64>]),
    )
    .unwrap();
    write_frame(&dir, 1, df);
    let error = export_err(&dir);
    assert!(error.contains("timestamp_ms is null"), "{error}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// RAPL is root-only on most machines, so a whole session can lack package
/// power. That must be a clear failure, not a header-only file.
#[test]
fn rejects_an_export_with_no_usable_rows() {
    let dir = scratch("no_rows");
    let mut row = src(1, 60, 250_000, 70.0, 0.0);
    row.package_w = None;
    let mut row2 = row;
    row2.ts = 2;
    write_batch(&dir, 1, &[row, row2]);
    let error = export_err(&dir);
    assert!(error.contains("no usable rows"), "{error}");
    assert!(error.contains("cpu_package_power_w=2"), "{error}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn report_carries_no_paths_or_labels() {
    let dir = scratch("report");
    write_batch(&dir, 1, &[src(1, 60, 250_000, 70.0, 120.0)]);
    let export = corinth_csv::export(&resolve_inputs(&dir).unwrap()).unwrap();
    let json = serde_json::to_string(&export.report).unwrap();
    assert!(!json.contains(dir.to_str().unwrap()), "path leaked: {json}");
    assert!(
        !json.contains("/home/") && !json.contains("/tmp/"),
        "{json}"
    );
    assert!(
        !json.contains("kcd2"),
        "session label leaked into report: {json}"
    );
    assert!(
        !export.csv.contains("kcd2"),
        "session label leaked into CSV"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
