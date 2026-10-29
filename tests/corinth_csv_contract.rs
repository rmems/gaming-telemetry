// SPDX-License-Identifier: MIT OR Apache-2.0

//! Conformance of the corinth-ingest CSV against the frozen vahtisiru telemetry
//! CSV contract (`rmems/vahtisiru` `docs/telemetry_csv.md`, RM-629).
//!
//! Two layers (the fail-loud gates are in `corinth_csv_gates.rs`, the CLI in
//! `corinth_csv_cli.rs`):
//! 1. Golden: Parquet batches in, `tests/fixtures/corinth_golden.csv` out,
//!    byte-for-byte.
//! 2. Upstream fixtures: our contract reader reproduces every expected result in
//!    the vendored vahtisiru fixture table, and our writer reproduces
//!    `canonical.csv` byte-for-byte.

use gaming_telemetry::corinth_csv::{
    COLUMNS, CONTRACT_REVISION, CorinthRow, HEADER, format_csv, parse_csv,
};
use gaming_telemetry::export::{labelled_frame, resolve_inputs, to_csv};
use std::path::Path;

mod common;

use common::*;

// ---------------------------------------------------------------- golden

#[test]
fn golden_header_and_rows_match_byte_for_byte() {
    let dir = scratch("golden");
    // Two batches, numbered so a lexical sort would misorder them.
    write_batch(&dir, 10, &[Src::at(2000).gpu(61, 252_500).cpu(70.5, 121.5)]);
    write_batch(&dir, 2, &[Src::at(1000).gpu(60, 250_000).cpu(70.0, 120.0)]);

    let export = export_ok(&dir);
    assert_eq!(export.csv, GOLDEN);
    assert_eq!(export.csv.lines().next(), Some(HEADER));
    assert_eq!(
        (
            export.report.rows_written,
            export.report.batches,
            export.report.contract_revision,
        ),
        (2, 2, CONTRACT_REVISION)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn golden_fixture_itself_satisfies_the_contract_reader() {
    let parsed = parse_csv(GOLDEN).unwrap();
    assert_eq!((parsed.skipped, parsed.rows.len()), (0, 2));
    assert_eq!(HEADER, COLUMNS.join(","));
}

#[test]
fn power_converts_from_milliwatts_exactly_once() {
    let dir = scratch("power");
    write_batch(&dir, 1, &[Src::at(1).gpu(65, 120_000)]);
    let row = parse_csv(&export_ok(&dir).csv).unwrap().rows[0];
    assert_eq!(
        row.gpu_power_w, 120.0,
        "120_000 mW is 120 W, not 120000 or 0.12"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn unmeasured_sensors_are_excluded_and_counted_never_zero_filled() {
    let dir = scratch("nulls");
    write_batch(
        &dir,
        1,
        &[
            // First RAPL poll: no delta yet.
            Src {
                package_w: None,
                ..Src::at(2)
            },
            Src {
                temp_c: None,
                power_mw: None,
                ..Src::at(3)
            },
            Src::at(4),
        ],
    );

    let export = export_ok(&dir);
    let parsed = parse_csv(&export.csv).unwrap();
    assert_eq!(parsed.skipped, 0, "no line the contract reader would skip");
    assert_eq!(timestamps(&export.csv), vec![4]);
    assert!(
        export.csv.lines().all(|l| !l.split(',').any(str::is_empty)),
        "no empty cells: {}",
        export.csv
    );

    let excluded = &export.report.excluded_unmeasured;
    assert_eq!(
        (
            export.report.source_rows,
            excluded.rows,
            excluded.cpu_package_power_w,
            excluded.gpu_temp_c,
            excluded.gpu_power_w,
            excluded.cpu_tctl_c,
        ),
        (3, 2, 1, 1, 1, 0)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The labelled artifact stays available and stays distinguishable: the
/// contract reader must refuse it outright.
#[test]
fn labelled_export_is_preserved_but_rejected_by_the_contract() {
    let dir = scratch("labelled");
    write_batch(&dir, 1, &[Src::at(1)]);
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

/// (fixture, expected timestamps, expected skipped) for every fixture the
/// upstream table marks `Ok`.
const UPSTREAM_OK: [(&str, &[u64], usize); 6] = [
    ("canonical.csv", &[1000, 2000], 0),
    ("canonical_crlf.csv", &[1000, 2000], 0),
    ("no_trailing_newline.csv", &[1000, 2000], 0),
    ("header_only.csv", &[], 0),
    ("malformed_rows.csv", &[1000, 4000, 7000], 7),
    ("whitespace_lines.csv", &[1000, 2000], 0),
];

/// (fixture, expected error substring) for every fixture the table marks `Err`.
const UPSTREAM_ERR: [(&str, &str); 3] = [
    ("header_extra_column.csv", "header mismatch"),
    ("header_mismatch.csv", "header mismatch"),
    ("empty.csv", "is empty"),
];

#[test]
fn reader_matches_the_upstream_fixture_table() {
    for (name, stamps, skipped) in UPSTREAM_OK {
        let parsed = parse_csv(&upstream(name)).unwrap();
        let got: Vec<u64> = parsed.rows.iter().map(|r| r.timestamp_ms).collect();
        assert_eq!(
            (got.as_slice(), parsed.skipped),
            (stamps, skipped),
            "{name}"
        );
    }
    for name in [
        "canonical.csv",
        "canonical_crlf.csv",
        "no_trailing_newline.csv",
    ] {
        let parsed = parse_csv(&upstream(name)).unwrap();
        assert_eq!(parsed.rows, upstream_canonical_rows(), "{name}: values");
    }
    for (name, needle) in UPSTREAM_ERR {
        let error = parse_csv(&upstream(name)).unwrap_err().to_string();
        assert!(error.contains(needle), "{name}: {error}");
    }
}
