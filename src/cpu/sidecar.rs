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

/// Filename the collector looks for next to its own binary. Windows-only:
/// the Linux test build compiles this module for its parsers but never
/// spawns the helper, so an un-gated constant is dead code there.
#[cfg(windows)]
pub const SIDECAR_FILENAME: &str = "gaming-telemetry-cpu-sidecar.exe";

/// One sample line the sidecar prints on stdout. A missing key decodes as
/// `None` — "not measured" — which is also what an absent sensor means.
#[derive(Deserialize)]
struct Line {
    ts_ms: Option<i64>,
    tctl_c: Option<f64>,
    ccd1_c: Option<f64>,
    ccd2_c: Option<f64>,
    package_power_w: Option<f64>,
}

/// Parse one stdout line into a sample plus the producer timestamp the
/// sidecar stamped it with; implausible values degrade to `None` through the
/// same gates the HWiNFO and LHM-app feeds use. `None` here means "not a
/// sample line", which the caller skips rather than treating as a dead feed.
pub fn parse_sample(line: &str) -> Option<(CpuSample, Option<i64>)> {
    let line: Line = serde_json::from_str(line).ok()?;
    let sample = CpuSample {
        tctl_c: line.tctl_c.and_then(plausible_temp),
        ccd1_c: line.ccd1_c.and_then(plausible_temp),
        ccd2_c: line.ccd2_c.and_then(plausible_temp),
        package_power_w: line.package_power_w.and_then(plausible_power),
    };
    // Every measured field at exactly 0.0 is not a measurement: without the
    // PawnIO driver LHM's register reads come back zero-filled buffers that
    // surface as 0 C and 0 W, and the null-not-zero contract forbids feeding
    // a dataset fabricated constants. A genuinely idle CPU still reports
    // tens of watts and real heat, so no true sample is lost to the check.
    let all_zero = [
        sample.tctl_c,
        sample.ccd1_c,
        sample.ccd2_c,
        sample.package_power_w,
    ]
    .iter()
    .flatten()
    .all(|v| *v == 0.0);
    let sample = if all_zero { CpuSample::EMPTY } else { sample };
    Some((sample, line.ts_ms))
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
        /// Held open for the child's life: its watchdog thread treats stdin
        /// EOF as "the collector is gone" and exits, which is what reaps the
        /// sidecar when the collector dies without `Drop` (crash, Task
        /// Manager kill, console close). `Drop`'s own kill still runs first
        /// on a clean shutdown; this is the hard-exit backstop.
        _stdin: std::process::ChildStdin,
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
    ///
    /// The override is honoured only while the collector runs unelevated. An
    /// elevated collector spawning a path an unprivileged environment chose
    /// would hand administrator rights to whatever that variable points at, so
    /// the trusted locations are only the co-located ones.
    fn resolve_path() -> Option<PathBuf> {
        if let Some(path) = std::env::var_os("CPU_SIDECAR_PATH")
            .map(PathBuf::from)
            .filter(|p| !p.as_os_str().is_empty())
        {
            if elevated() {
                eprintln!(
                    "CPU telemetry: CPU_SIDECAR_PATH ignored — the collector is \
                     elevated, so only a sidecar next to its binary is trusted."
                );
            } else {
                return Some(path);
            }
        }
        let exe_dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
        [exe_dir.clone(), exe_dir.join("sidecar")]
            .into_iter()
            .map(|dir| dir.join(SIDECAR_FILENAME))
            .find(|p| p.is_file())
    }

    /// `true` when this process holds an elevated token. A failed query answers
    /// `true` — the env override is a development convenience, and declining to
    /// trust it costs a developer one stderr note while trusting it wrongly
    /// costs a privilege boundary.
    fn elevated() -> bool {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::Security::{
            GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
        };
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

        unsafe {
            let mut token = std::mem::zeroed();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return true;
            }
            let mut elevation = TOKEN_ELEVATION::default();
            let mut size = std::mem::size_of::<TOKEN_ELEVATION>() as u32;
            let ok = GetTokenInformation(
                token,
                TokenElevation,
                &mut elevation as *mut _ as *mut _,
                size,
                &mut size,
            );
            CloseHandle(token);
            ok != 0 && elevation.TokenIsElevated != 0
        }
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
            // stdin stays piped and is held in `Sidecar`: the child's watchdog
            // thread reads it and treats EOF — which the OS delivers even when
            // this process is killed outright — as the signal to exit.
            .stdin(Stdio::piped())
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
        let (Some(stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
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
        Some(Sidecar {
            child,
            shared,
            _stdin: stdin,
        })
    }

    /// A line whose `ts_ms` is older than this counts as buffered backlog, not
    /// a live reading — the same window `FEED_DEAD` allows a silent feed.
    /// Without it a reader that falls behind would stamp each old queued line
    /// alive at *consumption* time and serve stale values as fresh.
    const SAMPLE_STALE_MS: i64 = 10_000;

    /// Decode sample lines until the child's stdout closes. Liveness requires
    /// a sample that is actually usable — an all-null stream proves nothing
    /// (a running HWiNFO or LHM host would serve real data in its place), and
    /// a stale `ts_ms` only proves the pipe holds a backlog. A line that does
    /// not parse is skipped, not treated as a dead feed.
    fn read_loop(stdout: impl Read, shared: Arc<Mutex<FeedState>>) {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            let Some((sample, ts_ms)) = parse_sample(&line) else {
                continue;
            };
            let fresh = ts_ms.is_none_or(|ts| now_unix_ms().saturating_sub(ts) <= SAMPLE_STALE_MS);
            publish(&shared, fresh && sample != CpuSample::EMPTY, sample);
        }
    }

    fn now_unix_ms() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
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
        let (sample, ts) = parse_sample(
            r#"{"ts_ms":1763340000000,"tctl_c":61.5,"ccd1_c":68.0,"ccd2_c":55.0,"package_power_w":142.5}"#,
        )
        .expect("valid sample line");
        assert_eq!(sample.tctl_c, Some(61.5));
        assert_eq!(sample.ccd1_c, Some(68.0));
        assert_eq!(sample.ccd2_c, Some(55.0));
        assert_eq!(sample.package_power_w, Some(142.5));
        assert_eq!(ts, Some(1763340000000));
    }

    #[test]
    fn null_fields_stay_none_not_zero() {
        let (sample, _) = parse_sample(r#"{"ts_ms":1,"tctl_c":61.5}"#).unwrap();
        assert_eq!(sample.tctl_c, Some(61.5));
        assert_eq!(sample.ccd1_c, None);
        assert_eq!(sample.package_power_w, None);
    }

    /// The plausibility gates apply on this side too: a corrupt or malicious
    /// sidecar cannot put 9999 C into a column.
    #[test]
    fn implausible_values_become_none() {
        let (sample, _) = parse_sample(r#"{"tctl_c":9999.0,"package_power_w":-5.0}"#).unwrap();
        assert_eq!(sample.tctl_c, None);
        assert_eq!(sample.package_power_w, None);
    }

    /// The PawnIO-less failure mode: every register read returns a zero-filled
    /// buffer, which surfaces as exactly 0 C and 0 W on every field. That
    /// stream is EMPTY — unmeasured — not a real reading, so the probe falls
    /// through to the operator-run hosts instead of binding fabricated zeros.
    #[test]
    fn an_all_zero_sample_counts_as_unmeasured() {
        let (sample, _) = parse_sample(
            r#"{"ts_ms":1,"tctl_c":0.0,"ccd1_c":0.0,"ccd2_c":0.0,"package_power_w":0.0}"#,
        )
        .unwrap();
        assert_eq!(sample, CpuSample::EMPTY);
        // One real field keeps the rest of the zeros honest — a mixed sample
        // is not the all-zero fabrication pattern.
        let (mixed, _) =
            parse_sample(r#"{"tctl_c":61.5,"ccd1_c":0.0,"package_power_w":142.5}"#).unwrap();
        assert_eq!(mixed.ccd1_c, Some(0.0));
        assert_eq!(mixed.tctl_c, Some(61.5));
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
