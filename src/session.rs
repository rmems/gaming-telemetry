// SPDX-License-Identifier: MIT OR Apache-2.0

//! Session directory, label, and batch-numbering helpers.

use anyhow::{Context, Result, bail};
use fs2::FileExt;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

/// Prefix of the Parquet batch files the collector writes.
pub const BATCH_PREFIX: &str = "gpu_telemetry_v2_batch_";
pub const BATCH_SUFFIX: &str = ".parquet";
/// Default collector cadence when `POLL_INTERVAL_MS` is not configured.
pub const DEFAULT_POLL_INTERVAL_MS: u64 = 5;

/// Parse the requested poll interval, rejecting the zero duration that Tokio
/// cannot construct an interval from.
pub fn parse_poll_interval_ms(raw: Option<&str>) -> Result<u64> {
    let Some(raw) = raw.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(DEFAULT_POLL_INTERVAL_MS);
    };
    let interval = raw
        .parse::<u64>()
        .with_context(|| format!("POLL_INTERVAL_MS must be a positive integer, got {raw:?}"))?;
    if interval == 0 {
        bail!("POLL_INTERVAL_MS must be greater than zero");
    }
    Ok(interval)
}

/// An advisory, process-lifetime lock for a capture directory.
///
/// A second collector writing the same directory can otherwise race both the
/// manifest and batch-number scan. The lock is released automatically when this
/// value is dropped at process exit.
pub struct SessionLock {
    _file: File,
}

/// Acquire exclusive ownership of `dir` for one collector process.
pub fn acquire_exclusive(dir: &Path) -> Result<SessionLock> {
    let path = dir.join(".gaming-telemetry.lock");
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .with_context(|| {
            format!(
                "failed to open session lock {}",
                crate::privacy::redact_personal_path(&path.display().to_string())
            )
        })?;

    file.try_lock_exclusive().with_context(|| {
        format!(
            "another collector already holds the session lock for {}",
            crate::privacy::redact_personal_path(&dir.display().to_string())
        )
    })?;

    Ok(SessionLock { _file: file })
}

/// Keep session tags as short ASCII identifiers (labels only — never used for
/// paths or exec).
pub fn sanitize_label(raw: &str) -> String {
    raw.chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
        .take(64)
        .collect()
}

/// Multi-game session tag. Production uses `SESSION_LABEL` only; the optional
/// `cli_label` parameter exists so precedence and sanitization stay unit testable.
pub fn resolve_label_from_sources(cli_label: Option<&str>, env_label: Option<&str>) -> String {
    // Sanitize each candidate *before* choosing between them. Filtering on the raw
    // value first lets a non-empty CLI label that sanitizes away — all punctuation,
    // or non-ASCII — win the precedence check and silently discard a perfectly
    // good env label, leaving the run unlabelled.
    [cli_label, env_label]
        .into_iter()
        .flatten()
        .map(sanitize_label)
        .find(|label| !label.is_empty())
        .unwrap_or_default()
}

/// Runtime label resolution. Operators set `SESSION_LABEL` (see README).
///
/// CLI argv is intentionally not read here: Codacy flags `std::env::args` as a
/// security surface for the long-running daemon, and env alone is enough for
/// multi-title capture.
pub fn resolve_label() -> String {
    let raw = std::env::var("SESSION_LABEL").ok();
    let label = resolve_label_from_sources(None, raw.as_deref());

    // An operator who mistypes a label otherwise gets the same silent result as
    // setting none at all: every row lands in the anonymous bucket, and the loss
    // is only visible after the capture.
    if label.is_empty()
        && let Some(raw) = raw.as_deref().map(str::trim)
        && !raw.is_empty()
    {
        eprintln!(
            "SESSION_LABEL {raw:?} has no usable characters (allowed: A-Z a-z 0-9 _ - .); \
             this session will be recorded with an empty label."
        );
    }
    label
}

/// Where this run writes its batches, manifest, and events.
///
/// `SESSION_DIR` is created if missing. Unset falls back to the current working
/// directory, which is how the collector behaved before session directories
/// existed.
pub fn resolve_dir() -> Result<PathBuf> {
    match std::env::var("SESSION_DIR")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
    {
        Some(dir) => {
            let path = PathBuf::from(dir);
            std::fs::create_dir_all(&path).with_context(|| {
                format!(
                    "failed to create SESSION_DIR {}",
                    crate::privacy::redact_personal_path(&path.display().to_string())
                )
            })?;
            Ok(path)
        }
        None => std::env::current_dir().context("failed to read current directory"),
    }
}

/// Stable identifier for a capture session.
///
/// Derived from the caller-provided session label and start timestamp, never from
/// a directory name that could contain personal information.
pub fn session_id(label: &str, started_at: &str) -> String {
    let stamp = sanitize_label(started_at);
    if label.is_empty() {
        format!("session_{stamp}")
    } else {
        format!("{label}_{stamp}")
    }
}

/// Highest batch id already present in `dir`, or 0 if there are none.
///
/// The collector used to start `batch_counter` at 0 on every process, so
/// restarting a capture in a populated directory silently overwrote
/// `gpu_telemetry_v2_batch_1.parquet`. Numbering resumes from here instead.
///
/// Parses the numeric suffix rather than sorting names — lexicographically
/// `batch_10` sorts before `batch_2`.
pub fn highest_batch_id(dir: &Path) -> Result<u32> {
    let entries = std::fs::read_dir(dir).with_context(|| {
        format!(
            "failed to enumerate session directory {}",
            crate::privacy::redact_personal_path(&dir.display().to_string())
        )
    })?;
    let mut highest = 0;
    for entry in entries {
        let entry = entry.context("failed to enumerate a session directory entry")?;
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
        highest = highest.max(id);
    }
    Ok(highest)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-fixtures")
            .join(format!(
                "session_{tag}_{}_{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn sanitize_label_strips_shell_metacharacters() {
        assert_eq!(sanitize_label("kcd2;rm -rf /"), "kcd2rm-rf");
        assert_eq!(sanitize_label("re_requiem"), "re_requiem");
        assert_eq!(sanitize_label("...ok"), "...ok");
        assert_eq!(sanitize_label(&"a".repeat(80)).len(), 64);
    }

    #[test]
    fn resolve_label_precedence_and_trimming() {
        assert_eq!(resolve_label_from_sources(None, None), "");
        assert_eq!(resolve_label_from_sources(None, Some("")), "");
        assert_eq!(resolve_label_from_sources(None, Some("  kcd2  ")), "kcd2");
        assert_eq!(
            resolve_label_from_sources(Some("cli"), Some("env")),
            "cli",
            "CLI label should take precedence over env"
        );
        assert_eq!(
            resolve_label_from_sources(Some(""), Some("env")),
            "env",
            "empty CLI label should fall back to env"
        );
        assert_eq!(
            resolve_label_from_sources(Some("!!!"), Some("kcd2")),
            "kcd2",
            "a CLI label that sanitizes away must not discard a valid env label"
        );
        assert_eq!(
            resolve_label_from_sources(Some("!!!"), None),
            "",
            "nothing usable anywhere resolves to the empty label"
        );
    }

    #[test]
    fn poll_interval_defaults_but_rejects_zero_and_invalid_values() {
        assert_eq!(
            parse_poll_interval_ms(None).unwrap(),
            DEFAULT_POLL_INTERVAL_MS
        );
        assert_eq!(parse_poll_interval_ms(Some(" 10 ")).unwrap(), 10);
        assert!(parse_poll_interval_ms(Some("0")).is_err());
        assert!(parse_poll_interval_ms(Some("fast")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn exclusive_session_lock_prevents_a_second_collector() {
        let dir = temp_dir("lock");
        let lock = acquire_exclusive(&dir).unwrap();
        assert!(acquire_exclusive(&dir).is_err());
        drop(lock);
        assert!(acquire_exclusive(&dir).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn session_id_never_uses_directory_components() {
        let id = session_id("kcd2", "20260816T101500Z");
        assert_eq!(id, "kcd2_20260816T101500Z");
        let id = session_id("", "20260816T101500Z");
        assert_eq!(id, "session_20260816T101500Z");
    }

    #[test]
    fn highest_batch_id_is_zero_for_empty_and_missing_dirs() {
        let dir = temp_dir("empty");
        assert_eq!(highest_batch_id(&dir).unwrap(), 0);
        assert!(highest_batch_id(Path::new("/nonexistent/path/xyz")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn highest_batch_id_parses_numerically_not_lexicographically() {
        let dir = temp_dir("numeric");
        for id in [1u32, 2, 10] {
            std::fs::write(dir.join(format!("{BATCH_PREFIX}{id}{BATCH_SUFFIX}")), b"x").unwrap();
        }
        // Lexicographic ordering would answer "2" here.
        assert_eq!(highest_batch_id(&dir).unwrap(), 10);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn highest_batch_id_ignores_unrelated_files() {
        let dir = temp_dir("unrelated");
        std::fs::write(dir.join("session_manifest.json"), b"{}").unwrap();
        std::fs::write(dir.join("canonical.csv"), b"x").unwrap();
        std::fs::write(
            dir.join(format!("{BATCH_PREFIX}notanumber{BATCH_SUFFIX}")),
            b"x",
        )
        .unwrap();
        std::fs::write(dir.join(format!("{BATCH_PREFIX}7{BATCH_SUFFIX}")), b"x").unwrap();
        assert_eq!(highest_batch_id(&dir).unwrap(), 7);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
