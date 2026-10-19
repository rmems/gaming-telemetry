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
//! 3. Fail-loud gates: every corrupt-input case errors instead of coercing;
//!    benign clock jitter (equal-millisecond ties) is excluded and counted.

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
///
/// Built with [`Src::at`] (fully measured, fixed readings) and adjusted with
/// the chained setters or struct-update syntax for nulls.
#[derive(Clone, Copy)]
struct Src {
    ts: i64,
    temp_c: Option<u32>,
    power_mw: Option<u32>,
    tctl_c: Option<f32>,
    package_w: Option<f32>,
}

impl Src {
    const fn at(ts: i64) -> Self {
        Self {
            ts,
            temp_c: Some(60),
            power_mw: Some(250_000),
            tctl_c: Some(70.0),
            package_w: Some(120.0),
        }
    }

    const fn gpu(self, temp_c: u32, power_mw: u32) -> Self {
        Self {
            temp_c: Some(temp_c),
            power_mw: Some(power_mw),
            ..self
        }
    }

    const fn cpu(self, tctl_c: f32, package_w: f32) -> Self {
        Self {
            tctl_c: Some(tctl_c),
            package_w: Some(package_w),
            ..self
        }
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

fn export_ok(dir: &Path) -> corinth_csv::CorinthExport {
    corinth_csv::export(&resolve_inputs(dir).unwrap()).unwrap()
}

fn export_err(dir: &Path) -> String {
    let error = corinth_csv::export(&resolve_inputs(dir).unwrap())
        .expect_err("corrupt input must fail the export");
    format!("{error:#}")
}

/// Write the given batches into a fresh dir and assert the export fails with
/// an error containing `needle`.
fn assert_rejected(tag: &str, batches: &[&[Src]], needle: &str) -> String {
    let dir = scratch(tag);
    for (id, rows) in batches.iter().enumerate() {
        write_batch(&dir, u32::try_from(id).unwrap() + 1, rows);
    }
    let error = export_err(&dir);
    assert!(
        error.contains(needle),
        "{tag}: expected {needle:?} in {error}"
    );
    let _ = std::fs::remove_dir_all(&dir);
    error
}

fn timestamps(csv: &str) -> Vec<u64> {
    parse_csv(csv)
        .unwrap()
        .rows
        .iter()
        .map(|r| r.timestamp_ms)
        .collect()
}

/// True if `text` contains anything shaped like an absolute path: a POSIX
/// `/segment/` or a Windows drive prefix such as `C:\` or `C:/`.
fn contains_absolute_path(text: &str) -> bool {
    let bytes = text.as_bytes();
    let posix = bytes.windows(3).enumerate().any(|(i, w)| {
        w[0] == b'/'
            && w[1].is_ascii_alphanumeric()
            && text[i + 1..].find('/').is_some_and(|end| {
                text[i + 1..i + 1 + end]
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-$".contains(&b))
            })
    });
    let windows = bytes
        .windows(3)
        .any(|w| w[0].is_ascii_alphabetic() && w[1] == b':' && (w[2] == b'\\' || w[2] == b'/'));
    posix || windows
}

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

// ------------------------------------------------------- fail-loud gates

#[test]
fn rejects_a_batch_missing_a_contract_column() {
    let dir = scratch("missing_col");
    let df = frame(&[Src::at(1)]).drop("cpu_tctl_c").unwrap();
    write_frame(&dir, 1, df);
    let error = export_err(&dir);
    assert!(error.contains("missing a column"), "{error}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A float `power_usage_mw` may already be watts. Refuse rather than divide.
#[test]
fn rejects_a_source_column_with_the_wrong_type_or_unit() {
    let dir = scratch("wrong_unit");
    let mut df = frame(&[Src::at(1)]);
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
        let rows = [Src::at(1), Src::at(2).cpu(bad, 120.0)];
        assert_rejected(tag, &[&rows], "cpu_tctl_c is");
    }
}

#[test]
fn rejects_a_backward_step_within_a_batch() {
    let rows = [Src::at(2000), Src::at(1000)];
    let error = assert_rejected("backwards", &[&rows], "steps backward by 1000 ms");
    assert!(error.contains("NTP"), "{error}");
}

/// A wall-clock step between batches breaks the timeline just as surely.
#[test]
fn rejects_a_backward_step_across_a_batch_boundary() {
    let error = assert_rejected(
        "backwards_boundary",
        &[&[Src::at(1000), Src::at(2000)], &[Src::at(1500)]],
        "steps backward by 500 ms",
    );
    assert!(error.contains("row 2 (batch #2, row 0)"), "{error}");
}

/// Two polls inside one wall-clock millisecond at a batch boundary (late tick
/// under `MissedTickBehavior::Skip`, or `POLL_INTERVAL_MS=1`): the later row is
/// excluded and counted, the export succeeds, nothing is shifted.
#[test]
fn equal_millisecond_tie_at_a_batch_boundary_is_excluded_and_counted() {
    let dir = scratch("tie");
    write_batch(&dir, 1, &[Src::at(1000), Src::at(1001)]);
    write_batch(&dir, 2, &[Src::at(1001).gpu(99, 99_000), Src::at(1002)]);

    let export = export_ok(&dir);
    assert_eq!(timestamps(&export.csv), vec![1000, 1001, 1002]);
    assert!(
        !export.csv.contains("99.0"),
        "the first 1001 row is kept, not the tie"
    );
    assert_eq!(
        (
            export.report.excluded_duplicate_timestamp,
            export.report.source_rows,
            export.report.rows_written,
            export.report.excluded_unmeasured.rows,
        ),
        (1, 4, 3, 0)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Ordering is tracked even through rows excluded for an unmeasured sensor, so
/// a backward step cannot hide behind one.
#[test]
fn backward_step_after_an_excluded_row_is_still_rejected() {
    let excluded = Src {
        package_w: None,
        ..Src::at(1000)
    };
    let rows = [Src::at(500), excluded, Src::at(800)];
    assert_rejected("backwards_null", &[&rows], "steps backward by 200 ms");
}

/// A complete row sharing a millisecond with an *excluded* row is a valid
/// measurement and must be kept; only a tie with a *kept* row is a duplicate.
#[test]
fn complete_row_tied_with_an_excluded_row_is_kept() {
    let dir = scratch("tie_with_excluded");
    let unmeasured = Src {
        package_w: None,
        ..Src::at(1000)
    };
    write_batch(&dir, 1, &[Src::at(999), unmeasured, Src::at(1000)]);
    let export = export_ok(&dir);
    assert_eq!(timestamps(&export.csv), vec![999, 1000]);
    assert_eq!(
        (
            export.report.excluded_duplicate_timestamp,
            export.report.excluded_unmeasured.rows,
            export.report.rows_written,
        ),
        (0, 1, 2)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The kept-row cursor must not weaken backward-step detection: a tie with an
/// excluded row, then a step back, still fails.
#[test]
fn backward_step_after_a_tie_with_an_excluded_row_is_still_rejected() {
    let unmeasured = Src {
        tctl_c: None,
        ..Src::at(1000)
    };
    let rows = [Src::at(1000), unmeasured, Src::at(990)];
    assert_rejected("tie_then_back", &[&rows], "steps backward by 10 ms");
}

/// Finiteness is checked before any exclusion: a NaN in a row that would be
/// dropped (as a tie, or as unmeasured) still fails the export.
#[test]
fn nan_in_a_row_that_would_be_excluded_still_fails() {
    let duplicate_nan = [Src::at(1000), Src::at(1000).cpu(f32::NAN, 120.0)];
    assert_rejected("dup_nan", &[&duplicate_nan], "cpu_tctl_c is NaN");
    let unmeasured_nan = [Src {
        temp_c: None,
        ..Src::at(1).cpu(70.0, f32::INFINITY)
    }];
    assert_rejected("null_inf", &[&unmeasured_nan], "cpu_package_power_w is inf");
}

/// Null and negative are validated before ordering: a negative timestamp after
/// a positive one must be reported as negative, not as a backward step.
#[test]
fn negative_timestamp_is_reported_before_the_ordering_check() {
    let error = assert_rejected(
        "negative_after_positive",
        &[&[Src::at(1000), Src::at(-5)]],
        "is negative",
    );
    assert!(!error.contains("backward"), "{error}");
}

#[test]
fn rejects_negative_timestamps() {
    assert_rejected("negative", &[&[Src::at(-1)]], "not a valid u64");
}

#[test]
fn rejects_null_timestamps() {
    let dir = scratch("null_ts");
    let mut df = frame(&[Src::at(1)]);
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
    let rows = [1, 2].map(|ts| Src {
        package_w: None,
        ..Src::at(ts)
    });
    let error = assert_rejected("no_rows", &[&rows], "no usable rows");
    assert!(error.contains("cpu_package_power_w=2"), "{error}");
}

#[test]
fn absolute_path_detector_catches_common_shapes() {
    for path in [
        "/home/alice/x",
        "/var/folders/ab/T/x",
        "/run/user/1000/x",
        "/tmp/x/y",
        "C:\\Users\\x",
        "D:/data",
    ] {
        assert!(contains_absolute_path(path), "{path}");
    }
    assert!(!contains_absolute_path(
        "rmems/vahtisiru:docs/telemetry_csv.md"
    ));
}

#[test]
fn report_carries_no_paths_or_labels() {
    let dir = scratch("report");
    write_batch(&dir, 1, &[Src::at(1)]);
    let export = export_ok(&dir);
    let json = serde_json::to_string(&export.report).unwrap();
    assert!(!json.contains(dir.to_str().unwrap()), "path leaked: {json}");
    assert!(
        !contains_absolute_path(&json),
        "absolute path in report: {json}"
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

/// Ordering errors must not name a path either (they reach logs).
#[test]
fn ordering_errors_carry_no_paths() {
    let error = assert_rejected(
        "ordering_no_path",
        &[&[Src::at(2), Src::at(1)]],
        "steps backward",
    );
    assert!(!contains_absolute_path(&error), "{error}");
}

// ------------------------------------------------------------------ CLI

fn run_cli(args: &[&std::ffi::OsStr]) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_export_csv"))
        .args(args)
        .env("RUST_BACKTRACE", "0")
        .output()
        .unwrap()
}

/// The report is informational. The failure signal on stdout output is the
/// exit status and the error on stderr, with nothing written to stdout.
#[test]
fn cli_stdout_output_fails_loud_on_a_contract_violation() {
    let dir = scratch("cli_fail");
    write_batch(&dir, 1, &[Src::at(2), Src::at(1)]);
    let out = run_cli(&[dir.as_os_str(), "-".as_ref()]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        (out.status.success(), out.stdout.is_empty()),
        (false, true),
        "stderr: {stderr}"
    );
    assert!(stderr.contains("steps backward"), "{stderr}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Stdout output carries the CSV only; the report goes with file output.
#[test]
fn cli_stdout_output_is_the_csv_alone() {
    let dir = scratch("cli_ok");
    write_batch(&dir, 10, &[Src::at(2000).gpu(61, 252_500).cpu(70.5, 121.5)]);
    write_batch(&dir, 2, &[Src::at(1000).gpu(60, 250_000).cpu(70.0, 120.0)]);
    let out = run_cli(&[dir.as_os_str(), "-".as_ref()]);
    assert!(out.status.success());
    assert_eq!(String::from_utf8(out.stdout).unwrap(), GOLDEN);
    assert!(
        out.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let target = dir.join("canonical.csv");
    let out = run_cli(&[dir.as_os_str(), target.as_os_str()]);
    assert!(out.status.success());
    assert_eq!(std::fs::read_to_string(&target).unwrap(), GOLDEN);
    let report = std::fs::read_to_string(dir.join("canonical.csv.contract.json")).unwrap();
    assert!(report.contains(CONTRACT_REVISION), "{report}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// `--labelled` to a path that held a corinth export must not leave the corinth
/// report behind describing a five-column file that is gone.
#[test]
fn cli_labelled_over_a_corinth_export_removes_the_stale_report() {
    let dir = scratch("cli_sidecar");
    write_batch(&dir, 1, &[Src::at(1)]);
    let target = dir.join("out.csv");
    let sidecar = dir.join("out.csv.contract.json");

    assert!(
        run_cli(&[dir.as_os_str(), target.as_os_str()])
            .status
            .success()
    );
    assert!(sidecar.is_file(), "corinth export writes its report");

    let out = run_cli(&["--labelled".as_ref(), dir.as_os_str(), target.as_os_str()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !sidecar.exists(),
        "stale corinth report survived --labelled"
    );
    assert!(
        std::fs::read_to_string(&target)
            .unwrap()
            .starts_with(&format!("{HEADER},session_label\n"))
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A report path that cannot be replaced (a directory squats on it) is a
/// refused write for both artifacts: non-zero exit, and the existing CSV is
/// left exactly as it was.
#[test]
fn cli_refuses_to_write_when_the_report_path_cannot_be_replaced() {
    let dir = scratch("cli_refused");
    write_batch(&dir, 1, &[Src::at(1)]);
    let target = dir.join("out.csv");
    std::fs::write(&target, "old\n").unwrap();
    std::fs::create_dir(dir.join("out.csv.contract.json")).unwrap();

    for args in [
        vec![dir.as_os_str(), target.as_os_str()],
        vec!["--labelled".as_ref(), dir.as_os_str(), target.as_os_str()],
    ] {
        let out = run_cli(&args);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{args:?} must fail");
        assert!(stderr.contains("failed to remove stale"), "{stderr}");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "old\n");
    }
    let _ = std::fs::remove_dir_all(&dir);
}
