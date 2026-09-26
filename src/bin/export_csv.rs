// SPDX-License-Identifier: MIT OR Apache-2.0

//! Export the canonical CSV from a session's Parquet batches for corinth-canal.
//!
//! Accepts either a whole session directory (every batch, in order, one header) or
//! a single batch file. The column contract lives in `gaming_telemetry::export`.
//!
//! GPU and CPU sensor columns are nullable. A missing NVML/hwmon/RAPL read
//! is an empty CSV cell, never a fabricated `0`. Do not treat empty as zero.

use anyhow::Result;
use gaming_telemetry::export::{canonical_frame, resolve_inputs, to_csv, write_csv_atomically};
use gaming_telemetry::privacy::redact_personal_path;
use std::ffi::OsStr;
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Debug, PartialEq, Eq)]
struct ExportArgs {
    input: PathBuf,
    output: PathBuf,
}

fn parse_cli_argv(argv: Vec<String>) -> Result<ExportArgs> {
    let mut args = argv.into_iter();
    let _program = args.next();
    let Some(input) = args.next() else {
        anyhow::bail!("Usage: export_csv <session_dir | parquet_file> [output.csv]\n\n  ");
    };

    let output = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("-"));
    if args.next().is_some() {
        anyhow::bail!("Usage: export_csv <session_dir | parquet_file> [output.csv]");
    }

    Ok(ExportArgs {
        input: PathBuf::from(input),
        output,
    })
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
    let mut df = canonical_frame(&inputs)?;
    let csv = to_csv(&mut df)?;

    if export_args.output.as_os_str() == OsStr::new("-") {
        print!("{csv}");
    } else {
        write_csv_atomically(&export_args.output, &csv)?;
        println!(
            "Exported {} rows from {} batch(es) to {}",
            df.height(),
            inputs.len(),
            export_args.output.display()
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_args_requires_an_input_path() {
        let error = parse_cli_argv(vec!["export_csv".to_owned()]).unwrap_err();
        assert!(error.to_string().contains("Usage: export_csv"));
    }

    #[test]
    fn parse_args_defaults_output_to_stdout() {
        let args = parse_cli_argv(vec!["export_csv".to_owned(), "session".to_owned()]).unwrap();
        assert_eq!(args.input, PathBuf::from("session"));
        assert_eq!(args.output, PathBuf::from("-"));
    }

    #[test]
    fn parse_args_preserves_explicit_output_path() {
        let args = parse_cli_argv(vec![
            "export_csv".to_owned(),
            "session".to_owned(),
            "out.csv".to_owned(),
        ])
        .unwrap();
        assert_eq!(args.output, PathBuf::from("out.csv"));
    }

    #[test]
    fn parse_args_rejects_surplus_operands() {
        let error = parse_cli_argv(vec![
            "export_csv".to_owned(),
            "session".to_owned(),
            "out.csv".to_owned(),
            "unexpected".to_owned(),
        ])
        .unwrap_err();
        assert!(error.to_string().contains("Usage: export_csv"));
    }
}
