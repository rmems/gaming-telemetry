// SPDX-License-Identifier: MIT OR Apache-2.0

//! Fail-loud gates of the corinth-ingest export: every corrupt-input case
//! errors instead of coercing; benign clock jitter (equal-millisecond ties) is
//! excluded and counted. Conformance and golden tests are in
//! `corinth_csv_contract.rs`.

use polars::prelude::Column;

mod common;

use common::*;

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
    assert!(error.contains("refusing to reinterpret it"), "{error}");
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
