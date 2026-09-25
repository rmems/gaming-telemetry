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
use gaming_telemetry::export_csv_args::{ExportCsvArgs, export_csv_argv, parse_export_csv_args};
use gaming_telemetry::privacy::redact_personal_path;
use std::ffi::OsStr;
use std::process::ExitCode;

fn main() -> ExitCode {
    let parsed = match parse_export_csv_args(export_csv_argv()) {
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

fn run(args: ExportCsvArgs) -> Result<()> {
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
