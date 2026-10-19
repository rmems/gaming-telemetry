// SPDX-License-Identifier: MIT OR Apache-2.0

//! Export a session's Parquet batches as CSV.
//!
//! Two artifacts, selected explicitly:
//!
//! - **default: corinth ingest** — the frozen five-column vahtisiru telemetry
//!   CSV contract (`gaming_telemetry::corinth_csv`). Rows with an unmeasured
//!   sensor are excluded and counted (never zero-filled); corrupt input fails the
//!   export with a non-zero exit before anything is printed, on stdout or file
//!   output alike. For a file output, a contract report (source, revision, row
//!   counts) is written beside it as `<output>.contract.json` and echoed to
//!   stderr; stdout output carries the CSV only.
//! - **`--labelled`** — the six-column session-labelled CSV for multi-title
//!   datasets. Unmeasured sensors are empty cells. Not corinth-ingestible.
//!
//! Accepts either a whole session directory (every batch, in order, one header) or
//! a single batch file.

use anyhow::Result;
use gaming_telemetry::corinth_csv;
use gaming_telemetry::export::{labelled_frame, resolve_inputs, to_csv, write_csv_atomically};
use gaming_telemetry::privacy::redact_personal_path;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const USAGE: &str = "Usage: export_csv [--labelled] <session_dir | parquet_file> [output.csv]\n\n  \
     default     corinth-ingest CSV (frozen vahtisiru telemetry CSV contract)\n  \
     --labelled  six-column session-labelled CSV (not corinth-ingestible)";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Artifact {
    Corinth,
    Labelled,
}

#[derive(Debug, PartialEq, Eq)]
struct ExportArgs {
    artifact: Artifact,
    input: PathBuf,
    output: PathBuf,
}

fn parse_cli_argv(argv: Vec<String>) -> Result<ExportArgs> {
    let mut artifact = Artifact::Corinth;
    let mut operands = Vec::new();
    for arg in argv.into_iter().skip(1) {
        match arg.as_str() {
            "--labelled" => artifact = Artifact::Labelled,
            flag if flag.starts_with("--") => anyhow::bail!("unknown option {flag:?}\n{USAGE}"),
            _ => operands.push(arg),
        }
    }
    let mut operands = operands.into_iter();
    let Some(input) = operands.next() else {
        anyhow::bail!("{USAGE}");
    };
    let output = operands
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("-"));
    if operands.next().is_some() {
        anyhow::bail!("{USAGE}");
    }

    Ok(ExportArgs {
        artifact,
        input: PathBuf::from(input),
        output,
    })
}

/// `<output>.contract.json`, beside the CSV.
fn report_path(output: &Path) -> PathBuf {
    let mut name = output.as_os_str().to_owned();
    name.push(".contract.json");
    PathBuf::from(name)
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            // Last line of defence. `main() -> Result` would print the chain
            // verbatim, and the layers underneath are not ours: polars and
            // `std::io` embed absolute paths in their own messages, so redacting
            // only the contexts we author is not enough to keep the operator's
            // identity out of stderr.
            eprintln!("Error: {}", redact_personal_path(&format!("{error:?}")));
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    // CLI export must read operator-supplied paths; atomic write hardening is in
    // `write_csv_atomically` (create_new + temp-then-rename).
    let args: Vec<String> = std::env::args().collect(); // nosemgrep: rust.lang.security.args.args
    let export_args = parse_cli_argv(args)?;
    let inputs = resolve_inputs(&export_args.input)?;
    let to_stdout = export_args.output.as_os_str() == OsStr::new("-");
    let shown = || redact_personal_path(&export_args.output.display().to_string());

    match export_args.artifact {
        Artifact::Corinth => {
            let export = corinth_csv::export(&inputs)?;
            let report = serde_json::to_string_pretty(&export.report)?;
            if to_stdout {
                print!("{}", export.csv);
            } else {
                write_csv_atomically(&export_args.output, &export.csv)?;
                write_csv_atomically(&report_path(&export_args.output), &format!("{report}\n"))?;
                println!(
                    "Exported {} corinth row(s) from {} batch(es) to {}",
                    export.report.rows_written,
                    inputs.len(),
                    shown()
                );
                eprintln!("{report}");
            }
        }
        Artifact::Labelled => {
            let mut df = labelled_frame(&inputs)?;
            let csv = to_csv(&mut df)?;
            if to_stdout {
                print!("{csv}");
            } else {
                write_csv_atomically(&export_args.output, &csv)?;
                println!(
                    "Exported {} labelled row(s) from {} batch(es) to {} (not corinth-ingestible)",
                    df.height(),
                    inputs.len(),
                    shown()
                );
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(args: &[&str]) -> Vec<String> {
        std::iter::once("export_csv")
            .chain(args.iter().copied())
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn parse_args_requires_an_input_path() {
        let error = parse_cli_argv(argv(&[])).unwrap_err();
        assert!(error.to_string().contains("Usage: export_csv"));
    }

    #[test]
    fn parse_args_defaults_to_corinth_on_stdout() {
        let args = parse_cli_argv(argv(&["session"])).unwrap();
        assert_eq!(args.artifact, Artifact::Corinth);
        assert_eq!(args.input, PathBuf::from("session"));
        assert_eq!(args.output, PathBuf::from("-"));
    }

    #[test]
    fn parse_args_preserves_explicit_output_path() {
        let args = parse_cli_argv(argv(&["session", "out.csv"])).unwrap();
        assert_eq!(args.output, PathBuf::from("out.csv"));
    }

    #[test]
    fn parse_args_selects_the_labelled_artifact_explicitly() {
        let args = parse_cli_argv(argv(&["--labelled", "session", "out.csv"])).unwrap();
        assert_eq!(args.artifact, Artifact::Labelled);
        assert_eq!(args.input, PathBuf::from("session"));
    }

    #[test]
    fn parse_args_rejects_unknown_options() {
        let error = parse_cli_argv(argv(&["--zero-fill", "session"])).unwrap_err();
        assert!(error.to_string().contains("unknown option"));
    }

    #[test]
    fn parse_args_rejects_surplus_operands() {
        let error = parse_cli_argv(argv(&["session", "out.csv", "unexpected"])).unwrap_err();
        assert!(error.to_string().contains("Usage: export_csv"));
    }

    #[test]
    fn contract_report_sits_beside_the_csv() {
        assert_eq!(
            report_path(Path::new("out/canonical.csv")),
            PathBuf::from("out/canonical.csv.contract.json")
        );
    }
}
