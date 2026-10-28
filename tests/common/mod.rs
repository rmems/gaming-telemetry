// SPDX-License-Identifier: MIT OR Apache-2.0

//! Helpers shared by the corinth contract test binaries: synthetic Parquet
//! batches in collector types, scratch directories, and export wrappers.
//! Each test binary uses a subset, hence the `dead_code` allowance.

#![allow(dead_code)]

use gaming_telemetry::corinth_csv::{self, parse_csv};
use gaming_telemetry::export::resolve_inputs;
use gaming_telemetry::session::{BATCH_PREFIX, BATCH_SUFFIX};
use polars::prelude::{Column, DataFrame, ParquetWriter};
use std::path::{Path, PathBuf};

pub const GOLDEN: &str = include_str!("../fixtures/corinth_golden.csv");
pub const UPSTREAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/vahtisiru_telemetry_csv"
);

pub fn scratch(tag: &str) -> PathBuf {
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
pub struct Src {
    pub ts: i64,
    pub temp_c: Option<u32>,
    pub power_mw: Option<u32>,
    pub tctl_c: Option<f32>,
    pub package_w: Option<f32>,
}

impl Src {
    pub const fn at(ts: i64) -> Self {
        Self {
            ts,
            temp_c: Some(60),
            power_mw: Some(250_000),
            tctl_c: Some(70.0),
            package_w: Some(120.0),
        }
    }

    pub const fn gpu(self, temp_c: u32, power_mw: u32) -> Self {
        Self {
            temp_c: Some(temp_c),
            power_mw: Some(power_mw),
            ..self
        }
    }

    pub const fn cpu(self, tctl_c: f32, package_w: f32) -> Self {
        Self {
            tctl_c: Some(tctl_c),
            package_w: Some(package_w),
            ..self
        }
    }
}

pub fn frame(rows: &[Src]) -> DataFrame {
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

pub fn write_frame(dir: &Path, id: u32, mut df: DataFrame) -> PathBuf {
    let path = dir.join(format!("{BATCH_PREFIX}{id}{BATCH_SUFFIX}"));
    let mut file = std::fs::File::create(&path).unwrap();
    ParquetWriter::new(&mut file).finish(&mut df).unwrap();
    path
}

pub fn write_batch(dir: &Path, id: u32, rows: &[Src]) -> PathBuf {
    write_frame(dir, id, frame(rows))
}

pub fn export_ok(dir: &Path) -> corinth_csv::CorinthExport {
    corinth_csv::export(&resolve_inputs(dir).unwrap()).unwrap()
}

pub fn export_err(dir: &Path) -> String {
    let error = corinth_csv::export(&resolve_inputs(dir).unwrap())
        .expect_err("corrupt input must fail the export");
    format!("{error:#}")
}

/// Write the given batches into a fresh dir and assert the export fails with
/// an error containing `needle`.
pub fn assert_rejected(tag: &str, batches: &[&[Src]], needle: &str) -> String {
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

pub fn timestamps(csv: &str) -> Vec<u64> {
    parse_csv(csv)
        .unwrap()
        .rows
        .iter()
        .map(|r| r.timestamp_ms)
        .collect()
}

/// True if `text` contains anything shaped like an absolute path: a POSIX
/// `/segment/` or a Windows drive prefix such as `C:\` or `C:/`.
pub fn contains_absolute_path(text: &str) -> bool {
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
