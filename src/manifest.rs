// SPDX-License-Identifier: MIT OR Apache-2.0

//! Versioned session manifest — the sidecar that makes a capture directory
//! self-describing.
//!
//! Written at session start so it exists during capture, and finalized on clean
//! shutdown with the end timestamp and timing-quality summary. Deliberately
//! records nothing personal: no usernames, home paths, Steam identifiers,
//! secrets, or wider machine inventory.

use crate::timing::TimingSummary;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Sidecar filename inside the session directory.
pub const MANIFEST_FILENAME: &str = "session_manifest.json";
/// Bump only on a breaking change to the field layout below.
pub const SCHEMA_VERSION: u32 = 1;

fn unknown() -> String {
    "unknown".to_owned()
}

/// Hardware identity of the capturing machine. Model names only — nothing that
/// identifies the operator.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostInfo {
    pub gpu_name: String,
    pub driver: String,
    pub cpu_model: String,
}

impl HostInfo {
    /// GPU fields come from the caller's existing NVML handle; the CPU model is
    /// read here. Every field degrades to `"unknown"` — host detection must never
    /// abort a capture.
    pub fn new(gpu_name: Option<String>, driver: Option<String>) -> Self {
        Self {
            gpu_name: gpu_name.unwrap_or_else(unknown),
            driver: driver.unwrap_or_else(unknown),
            cpu_model: read_cpu_model().unwrap_or_else(unknown),
        }
    }
}

/// Parse the `model name` line out of `/proc/cpuinfo` contents.
pub fn parse_cpu_model(contents: &str) -> Option<String> {
    contents
        .lines()
        .find_map(|line| {
            line.split_once(':')
                .filter(|(key, _)| key.trim() == "model name")
        })
        .map(|(_, value)| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn read_cpu_model() -> Option<String> {
    parse_cpu_model(&std::fs::read_to_string("/proc/cpuinfo").ok()?)
}

/// What was running during the capture.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Workload {
    pub class: String,
    pub label: String,
}

/// Metadata preserved for a collector process that previously wrote to this
/// session directory. It keeps resumed sessions reproducible without changing
/// the session-level identity fields.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PriorRun {
    pub run_started_at_utc: Option<DateTime<Utc>>,
    pub ended_at_utc: Option<DateTime<Utc>>,
    pub poll_interval_ms_requested: u64,
    pub collector_version: String,
    pub git_commit: String,
    pub host: HostInfo,
    pub workload: Workload,
    pub timing: Option<TimingSummary>,
}

impl Workload {
    pub fn new(label: &str) -> Self {
        Self::with_class(std::env::var("WORKLOAD_CLASS").ok(), label)
    }

    /// Class selection split from the environment lookup, so the rules can be
    /// tested without mutating process env — which is `unsafe` under edition 2024
    /// and races other tests in the same process.
    fn with_class(raw_class: Option<String>, label: &str) -> Self {
        let class = raw_class
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "gaming".to_owned());
        Self {
            class,
            label: label.to_owned(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionManifest {
    pub schema_version: u32,
    pub session_id: String,
    pub session_label: String,
    pub started_at_utc: DateTime<Utc>,
    /// Start time of this collector process. `started_at_utc` remains the
    /// stable identity timestamp for the overall session.
    #[serde(default)]
    pub run_started_at_utc: Option<DateTime<Utc>>,
    pub ended_at_utc: Option<DateTime<Utc>>,
    pub poll_interval_ms_requested: u64,
    pub collector_version: String,
    pub git_commit: String,
    /// How many collector processes have written into this directory. 0 on the
    /// first run.
    pub restart_count: u32,
    /// Number of prior processes that ended without finalizing the manifest.
    #[serde(default)]
    pub unclean_restart_count: u32,
    pub host: HostInfo,
    pub workload: Workload,
    /// Total Parquet batches that failed during this session. A nonzero
    /// value means timing sample_count can exceed rows available on disk.
    #[serde(default)]
    pub parquet_write_failures: u32,
    pub timing: Option<TimingSummary>,
    /// Metadata for every earlier collector process in this session.
    #[serde(default)]
    pub prior_runs: Vec<PriorRun>,
}

fn manifest_path(dir: &Path) -> PathBuf {
    dir.join(MANIFEST_FILENAME)
}

fn temp_path(dir: &Path) -> PathBuf {
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    dir.join(format!(
        "{MANIFEST_FILENAME}.{}.{}.{sequence}.tmp",
        std::process::id(),
        nanos
    ))
}

/// Remove manifest temporaries left behind by a process that died between
/// `create_new` and `rename`.
///
/// In-process failures clean up after themselves, but a SIGKILL (or a power loss)
/// in that window strands the file forever. A collector that restarts often would
/// otherwise accumulate them in the session directory indefinitely. Safe to run at
/// startup: the caller holds the exclusive session lock, so no live writer owns a
/// temporary here.
/// True only for the exact filename shape `temp_path` generates:
/// `session_manifest.json.<pid>.<nanos>.<sequence>.tmp`, all three numeric.
///
/// Deliberately not a `session_manifest.json*.tmp` glob. The sweep deletes files,
/// so it must recognise only what this module itself writes — an operator's
/// `session_manifest.json.backup.tmp` matches the loose pattern and is exactly the
/// kind of file that must survive.
fn is_generated_temporary(name: &str) -> bool {
    let Some(fields) = name
        .strip_prefix(MANIFEST_FILENAME)
        .and_then(|rest| rest.strip_suffix(".tmp"))
        .and_then(|rest| rest.strip_prefix('.'))
    else {
        return false;
    };
    let fields: Vec<&str> = fields.split('.').collect();
    fields.len() == 3
        && fields
            .iter()
            .all(|field| !field.is_empty() && field.bytes().all(|b| b.is_ascii_digit()))
}

/// What a sweep did, so the caller can report both halves.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SweepOutcome {
    pub removed: usize,
    /// Temporaries that matched but could not be deleted.
    pub failed: Vec<PathBuf>,
    /// Directory entries that could not be read at all.
    ///
    /// Each one may have been a stale temporary the sweep has now failed to
    /// reclaim, so a non-zero count means "swept, but not exhaustively" — which
    /// the caller must be able to say rather than reporting a clean sweep.
    pub unreadable_entries: usize,
}

/// Whether a directory entry is a generated manifest temporary the sweep
/// should act on.
enum StaleMatch {
    /// The name isn't a shape this module generates -- not the sweep's business.
    NotOurs,
    /// The name matches, but its type couldn't be confirmed (a transient or
    /// mounted-filesystem metadata error). Folding this into `NotOurs` would
    /// let a real stale temporary be silently skipped while the sweep still
    /// reports a clean pass.
    Unreadable,
    /// A confirmed regular file matching the generated shape.
    Match(PathBuf),
}

/// Classify `entry` against the generated-temporary name shape.
fn stale_temporary_path(entry: &std::fs::DirEntry) -> StaleMatch {
    let name = entry.file_name();
    let Some(name) = name.to_str() else {
        return StaleMatch::NotOurs;
    };
    if !is_generated_temporary(name) {
        return StaleMatch::NotOurs;
    }
    match entry.file_type() {
        Ok(kind) if kind.is_file() => StaleMatch::Match(entry.path()),
        Ok(_) => StaleMatch::NotOurs,
        Err(_) => StaleMatch::Unreadable,
    }
}

pub fn sweep_stale_temporaries(dir: &Path) -> Result<SweepOutcome> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        // A directory that does not exist yet simply has nothing to sweep.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(SweepOutcome::default());
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to enumerate {} while sweeping stale manifests",
                    crate::privacy::redact_personal_path(&dir.display().to_string())
                )
            });
        }
    };

    // A failed removal is not fatal -- stale debris wastes space but blocks
    // nothing, and refusing to start a capture over it would be worse -- but it is
    // returned rather than dropped, so it can be reported instead of recurring
    // silently on every restart.
    let mut outcome = SweepOutcome::default();
    for entry in entries {
        // `ReadDir` can fail per entry after the directory opened — an I/O error
        // on a mounted SESSION_DIR, say. Dropping those made a partial sweep
        // indistinguishable from a complete one.
        let Ok(entry) = entry else {
            outcome.unreadable_entries += 1;
            continue;
        };
        let path = match stale_temporary_path(&entry) {
            StaleMatch::NotOurs => continue,
            StaleMatch::Unreadable => {
                outcome.unreadable_entries += 1;
                continue;
            }
            StaleMatch::Match(path) => path,
        };
        match std::fs::remove_file(&path) {
            Ok(()) => outcome.removed += 1,
            Err(_) => outcome.failed.push(path),
        }
    }
    Ok(outcome)
}

impl SessionManifest {
    pub fn new(
        session_id: String,
        session_label: String,
        started_at_utc: DateTime<Utc>,
        poll_interval_ms_requested: u64,
        host: HostInfo,
    ) -> Self {
        let workload = Workload::new(&session_label);
        Self {
            schema_version: SCHEMA_VERSION,
            session_id,
            session_label,
            started_at_utc,
            run_started_at_utc: Some(started_at_utc),
            ended_at_utc: None,
            poll_interval_ms_requested,
            collector_version: crate::build_info::collector_version().to_owned(),
            git_commit: crate::build_info::git_sha(),
            restart_count: 0,
            unclean_restart_count: 0,
            host,
            workload,
            parquet_write_failures: 0,
            timing: None,
            prior_runs: Vec::new(),
        }
    }

    /// Read an existing manifest. An absent file is normal; malformed or
    /// unsupported manifests must stop the collector rather than being silently
    /// overwritten as a new session.
    pub fn load(dir: &Path) -> Result<Option<Self>> {
        let path = manifest_path(dir);
        let contents = match std::fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "failed to read {}",
                        crate::privacy::redact_personal_path(&path.display().to_string())
                    )
                });
            }
        };
        let manifest: Self = serde_json::from_str(&contents).with_context(|| {
            format!(
                "failed to parse {}",
                crate::privacy::redact_personal_path(&path.display().to_string())
            )
        })?;
        anyhow::ensure!(
            manifest.schema_version == SCHEMA_VERSION,
            "unsupported session manifest schema_version {}; expected {}",
            manifest.schema_version,
            SCHEMA_VERSION
        );
        Ok(Some(manifest))
    }

    /// Build the manifest for a run, adopting any prior manifest in the same
    /// directory.
    ///
    /// Restarting a collector into an existing session directory continues that
    /// session rather than starting a new one: its identity, workload, label,
    /// and cumulative persistence failures are preserved. Metadata for the
    /// prior process is appended to `prior_runs`, and an unfinished prior
    /// process increments `unclean_restart_count`. `ended_at_utc` is cleared
    /// because the session is live again, and `restart_count` increments.
    /// Run-specific values (version, git SHA, host, poll interval) describe
    /// the current process.
    pub fn load_or_new(
        dir: &Path,
        session_id: String,
        session_label: String,
        started_at_utc: DateTime<Utc>,
        poll_interval_ms_requested: u64,
        host: HostInfo,
    ) -> Result<Self> {
        let mut manifest = Self::new(
            session_id,
            session_label,
            started_at_utc,
            poll_interval_ms_requested,
            host,
        );
        if let Some(previous) = Self::load(dir)? {
            let prior_run = PriorRun::from(&previous);
            manifest.session_id = previous.session_id;
            manifest.started_at_utc = previous.started_at_utc;
            manifest.restart_count = previous.restart_count.saturating_add(1);
            manifest.unclean_restart_count = previous
                .unclean_restart_count
                .saturating_add(u32::from(previous.ended_at_utc.is_none()));
            manifest.session_label = previous.session_label;
            manifest.workload = previous.workload;
            manifest.parquet_write_failures = previous.parquet_write_failures;
            manifest.prior_runs = previous.prior_runs;
            manifest.prior_runs.push(prior_run);
        }
        Ok(manifest)
    }

    /// Stamp when collection stopped and attach the timing/persistence summary.
    pub fn finalize(
        &mut self,
        ended_at_utc: DateTime<Utc>,
        timing: TimingSummary,
        parquet_write_failures: u32,
    ) {
        self.ended_at_utc = Some(ended_at_utc);
        self.timing = Some(timing);
        self.parquet_write_failures = self
            .parquet_write_failures
            .saturating_add(parquet_write_failures);
    }

    /// Serialize to `session_manifest.json` via a temp file and rename.
    ///
    /// The rename is atomic, so a kill mid-write can leave the previous manifest
    /// or the new one — never a truncated file that fails to parse.
    pub fn write_atomic(&self, dir: &Path) -> Result<()> {
        let tmp = temp_path(dir);
        let json = serde_json::to_string_pretty(self).context("failed to serialize manifest")?;
        write_temp_manifest(&tmp, &json)?;
        publish_manifest(&tmp, dir)
    }
}

fn write_temp_manifest(tmp: &Path, json: &str) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(tmp)
        .with_context(|| {
            format!(
                "failed to create {}",
                crate::privacy::redact_personal_path(&tmp.display().to_string())
            )
        })?;
    if let Err(error) = file
        .write_all(json.as_bytes())
        .and_then(|_| file.sync_all())
    {
        drop(file);
        let _ = std::fs::remove_file(tmp);
        return Err(error).with_context(|| {
            format!(
                "failed to write {}",
                crate::privacy::redact_personal_path(&tmp.display().to_string())
            )
        });
    }
    Ok(())
}

fn publish_manifest(tmp: &Path, dir: &Path) -> Result<()> {
    if let Err(error) = std::fs::rename(tmp, manifest_path(dir)) {
        let _ = std::fs::remove_file(tmp);
        return Err(error).with_context(|| {
            format!(
                "failed to finalize {}",
                crate::privacy::redact_personal_path(&manifest_path(dir).display().to_string())
            )
        });
    }
    File::open(dir)
        .and_then(|directory| directory.sync_all())
        .with_context(|| {
            format!(
                "failed to persist manifest rename in {}",
                crate::privacy::redact_personal_path(&dir.display().to_string())
            )
        })
}

impl From<&SessionManifest> for PriorRun {
    fn from(manifest: &SessionManifest) -> Self {
        Self {
            run_started_at_utc: manifest.run_started_at_utc,
            ended_at_utc: manifest.ended_at_utc,
            poll_interval_ms_requested: manifest.poll_interval_ms_requested,
            collector_version: manifest.collector_version.clone(),
            git_commit: manifest.git_commit.clone(),
            host: manifest.host.clone(),
            workload: manifest.workload.clone(),
            timing: manifest.timing.clone(),
        }
    }
}

#[cfg(test)]
#[path = "manifest_failure_tests.rs"]
mod failure_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timing::TimingStats;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-fixtures")
            .join(format!(
                "manifest_{tag}_{}_{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn fixture(session_id: &str) -> SessionManifest {
        SessionManifest::new(
            session_id.to_owned(),
            "kcd2".to_owned(),
            Utc::now(),
            5,
            HostInfo::new(Some("RTX 5080".to_owned()), Some("580.00".to_owned())),
        )
    }

    #[test]
    fn parse_cpu_model_extracts_the_model_name_line() {
        let cpuinfo = "processor\t: 0\nvendor_id\t: AuthenticAMD\nmodel name\t: AMD Ryzen 9 7950X 16-Core Processor\ncpu MHz\t: 4500.000\n";
        assert_eq!(
            parse_cpu_model(cpuinfo).as_deref(),
            Some("AMD Ryzen 9 7950X 16-Core Processor")
        );
    }

    #[test]
    fn parse_cpu_model_handles_missing_or_empty_values() {
        assert_eq!(parse_cpu_model(""), None);
        assert_eq!(parse_cpu_model("processor\t: 0\nflags\t: fpu vme\n"), None);
        assert_eq!(parse_cpu_model("model name\t:   \n"), None);
        // A value containing a colon must not be truncated at the second one.
        assert_eq!(
            parse_cpu_model("model name\t: Weird: CPU v2\n").as_deref(),
            Some("Weird: CPU v2")
        );
    }

    #[test]
    fn host_info_falls_back_to_unknown() {
        let host = HostInfo::new(None, None);
        assert_eq!(host.gpu_name, "unknown");
        assert_eq!(host.driver, "unknown");
        assert!(!host.cpu_model.is_empty());
    }

    /// A SIGKILL between `create_new` and `rename` strands a temporary that no
    /// in-process cleanup can reach. Startup must reclaim it.
    #[test]
    fn sweep_removes_stale_temporaries_but_spares_real_files() {
        let dir = temp_dir("sweep");
        let stale_a = dir.join(format!("{MANIFEST_FILENAME}.999.123.0.tmp"));
        let stale_b = dir.join(format!("{MANIFEST_FILENAME}.998.456.1.tmp"));
        let manifest = dir.join(MANIFEST_FILENAME);
        let unrelated = dir.join("canonical.csv");
        let other_tmp = dir.join("something_else.tmp");
        // An operator's own backup matches a loose `*.tmp` glob and must survive.
        let backup = dir.join(format!("{MANIFEST_FILENAME}.backup.tmp"));
        for path in [
            &stale_a, &stale_b, &manifest, &unrelated, &other_tmp, &backup,
        ] {
            std::fs::write(path, b"x").unwrap();
        }

        let outcome = sweep_stale_temporaries(&dir).unwrap();
        assert_eq!(outcome.removed, 2);
        assert!(outcome.failed.is_empty());
        assert!(!stale_a.exists() && !stale_b.exists());
        assert!(
            manifest.exists() && unrelated.exists() && other_tmp.exists() && backup.exists(),
            "the sweep must only claim the names it generates"
        );

        // Idempotent: a second startup finds nothing left to do.
        assert_eq!(sweep_stale_temporaries(&dir).unwrap().removed, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Only the generated `<pid>.<nanos>.<sequence>` shape, all numeric.
    #[test]
    fn only_generated_temporary_names_are_swept() {
        for name in [
            "session_manifest.json.1.2.3.tmp",
            "session_manifest.json.999999.1788746199000000000.42.tmp",
        ] {
            assert!(is_generated_temporary(name), "{name} should match");
        }
        for name in [
            "session_manifest.json.backup.tmp",  // an operator's backup
            "session_manifest.json.tmp",         // no fields
            "session_manifest.json.1.2.tmp",     // too few fields
            "session_manifest.json.1.2.3.4.tmp", // too many fields
            "session_manifest.json.1.2.x.tmp",   // non-numeric field
            "session_manifest.json.1..3.tmp",    // empty field
            "session_manifest.json",             // the real manifest
            "other.json.1.2.3.tmp",              // a different file
        ] {
            assert!(!is_generated_temporary(name), "{name} must be spared");
        }
    }

    /// A temporary that cannot be deleted is returned, not silently dropped --
    /// otherwise it recurs on every restart with no diagnostic.
    #[cfg(unix)]
    #[test]
    fn undeletable_temporaries_are_reported() {
        use std::os::unix::fs::PermissionsExt;

        let dir = temp_dir("sweep_fail");
        let guarded = dir.join("guarded");
        std::fs::create_dir_all(&guarded).unwrap();
        let stuck = guarded.join(format!("{MANIFEST_FILENAME}.1.2.3.tmp"));
        std::fs::write(&stuck, b"x").unwrap();
        // Removing a directory entry needs write permission on the directory.
        std::fs::set_permissions(&guarded, std::fs::Permissions::from_mode(0o555)).unwrap();

        let outcome = sweep_stale_temporaries(&guarded).unwrap();
        // Running as root defeats the permission bits, so only assert when it took.
        if outcome.removed == 0 {
            assert_eq!(outcome.failed, vec![stuck], "the failure must be surfaced");
        }

        let _ = std::fs::set_permissions(&guarded, std::fs::Permissions::from_mode(0o755));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sweep_of_a_missing_directory_is_not_an_error() {
        assert_eq!(
            sweep_stale_temporaries(Path::new("/nonexistent/session/dir")).unwrap(),
            SweepOutcome::default()
        );
    }

    /// `WORKLOAD_CLASS` segments the training mix downstream; only its default
    /// was previously exercised.
    #[test]
    fn workload_class_comes_from_the_environment_with_a_gaming_default() {
        assert_eq!(Workload::with_class(None, "kcd2").class, "gaming");
        assert_eq!(
            Workload::with_class(Some("  benchmark  ".to_owned()), "kcd2").class,
            "benchmark",
            "the value is trimmed"
        );
        assert_eq!(
            Workload::with_class(Some("   ".to_owned()), "kcd2").class,
            "gaming",
            "a whitespace-only value falls back to the default"
        );
        assert_eq!(Workload::with_class(None, "kcd2").label, "kcd2");
    }

    #[test]
    fn manifest_round_trips_through_json() {
        let manifest = fixture("kcd2_20260816");
        let json = serde_json::to_string(&manifest).unwrap();
        let parsed: SessionManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, manifest);
        assert_eq!(parsed.schema_version, 1);
        assert_eq!(parsed.ended_at_utc, None);
        assert_eq!(parsed.timing, None);
        assert_eq!(parsed.restart_count, 0);
        assert_eq!(parsed.unclean_restart_count, 0);
        assert_eq!(parsed.prior_runs, Vec::new());
        assert_eq!(parsed.workload.class, "gaming");
        assert_eq!(parsed.workload.label, "kcd2");
    }

    #[test]
    fn finalize_sets_end_time_and_timing() {
        let dir = temp_dir("finalize");
        let mut manifest = fixture("s2");
        let mut stats = TimingStats::new(5);
        stats.record(std::time::Instant::now());

        manifest.finalize(Utc::now(), stats.summary(), 0);
        manifest.write_atomic(&dir).unwrap();

        let reloaded = SessionManifest::load(&dir).unwrap().unwrap();
        assert!(reloaded.ended_at_utc.is_some());
        let timing = reloaded.timing.expect("timing summary should be attached");
        assert_eq!(timing.poll_interval_ms_requested, 5);
        assert_eq!(timing.sample_count, 1);
        assert_eq!(reloaded.parquet_write_failures, 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn restart_preserves_identity_and_bumps_count() {
        let dir = temp_dir("restart");
        let mut first = fixture("original_id");
        let original_start = first.started_at_utc;
        first.parquet_write_failures = 2;
        first.finalize(Utc::now(), TimingStats::new(5).summary(), 1);
        first.write_atomic(&dir).unwrap();

        // A later process resolves a different id/start, but the directory already
        // has a session in it.
        let second = SessionManifest::load_or_new(
            &dir,
            "some_other_id".to_owned(),
            "different_label".to_owned(),
            Utc::now(),
            5,
            HostInfo::new(None, None),
        )
        .unwrap();

        assert_eq!(
            second.session_id, "original_id",
            "identity must be preserved"
        );
        assert_eq!(
            second.started_at_utc, original_start,
            "session start must be preserved across restarts"
        );
        assert_eq!(second.restart_count, 1);
        assert_eq!(second.unclean_restart_count, 0);
        assert_eq!(second.session_label, "kcd2");
        assert_eq!(second.workload.label, "kcd2");
        assert_eq!(second.parquet_write_failures, 3);
        assert_eq!(second.prior_runs.len(), 1);
        let prior = &second.prior_runs[0];
        assert_eq!(prior.poll_interval_ms_requested, 5);
        assert_eq!(prior.workload.label, "kcd2");
        assert_eq!(prior.ended_at_utc, first.ended_at_utc);
        assert_eq!(
            second.ended_at_utc, None,
            "a restarted session is live again"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn repeated_restart_accumulates_prior_runs() {
        let dir = temp_dir("repeated_restart");
        fixture("original_id").write_atomic(&dir).unwrap();
        let second = SessionManifest::load_or_new(
            &dir,
            "ignored".to_owned(),
            "kcd2".to_owned(),
            Utc::now(),
            5,
            HostInfo::new(None, None),
        )
        .unwrap();
        second.write_atomic(&dir).unwrap();
        let third = SessionManifest::load_or_new(
            &dir,
            "ignored".to_owned(),
            "kcd2".to_owned(),
            Utc::now(),
            5,
            HostInfo::new(None, None),
        )
        .unwrap();
        assert_eq!(third.restart_count, 2);
        assert_eq!(third.prior_runs.len(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn restart_records_an_unclean_prior_process() {
        let dir = temp_dir("unclean_restart");
        let first = fixture("original_id");
        first.write_atomic(&dir).unwrap();

        let second = SessionManifest::load_or_new(
            &dir,
            "ignored".to_owned(),
            "kcd2".to_owned(),
            Utc::now(),
            50,
            HostInfo::new(None, None),
        )
        .unwrap();

        assert_eq!(second.unclean_restart_count, 1);
        assert_eq!(second.prior_runs.len(), 1);
        assert_eq!(second.prior_runs[0].ended_at_utc, None);
        assert_eq!(second.prior_runs[0].poll_interval_ms_requested, 5);
        assert_eq!(second.poll_interval_ms_requested, 50);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_or_new_starts_fresh_when_directory_is_empty_but_rejects_corruption() {
        let dir = temp_dir("fresh");
        let fresh = SessionManifest::load_or_new(
            &dir,
            "new_id".to_owned(),
            "re2r".to_owned(),
            Utc::now(),
            5,
            HostInfo::new(None, None),
        )
        .unwrap();
        assert_eq!(fresh.session_id, "new_id");
        assert_eq!(fresh.restart_count, 0);

        // A truncated manifest must not be replaced silently: the existing data
        // could be a real, interrupted session that needs operator recovery.
        std::fs::write(dir.join(MANIFEST_FILENAME), b"{\"schema_vers").unwrap();
        assert!(
            SessionManifest::load_or_new(
                &dir,
                "new_id2".to_owned(),
                "re2r".to_owned(),
                Utc::now(),
                5,
                HostInfo::new(None, None),
            )
            .is_err()
        );

        std::fs::write(
            dir.join(MANIFEST_FILENAME),
            serde_json::json!({ "schema_version": SCHEMA_VERSION + 1 }).to_string(),
        )
        .unwrap();
        assert!(SessionManifest::load(&dir).is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
