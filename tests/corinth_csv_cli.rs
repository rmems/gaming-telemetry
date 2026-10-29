// SPDX-License-Identifier: MIT OR Apache-2.0

//! The `export_csv` binary end to end: stdout carries the CSV alone, file output
//! publishes the CSV with its contract report, and `--labelled` never leaves a
//! stale corinth report behind.

use gaming_telemetry::corinth_csv::{CONTRACT_REVISION, HEADER};

mod common;

use common::*;

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
