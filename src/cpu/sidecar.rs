// SPDX-License-Identifier: MIT OR Apache-2.0

//! Bundled LibreHardwareMonitor sidecar feed.
//!
//! `gaming-telemetry-cpu-sidecar` is a small .NET helper built on
//! LibreHardwareMonitorLib (see `sidecar/`). The collector spawns it at capture
//! start and owns its lifecycle — no GUI to launch, no shared-memory option to
//! enable, no `/metrics` server to run, and no 12-hour licensing cliff. The
//! helper reads sensors through the signed PawnIO driver and prints one JSON
//! object per sample on stdout:
//!
//! ```text
//! {"ts_ms":1763340000000,"tctl_c":61.5,"ccd1_c":68.0,"ccd2_c":null,"package_power_w":142.5}
//! ```
//!
//! Diagnostics go to its stderr, forwarded here with a `[cpu-sidecar]` prefix.
//! A field is `null` when that sensor was not measured — the same
//! null-not-zero contract as every other source.

use serde::Deserialize;

use super::{CpuSample, plausible_power, plausible_temp};

/// Filename the collector looks for next to its own binary.
pub const SIDECAR_FILENAME: &str = "gaming-telemetry-cpu-sidecar.exe";

/// One sample line the sidecar prints on stdout. A missing key decodes as
/// `None` — "not measured" — which is also what an absent sensor means.
#[derive(Deserialize)]
struct Line {
    tctl_c: Option<f64>,
    ccd1_c: Option<f64>,
    ccd2_c: Option<f64>,
    package_power_w: Option<f64>,
}

/// Parse one stdout line into a sample; implausible values degrade to `None`
/// through the same gates the HWiNFO and LHM-app feeds use. `None` here means
/// "not a sample line", which the caller skips rather than treating as a dead
/// feed.
pub fn parse_sample(line: &str) -> Option<CpuSample> {
    let line: Line = serde_json::from_str(line).ok()?;
    Some(CpuSample {
        tctl_c: line.tctl_c.and_then(plausible_temp),
        ccd1_c: line.ccd1_c.and_then(plausible_temp),
        ccd2_c: line.ccd2_c.and_then(plausible_temp),
        package_power_w: line.package_power_w.and_then(plausible_power),
    })
}

#[cfg(windows)]
mod imp {
    use std::io::{BufRead, BufReader, Read};
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    use super::{SIDECAR_FILENAME, parse_sample};
    use crate::cpu::{CpuSample, FeedState, publish};
    use crate::privacy::redact_personal_path;

    /// `CREATE_NO_WINDOW`: the sidecar is a console exe; spawned by a
    /// console-less host it would otherwise pop a window every probe cycle.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    /// A spawned sidecar plus its decoded-sample mailbox. `child` is held so
    /// `Drop` can kill it — `std::process::Child`'s own drop does NOT
    /// terminate the process, so without an explicit kill an abandoned
    /// sidecar would leak past the capture it was spawned for.
    pub struct Sidecar {
        child: Child,
        shared: Arc<Mutex<FeedState>>,
    }

    impl Sidecar {
        /// The latest published sample once any line has arrived — used at
        /// connect time, before any freshness guarantee exists.
        pub fn published(&self) -> Option<CpuSample> {
            let shared = self.shared.lock().ok()?;
            shared.alive_at.is_some().then_some(shared.sample)
        }

        /// The latest sample while the feed has stayed fresh.
        pub fn fresh(&self, now: Instant, max_age: Duration) -> Option<CpuSample> {
            let shared = self.shared.lock().ok()?;
            shared
                .alive_at
                .is_some_and(|t| now.duration_since(t) <= max_age)
                .then_some(shared.sample)
        }

        /// `true` once the child has exited — nothing can produce again, so
        /// there is no point serving out a staleness window.
        pub fn exited(&mut self) -> bool {
            matches!(self.child.try_wait(), Ok(Some(_)))
        }
    }

    impl Drop for Sidecar {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    /// `true` when the operator asked that the sidecar never be spawned — the
    /// escape hatch that also proves the null-not-zero contract: with the
    /// sidecar "missing", columns stay null rather than fabricated.
    fn disabled() -> bool {
        std::env::var("CPU_SIDECAR_DISABLE")
            .map(|v| !v.trim().is_empty() && v.trim() != "0")
            .unwrap_or(false)
    }

    /// Locate the sidecar exe: `CPU_SIDECAR_PATH` when set (also returned when
    /// it points somewhere that does not exist, so the spawn error names what
    /// the operator configured), else next to the collector binary — directly,
    /// or under a `sidecar/` sibling directory.
    fn resolve_path() -> Option<PathBuf> {
        if let Some(path) = std::env::var_os("CPU_SIDECAR_PATH")
            .map(PathBuf::from)
            .filter(|p| !p.as_os_str().is_empty())
        {
            return Some(path);
        }
        let exe_dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
        [exe_dir.clone(), exe_dir.join("sidecar")]
            .into_iter()
            .map(|dir| dir.join(SIDECAR_FILENAME))
            .find(|p| p.is_file())
    }

    /// Spawn the sidecar with sample output piped back and diagnostics
    /// forwarded to the collector's stderr. `None` when disabled, absent, or
    /// unspawnable — the caller falls through to the next source.
    pub fn spawn() -> Option<Sidecar> {
        if disabled() {
            return None;
        }
        let path = resolve_path()?;
        spawn_child(&path, &[])
    }

    /// Spawn `path` wired to the feed mailbox; split from [`spawn`] so tests
    /// can exercise the pipe plumbing with a stand-in child.
    pub fn spawn_child(path: &Path, args: &[&str]) -> Option<Sidecar> {
        use std::os::windows::process::CommandExt;
        let mut child = match Command::new(path)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
        {
            Ok(child) => child,
            Err(error) => {
                eprintln!(
                    "CPU telemetry: could not spawn sidecar {}: {error}",
                    redact_personal_path(&path.display().to_string())
                );
                return None;
            }
        };
        let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
            let _ = child.kill();
            return None;
        };
        let shared = Arc::new(Mutex::new(FeedState::new()));
        // Two detached threads: samples and diagnostics each need a blocking
        // reader, and the child's lifetime bounds both — the pipes close when
        // it exits, ending each loop.
        let read_ok = thread::Builder::new()
            .name("cpu-sidecar-read".into())
            .spawn({
                let shared = Arc::clone(&shared);
                move || read_loop(stdout, shared)
            })
            .is_ok();
        let drain_ok = thread::Builder::new()
            .name("cpu-sidecar-stderr".into())
            .spawn(move || drain_loop(stderr))
            .is_ok();
        if !(read_ok && drain_ok) {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        Some(Sidecar { child, shared })
    }

    /// Decode sample lines until the child's stdout closes. A parsed line is
    /// liveness even when every field is null — the child is alive and
    /// updating, which is all the freshness stamp measures — while a line
    /// that does not parse is skipped, not treated as a dead feed.
    fn read_loop(stdout: impl Read, shared: Arc<Mutex<FeedState>>) {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if let Some(sample) = parse_sample(&line) {
                publish(&shared, true, sample);
            }
        }
    }

    /// Forward the child's stderr so "not elevated" / "PawnIO missing" reach
    /// the operator instead of dying inside a pipe.
    fn drain_loop(stderr: impl Read) {
        for line in BufReader::new(stderr).lines() {
            let Ok(line) = line else { break };
            eprintln!("[cpu-sidecar] {line}");
        }
    }
}

#[cfg(windows)]
pub use imp::{Sidecar, spawn};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_sample_line_yields_all_four_columns() {
        let sample = parse_sample(
            r#"{"ts_ms":1763340000000,"tctl_c":61.5,"ccd1_c":68.0,"ccd2_c":55.0,"package_power_w":142.5}"#,
        )
        .expect("valid sample line");
        assert_eq!(sample.tctl_c, Some(61.5));
        assert_eq!(sample.ccd1_c, Some(68.0));
        assert_eq!(sample.ccd2_c, Some(55.0));
        assert_eq!(sample.package_power_w, Some(142.5));
    }

    #[test]
    fn null_fields_stay_none_not_zero() {
        let sample = parse_sample(r#"{"ts_ms":1,"tctl_c":61.5}"#).unwrap();
        assert_eq!(sample.tctl_c, Some(61.5));
        assert_eq!(sample.ccd1_c, None);
        assert_eq!(sample.package_power_w, None);
    }

    /// The plausibility gates apply on this side too: a corrupt or malicious
    /// sidecar cannot put 9999 C into a column.
    #[test]
    fn implausible_values_become_none() {
        let sample = parse_sample(r#"{"tctl_c":9999.0,"package_power_w":-5.0}"#).unwrap();
        assert_eq!(sample.tctl_c, None);
        assert_eq!(sample.package_power_w, None);
    }

    #[test]
    fn non_sample_lines_are_skipped() {
        assert!(parse_sample("not json").is_none());
        assert!(parse_sample("").is_none());
    }

    /// The spawn/read/kill plumbing against a stand-in child: a `cmd` script
    /// emits one sample line then exits, so `published` must see it and
    /// `exited` must eventually report the death. Windows-only, like the
    /// plumbing itself. A script file rather than `cmd /c echo …`, because
    /// /c's own quote handling would mangle the JSON's embedded quotes.
    #[cfg(windows)]
    #[test]
    fn a_spawned_child_feeds_the_mailbox() {
        use std::path::Path;
        use std::time::{Duration, Instant};

        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-fixtures")
            .join(format!("sidecar_spawn_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("emit.cmd");
        std::fs::write(
            &script,
            "echo {\"tctl_c\":61.5,\"package_power_w\":142.5}\r\n",
        )
        .unwrap();

        let mut sidecar =
            super::imp::spawn_child(Path::new("cmd.exe"), &["/c", &script.to_string_lossy()])
                .expect("cmd spawns");
        let deadline = Instant::now() + Duration::from_secs(10);
        let sample = loop {
            if let Some(sample) = sidecar.published() {
                break sample;
            }
            assert!(Instant::now() < deadline, "child sample never arrived");
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(sample.tctl_c, Some(61.5));
        assert_eq!(sample.package_power_w, Some(142.5));
        while !sidecar.exited() {
            assert!(Instant::now() < deadline, "child never exited");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
