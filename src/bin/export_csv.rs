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

fn parse_args<I>(args: I) -> Result<ExportArgs>
where
    I: IntoIterator<Item = std::ffi::OsString>,
{
    let mut args = args.into_iter();
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
    let parsed = match parse_args(std::env::args_os()) {
        Ok(args) => args,
        Err(error) => {
            eprintln!("Error: {}", redact_personal_path(&format!("{error:?}")));
            return ExitCode::FAILURE;
        }
    };

    match run(parsed) {
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

fn run(args: ExportArgs) -> Result<()> {
    let inputs = resolve_inputs(&args.input)?;
    let mut df = canonical_frame(&inputs)?;
    let csv = to_csv(&mut df)?;

    if args.output.as_os_str() == OsStr::new("-") {
        print!("{csv}");
    } else {
        write_csv_atomically(&args.output, &csv)?;
        println!(
            "Exported {} rows from {} batch(es) to {}",
            df.height(),
            inputs.len(),
            args.output.display()
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    #[test]
    fn parse_args_requires_an_input_path() {
        let error = parse_args([OsString::from("export_csv")]).unwrap_err();
        assert!(error.to_string().contains("Usage: export_csv"));
    }

    #[test]
    fn parse_args_defaults_output_to_stdout() {
        let args = parse_args([OsString::from("export_csv"), OsString::from("session")]).unwrap();
        assert_eq!(args.input, PathBuf::from("session"));
        assert_eq!(args.output, PathBuf::from("-"));
    }

    #[test]
    fn parse_args_preserves_explicit_output_path() {
        let args = parse_args([
            OsString::from("export_csv"),
            OsString::from("session"),
            OsString::from("out.csv"),
        ])
        .unwrap();
        assert_eq!(args.output, PathBuf::from("out.csv"));
    }

    #[test]
    fn parse_args_rejects_surplus_operands() {
        let error = parse_args([
            OsString::from("export_csv"),
            OsString::from("session"),
            OsString::from("out.csv"),
            OsString::from("unexpected"),
        ])
        .unwrap_err();
        assert!(error.to_string().contains("Usage: export_csv"));
    }
}
