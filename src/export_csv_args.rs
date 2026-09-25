// SPDX-License-Identifier: MIT OR Apache-2.0

//! Argument parsing for the `export_csv` binary.
//!
//! Kept separate from [`crate::export`] so static analysis does not treat argv
//! collection as part of the atomic CSV write path. Uses [`std::env::args`]
//! (UTF-8 argv) so paths flow through `PathBuf` without lossy conversions in
//! the binary that performs the export.

use anyhow::Result;
use std::path::PathBuf;

#[derive(Debug, PartialEq, Eq)]
pub struct ExportCsvArgs {
    pub input: PathBuf,
    pub output: PathBuf,
}

pub fn export_csv_argv() -> impl Iterator<Item = String> {
    std::env::args()
}

pub fn parse_export_csv_args<I>(args: I) -> Result<ExportCsvArgs>
where
    I: IntoIterator<Item = String>,
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

    Ok(ExportCsvArgs {
        input: PathBuf::from(input),
        output,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parse_args_requires_an_input_path() {
        let error = parse_export_csv_args(["export_csv".to_owned()]).unwrap_err();
        assert!(error.to_string().contains("Usage: export_csv"));
    }

    #[test]
    fn parse_args_defaults_output_to_stdout() {
        let args = parse_export_csv_args(["export_csv".to_owned(), "session".to_owned()]).unwrap();
        assert_eq!(args.input, PathBuf::from("session"));
        assert_eq!(args.output, PathBuf::from("-"));
    }

    #[test]
    fn parse_args_preserves_explicit_output_path() {
        let args = parse_export_csv_args([
            "export_csv".to_owned(),
            "session".to_owned(),
            "out.csv".to_owned(),
        ])
        .unwrap();
        assert_eq!(args.output, PathBuf::from("out.csv"));
    }

    #[test]
    fn parse_args_rejects_surplus_operands() {
        let error = parse_export_csv_args([
            "export_csv".to_owned(),
            "session".to_owned(),
            "out.csv".to_owned(),
            "unexpected".to_owned(),
        ])
        .unwrap_err();
        assert!(error.to_string().contains("Usage: export_csv"));
    }
}
