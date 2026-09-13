// SPDX-License-Identifier: MIT OR Apache-2.0

//! Canonical CSV projection over a session's Parquet batches.
//!
//! This is the contract with downstream replay (`corinth-canal`). It lives in the
//! library rather than inside `bin/export_csv.rs` so the column set and the batch
//! ordering are testable, instead of only being exercised by running the binary.

use anyhow::{Context, Result};
// Imported by name, not by glob: `polars::prelude::*` brings its own `Context`
// trait into scope and makes every `anyhow` `.context()` call ambiguous.
use polars::prelude::{
    CsvWriter, DataFrame, LazyFrame, PlRefPath, ScanArgsParquet, SerWriter, UnionArgs, col, concat,
    lit,
};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::privacy::redact_personal_path;
use crate::session::{BATCH_PREFIX, BATCH_SUFFIX};

/// Columns emitted by the canonical export, in order.
///
/// `session_label` is appended last so a consumer reading the original five
/// positionally keeps working, while multi-title captures become separable — the
/// whole point of recording the label in the first place.
pub const CANONICAL_COLUMNS: [&str; 6] = [
    "timestamp_ms",
    "gpu_temp_c",
    "gpu_power_w",
    "cpu_tctl_c",
    "cpu_package_power_w",
    "session_label",
];

/// Every batch file in a session directory, ordered by batch number.
///
/// Ordering is numeric, not lexical: `batch_10` sorts after `batch_2`, which a
/// plain filename sort or a glob would get backwards. Batch numbers are assigned
/// monotonically by the collector (and resume from the highest on restart), so
/// this ordering is also timestamp order — no global sort needed.
pub fn batch_files_in(dir: &Path) -> Result<Vec<PathBuf>> {
    let entries = std::fs::read_dir(dir).with_context(|| {
        format!(
            "failed to enumerate session directory {}",
            redact_personal_path(&dir.display().to_string())
        )
    })?;

    let mut batches: Vec<(u32, PathBuf)> = Vec::new();
    for entry in entries {
        let entry = entry.context("failed to enumerate a session directory entry")?;

        // `metadata()` rather than `file_type()`: the latter does not follow
        // symlinks, so a batch relocated to another mount and symlinked back
        // would be dropped as silently as the directory/FIFO cases this check
        // targets. A lookup failure (transient or mounted-fs error) is
        // propagated instead of being folded into "not a match" -- otherwise a
        // real stale-match could be silently skipped while the sweep still
        // reports success.
        let is_file = entry
            .metadata()
            .with_context(|| {
                format!(
                    "failed to inspect session entry {}",
                    redact_personal_path(&entry.path().display().to_string())
                )
            })?
            .is_file();
        if !is_file {
            continue;
        }

        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(id) = name
            .strip_prefix(BATCH_PREFIX)
            .and_then(|name| name.strip_suffix(BATCH_SUFFIX))
            .and_then(|id| id.parse::<u32>().ok())
        else {
            continue;
        };
        batches.push((id, entry.path()));
    }

    batches.sort_by_key(|(id, _)| *id);
    Ok(batches.into_iter().map(|(_, path)| path).collect())
}

/// Resolve an operator-supplied path into the batch files it names.
///
/// A directory means "the whole session"; a single file means just that file, so
/// the previous one-batch-at-a-time usage keeps working.
pub fn resolve_inputs(path: &Path) -> Result<Vec<PathBuf>> {
    if path.is_dir() {
        let batches = batch_files_in(path)?;
        anyhow::ensure!(
            !batches.is_empty(),
            "no {BATCH_PREFIX}*{BATCH_SUFFIX} files in {}",
            redact_personal_path(&path.display().to_string())
        );
        Ok(batches)
    } else {
        // The directory branch above rejects a FIFO or device masquerading as a
        // batch name; a single explicit path deserves the same protection, or a
        // typo'd device path blocks the export indefinitely at read time instead
        // of failing here with a clear message.
        anyhow::ensure!(
            path.is_file(),
            "{} is not a regular file",
            redact_personal_path(&path.display().to_string())
        );
        Ok(vec![path.to_path_buf()])
    }
}

/// The canonical projection applied to one already-scanned batch.
fn canonical_projection(frame: LazyFrame) -> LazyFrame {
    frame.select(&[
        col("timestamp_ms"),
        col("temperature_c").alias("gpu_temp_c"),
        // Milliwatts to watts. Kept as the only unit conversion in the contract.
        (col("power_usage_mw") / lit(1000.0)).alias("gpu_power_w"),
        col("cpu_tctl_c"),
        col("cpu_package_power_w"),
        col("session_label"),
    ])
}

/// Scan one batch and apply the canonical projection, with the path named in
/// every error this step can raise.
fn scan_and_project(path: &Path) -> Result<LazyFrame> {
    let scanned = LazyFrame::scan_parquet(
        PlRefPath::try_from_path(path).with_context(|| {
            format!(
                "not a usable Parquet path: {}",
                redact_personal_path(&path.display().to_string())
            )
        })?,
        ScanArgsParquet::default(),
    )
    .map_err(redacted)
    .with_context(|| {
        format!(
            "failed to scan {}",
            redact_personal_path(&path.display().to_string())
        )
    })?;
    Ok(canonical_projection(scanned))
}

/// Redacted, comma-joined batch paths, for schema-shaped failures that polars
/// itself cannot attribute to a specific batch (see `canonical_frame`).
fn redacted_batch_list(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|p| redact_personal_path(&p.display().to_string()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Build the canonical frame for a whole session, in batch order.
///
/// Concatenating projected frames (rather than projecting a concatenation) keeps
/// each batch's own schema local, so an IO-shaped failure (missing file,
/// permission denied) names itself in the error via the path polars embeds in
/// its own message. A schema-shaped failure -- a batch that scans cleanly but is
/// missing a projected column, e.g. a legacy batch or any schema drift -- fails
/// lazily at `concat`/`collect` instead, where polars names no path or column;
/// the batch count and list are attached here so directory-mode failures are
/// still attributable to *a* batch, even when polars itself cannot say which one.
pub fn canonical_frame(paths: &[PathBuf]) -> Result<DataFrame> {
    anyhow::ensure!(!paths.is_empty(), "no Parquet batches to export");

    let mut frames: Vec<LazyFrame> = Vec::with_capacity(paths.len());
    for path in paths {
        frames.push(scan_and_project(path)?);
    }

    let combined = if frames.len() == 1 {
        frames.remove(0)
    } else {
        concat(&frames, UnionArgs::default())
            .map_err(redacted)
            .with_context(|| {
                format!(
                    "failed to concatenate {} session batches: one of [{}] likely has a schema \
                     mismatch (missing a projected column, e.g. a legacy batch)",
                    paths.len(),
                    redacted_batch_list(paths)
                )
            })?
    };

    combined.collect().map_err(redacted).with_context(|| {
        format!(
            "failed to build the canonical export frame from [{}]",
            redacted_batch_list(paths)
        )
    })
}

/// Temp-file names get a pid/nanos/sequence suffix, not just a pid, so two calls
/// in the same process (or a process whose pid was reused) never share a path.
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn temp_export_path(path: &Path) -> PathBuf {
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    path.with_extension(format!("tmp.{}.{nanos}.{sequence}", std::process::id()))
}

/// Create `temporary`, write and fsync `csv` into it, cleaning up on any failure.
fn write_temp_export(temporary: &Path, csv: &str, redacted: impl Fn() -> String) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(temporary)
        .with_context(|| format!("failed to create a temporary export beside {}", redacted()))?;

    if let Err(error) = file.write_all(csv.as_bytes()).and_then(|_| file.sync_all()) {
        drop(file);
        let _ = std::fs::remove_file(temporary);
        return Err(error).with_context(|| {
            format!("failed to write the temporary export beside {}", redacted())
        });
    }
    Ok(())
}

/// Rename `temporary` into place and fsync its parent directory, cleaning up
/// the temporary on a failed rename.
fn publish_temp_export(temporary: &Path, path: &Path, redacted: impl Fn() -> String) -> Result<()> {
    if let Err(error) = std::fs::rename(temporary, path) {
        let _ = std::fs::remove_file(temporary);
        return Err(error).with_context(|| format!("failed to publish {}", redacted()));
    }

    let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
    let parent = parent.unwrap_or_else(|| Path::new("."));
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .with_context(|| {
            format!(
                "failed to persist export rename in {}",
                redact_personal_path(&parent.display().to_string())
            )
        })
}

/// Write CSV to `path` without truncating an existing export on failure.
///
/// A direct write truncates the destination before the new bytes land, so an I/O
/// failure part-way leaves a consumer with a partial or empty CSV that still looks
/// like a valid export. Writing beside the target and renaming makes the
/// replacement atomic. Mirrors `manifest.rs`'s `write_temp_manifest`/
/// `publish_manifest`: `create_new` refuses to write through a pre-planted
/// symlink at the temp path (CWE-59/377), the file is `sync_all`'d before the
/// rename, cleanup runs on any failure (not just a failed rename), and the
/// directory is fsynced after so the rename itself survives a crash.
///
/// (This is the third temp-then-rename site in the tree, after `manifest.rs` and
/// the Parquet writer in `main.rs`; consolidating them is tracked separately.)
pub fn write_csv_atomically(path: &Path, csv: &str) -> Result<()> {
    let temporary = temp_export_path(path);
    let redacted = || redact_personal_path(&path.display().to_string());

    write_temp_export(&temporary, csv, redacted)?;
    publish_temp_export(&temporary, path, redacted)
}

/// Wrap a foreign error with its operator path stripped.
///
/// Our own `.context(...)` strings are redacted at the call site, but the errors
/// underneath are not ours: polars embeds the absolute path in its own message
/// (`No such file or directory (os error 2): /home/<user>/…`), so wrapping without
/// flattening it through the redactor leaks the very thing the contexts hide.
fn redacted(error: impl std::fmt::Display) -> anyhow::Error {
    anyhow::anyhow!(redact_personal_path(&error.to_string()))
}

/// Render the canonical frame as CSV with a single header row.
pub fn to_csv(frame: &mut DataFrame) -> Result<String> {
    let mut buffer = Vec::new();
    CsvWriter::new(&mut buffer)
        .include_header(true)
        .finish(frame)
        .map_err(redacted)
        .context("failed to serialize CSV")?;
    String::from_utf8(buffer).context("CSV output was not valid UTF-8")
}

#[cfg(test)]
mod tests {
    use super::*;
    use polars::prelude::{Column, ParquetWriter};

    fn fixture_dir(tag: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-fixtures")
            .join(format!(
                "export_{tag}_{}_{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The last `n` characters of `text`, at a char boundary. Byte-slicing at a
    /// raw offset (`&text[text.len() - n..]`) can land inside a multi-byte
    /// character and panic -- safe today with ASCII-only fixtures, wrong the day
    /// a non-ASCII session label is tested.
    fn last_chars(text: &str, n: usize) -> String {
        let mut chars: Vec<char> = text.chars().rev().take(n).collect();
        chars.reverse();
        chars.into_iter().collect()
    }

    /// Write a batch carrying only the columns the projection reads.
    fn write_batch(dir: &Path, id: u32, timestamps: &[i64], label: &str) {
        let rows = timestamps.len();
        let mut df = DataFrame::new(
            rows,
            vec![
                Column::new("timestamp_ms".into(), timestamps.to_vec()),
                Column::new(
                    "session_label".into(),
                    vec![label; rows].into_iter().collect::<Vec<&str>>(),
                ),
                Column::new("temperature_c".into(), vec![65u32; rows]),
                Column::new("power_usage_mw".into(), vec![120_000u32; rows]),
                Column::new("cpu_tctl_c".into(), vec![Some(55.0f32); rows]),
                Column::new("cpu_package_power_w".into(), vec![None::<f32>; rows]),
            ],
        )
        .unwrap();
        let path = dir.join(format!("{BATCH_PREFIX}{id}{BATCH_SUFFIX}"));
        let mut file = std::fs::File::create(path).unwrap();
        ParquetWriter::new(&mut file).finish(&mut df).unwrap();
    }

    #[test]
    fn canonical_columns_include_the_session_label_last() {
        let dir = fixture_dir("columns");
        write_batch(&dir, 1, &[10, 20], "kcd2");

        let mut df = canonical_frame(&batch_files_in(&dir).unwrap()).unwrap();
        let names: Vec<String> = df
            .get_column_names()
            .iter()
            .map(|n| n.to_string())
            .collect();
        assert_eq!(names, CANONICAL_COLUMNS);

        let csv = to_csv(&mut df).unwrap();
        let header = csv.lines().next().unwrap();
        assert_eq!(header, CANONICAL_COLUMNS.join(","));
        assert!(csv.contains("kcd2"), "the label must reach the CSV");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `batch_10` must follow `batch_2`. A lexical sort or a plain glob gets this
    /// backwards and silently reorders the exported time series.
    #[test]
    fn batches_are_ordered_numerically_not_lexically() {
        let dir = fixture_dir("ordering");
        write_batch(&dir, 2, &[200], "re4r");
        write_batch(&dir, 10, &[1000], "re4r");
        write_batch(&dir, 1, &[100], "re4r");

        let files = batch_files_in(&dir).unwrap();
        let ids: Vec<String> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            ids,
            vec![
                format!("{BATCH_PREFIX}1{BATCH_SUFFIX}"),
                format!("{BATCH_PREFIX}2{BATCH_SUFFIX}"),
                format!("{BATCH_PREFIX}10{BATCH_SUFFIX}"),
            ]
        );

        let df = canonical_frame(&files).unwrap();
        let stamps: Vec<Option<i64>> = df
            .column("timestamp_ms")
            .unwrap()
            .i64()
            .unwrap()
            .iter()
            .collect();
        assert_eq!(stamps, vec![Some(100), Some(200), Some(1000)]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A whole session must export as one CSV with exactly one header row.
    #[test]
    fn a_multi_batch_session_exports_one_header() {
        let dir = fixture_dir("header");
        write_batch(&dir, 1, &[1, 2], "re2r");
        write_batch(&dir, 2, &[3, 4], "re2r");

        let mut df = canonical_frame(&resolve_inputs(&dir).unwrap()).unwrap();
        let csv = to_csv(&mut df).unwrap();
        assert_eq!(
            csv.lines()
                .filter(|line| line.starts_with("timestamp_ms"))
                .count(),
            1,
            "concatenated batches must not repeat the header"
        );
        assert_eq!(df.height(), 4);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The mW-to-W conversion is the only arithmetic transformation in the
    /// export contract; pin its exact value rather than only checking presence,
    /// so a wrong divisor or a raw passthrough would fail this test.
    #[test]
    fn power_is_converted_from_milliwatts_to_watts() {
        let dir = fixture_dir("power_units");
        write_batch(&dir, 1, &[1], "kcd2");

        let mut df = canonical_frame(&batch_files_in(&dir).unwrap()).unwrap();
        let csv = to_csv(&mut df).unwrap();
        let row = csv.lines().nth(1).unwrap();
        let power_w: f64 = row.split(',').nth(2).unwrap().parse().unwrap();
        assert_eq!(power_w, 120.0, "120_000 mW must export as 120 W");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An unavailable CPU sensor stays empty in the CSV rather than becoming 0.0.
    #[test]
    fn null_cpu_readings_export_as_empty_fields() {
        let dir = fixture_dir("nulls");
        write_batch(&dir, 1, &[1], "kcd2");

        let mut df = canonical_frame(&batch_files_in(&dir).unwrap()).unwrap();
        let csv = to_csv(&mut df).unwrap();
        let row = csv.lines().nth(1).unwrap();
        let power = row.split(',').nth(4).unwrap();
        assert!(
            power.is_empty(),
            "unavailable CPU power must be empty, got {power:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `export_csv` prints with `print!`, not `println!`. That is only correct if
    /// the writer already terminates the final record — otherwise piped output
    /// loses its last newline.
    #[test]
    fn csv_output_is_newline_terminated() {
        let dir = fixture_dir("trailing_newline");
        write_batch(&dir, 1, &[10, 20], "kcd2");

        let mut df = canonical_frame(&batch_files_in(&dir).unwrap()).unwrap();
        let csv = to_csv(&mut df).unwrap();
        assert!(
            csv.ends_with('\n'),
            "writer must terminate the last record; got {:?}",
            last_chars(&csv, 20)
        );
        assert!(
            !csv.ends_with("\n\n"),
            "exactly one terminator, so `print!` does not add a blank line"
        );
        assert_eq!(csv.lines().count(), 3, "header + two rows");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Every error this module raises names a path, and those strings reach logs
    /// and downstream reports. None may carry the operator's identity.
    #[test]
    fn scan_errors_redact_the_operator_path() {
        let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) else {
            return; // Nothing to redact against; `redact_personal_path` covers this.
        };
        let missing = home
            .join("gt_export_redaction_probe")
            .join("absent.parquet");

        let error =
            canonical_frame(std::slice::from_ref(&missing)).expect_err("missing file must fail");
        let text = format!("{error:?}");

        assert!(
            !text.contains(home.to_str().unwrap()),
            "raw home path leaked into an error: {text}"
        );
        // `redact_personal_path` runs its structural /home/<name> pass before the
        // literal $HOME substitution (see privacy.rs), so a plain HOME path is
        // redacted as `/home/$USER/...` rather than `$HOME/...` -- both markers
        // mean "the operator's identity was stripped here."
        assert!(
            text.contains("$USER") || text.contains("$HOME"),
            "expected a redacted marker in: {text}"
        );
    }

    #[test]
    fn a_directory_without_batches_is_an_error() {
        let dir = fixture_dir("empty");
        assert!(resolve_inputs(&dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A directory that happens to match the batch name must not be treated as a
    /// batch: it would fail confusingly at scan time instead of being skipped.
    #[test]
    fn non_regular_entries_are_not_treated_as_batches() {
        let dir = fixture_dir("entry_kinds");
        write_batch(&dir, 1, &[1], "kcd2");
        std::fs::create_dir(dir.join(format!("{BATCH_PREFIX}2{BATCH_SUFFIX}"))).unwrap();

        let files = batch_files_in(&dir).unwrap();
        assert_eq!(files.len(), 1, "only the regular file is a batch");
        assert!(files[0].is_file());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A failed export must not leave a truncated file where a good one was.
    #[test]
    fn writing_csv_replaces_atomically_and_leaves_no_temp_file() {
        let dir = fixture_dir("atomic");
        let target = dir.join("canonical.csv");
        std::fs::write(&target, "stale,contents\n").unwrap();

        write_csv_atomically(&target, "a,b\n1,2\n").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "a,b\n1,2\n");
        assert!(
            std::fs::read_dir(&dir)
                .unwrap()
                .flatten()
                .all(|e| !e.file_name().to_string_lossy().contains(".tmp.")),
            "a successful write leaves no temporary behind"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_single_file_path_still_exports() {
        let dir = fixture_dir("single");
        write_batch(&dir, 3, &[7], "cp2077");
        let path = dir.join(format!("{BATCH_PREFIX}3{BATCH_SUFFIX}"));

        let inputs = resolve_inputs(&path).unwrap();
        assert_eq!(inputs, vec![path.clone()]);
        assert_eq!(canonical_frame(&inputs).unwrap().height(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
