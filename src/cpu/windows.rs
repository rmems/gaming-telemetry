// SPDX-License-Identifier: MIT OR Apache-2.0

//! Windows `CpuMonitor`: CPU telemetry through a running sensor host.
//!
//! Probe order is HWiNFO64 shared memory, then LibreHardwareMonitor's
//! `/metrics` server, then nothing — matching how much setup each asks of the
//! operator and how fast each source can be read at capture tick rates. A
//! monitor that found nothing keeps re-probing so a sensor host started
//! mid-session is picked up without a collector restart.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::{CpuSample, hwinfo, lhm};

/// A monitor with no source re-probes at this interval, so an operator who
/// starts HWiNFO or LHM mid-session still gets CPU columns for the rest of it.
const PROBE_RETRY: Duration = Duration::from_secs(30);

/// How long a HWiNFO `last_update` stamp may go unchanged before the feed is
/// treated as frozen — HWiNFO exited, or the free edition's shared-memory
/// window elapsed. Generous relative to HWiNFO's fastest sensor poll so a
/// merely slow update is not misread as a dead feed.
const HWINFO_STALE: Duration = Duration::from_secs(10);

/// A frozen HWiNFO feed waits this long before the monitor looks for a
/// different source, so a brief HWiNFO hiccup does not flap between backends.
const HWINFO_GIVE_UP: Duration = Duration::from_secs(30);

/// Interval between `/metrics` fetches on the LHM worker thread. LHM
/// refreshes its sensor tree roughly once a second; polling faster buys
/// nothing and hammers its HTTP server.
const LHM_REFRESH: Duration = Duration::from_millis(500);

/// A `/metrics` sample older than this counts as not measured.
const LHM_STALE_MS: i64 = 10_000;

/// How long the LHM worker may go without a completed fetch before the source
/// is abandoned and another probe is tried — the same patience the HWiNFO
/// path allows a frozen feed.
const LHM_DEAD: Duration = Duration::from_secs(10);

/// Tracks the energy-independent sensor host selected at probe time.
///
/// Two pieces of sensor work block on other processes — attaching to a
/// source, and each LHM HTTP round-trip — and `poll` runs inside a 5 ms
/// capture tick ahead of the GPU read. Both run on detached threads so a
/// hung sensor host stalls only its own worker, never a capture tick.
pub struct CpuMonitor {
    source: Source,
    next_probe_at: Instant,
    /// Receives the next probe's result while it runs detached.
    probe_rx: Option<mpsc::Receiver<ProbeResult>>,
}

enum Source {
    Hwinfo(HwinfoSource),
    Lhm(LhmSource),
    Disconnected,
}

/// A completed probe handed back from the detached probe thread. The handles
/// it can wrap (a mapped view, a mutex) name process-global kernel objects —
/// not thread-affine — and the value has exactly one owner at a time, so
/// moving ownership between threads is safe.
struct ProbeResult(Source, Option<&'static str>);
unsafe impl Send for ProbeResult {}

struct HwinfoSource {
    shm: hwinfo::SharedMemory,
    /// Last seen `last_update` stamp and when it last changed.
    stamp: i64,
    advanced_at: Instant,
    /// When the current frozen stretch began, if any.
    frozen_since: Option<Instant>,
}

struct LhmSource {
    /// Written by the fetch worker, read by capture ticks.
    shared: Arc<Mutex<LhmShared>>,
    /// Asked on drop so a superseded worker exits instead of fetching forever.
    stop: Arc<AtomicBool>,
}

struct LhmShared {
    /// The most recent fetch's sample; `EMPTY` after a failed one.
    sample: CpuSample,
    /// When the last *successful* fetch finished.
    succeeded_at: Instant,
}

impl Default for CpuMonitor {
    fn default() -> Self {
        Self::new()
    }
}

impl CpuMonitor {
    /// Probe for a sensor host, reporting on stderr what was found — or, with
    /// none, exactly what the operator can run. The report matters as much here
    /// as on Linux: without it, an empty CPU column for a whole session is
    /// indistinguishable from a broken build.
    pub fn new() -> Self {
        let now = Instant::now();
        let ProbeResult(source, name) = probe(now);
        let monitor = Self {
            source,
            next_probe_at: now + PROBE_RETRY,
            probe_rx: None,
        };
        match name {
            Some(name) => eprintln!("CPU telemetry via {name}."),
            None => eprintln!(
                "CPU telemetry unavailable: no supported Windows sensor host found. \
                 Run HWiNFO64 with \"Shared Memory Support\" enabled (Settings → \
                 General/User Interface), or LibreHardwareMonitor with its remote \
                 web server running (Options → Remote Web Server → Run, port \
                 8085). The cpu_* columns will be empty; probing again every \
                 {}s.",
                PROBE_RETRY.as_secs()
            ),
        }
        monitor
    }

    /// A monitor bound to no source; every reading is `None`.
    #[cfg(test)]
    fn disconnected() -> Self {
        Self {
            source: Source::Disconnected,
            next_probe_at: Instant::now() + Duration::from_secs(3600),
            probe_rx: None,
        }
    }

    /// Read every CPU sensor for this tick.
    pub fn poll(&mut self) -> CpuSample {
        let now = Instant::now();
        match &mut self.source {
            Source::Hwinfo(src) => match src.poll(now) {
                Some(sample) => sample,
                None => {
                    if src.gave_up(now) {
                        // The feed froze and stayed frozen (HWiNFO exited, or
                        // the free edition's 12-hour shared-memory window
                        // closed). Re-probe: LHM may be running, or HWiNFO may
                        // come back under a fresh mapping.
                        eprintln!(
                            "HWiNFO64 sensor feed stopped updating; CPU columns \
                             going empty while another source is probed."
                        );
                        self.source = Source::Disconnected;
                        self.next_probe_at = now;
                    }
                    CpuSample::EMPTY
                }
            },
            Source::Lhm(src) => match src.poll(now) {
                Some(sample) => sample,
                None => {
                    // The server stopped answering (exited, or stopped its web
                    // server). Re-probe: HWiNFO may have appeared meanwhile.
                    eprintln!(
                        "LibreHardwareMonitor /metrics stopped answering; CPU \
                         columns going empty while another source is probed."
                    );
                    self.source = Source::Disconnected;
                    self.next_probe_at = now;
                    CpuSample::EMPTY
                }
            },
            Source::Disconnected => {
                self.drive_probe(now);
                CpuSample::EMPTY
            }
        }
    }

    /// Advance the detached probe, if any, and start one when due. Never
    /// blocks the tick on loopback TCP.
    fn drive_probe(&mut self, now: Instant) {
        if let Some(rx) = &self.probe_rx {
            match rx.try_recv() {
                Ok(ProbeResult(source, name)) => {
                    self.probe_rx = None;
                    self.source = source;
                    if let Some(name) = name {
                        eprintln!("CPU telemetry via {name}.");
                    }
                }
                Err(TryRecvError::Empty) => {}
                // A probe thread that died before answering is retried at the
                // next scheduled probe, not in a hot loop.
                Err(TryRecvError::Disconnected) => self.probe_rx = None,
            }
        } else if now >= self.next_probe_at {
            self.next_probe_at = now + PROBE_RETRY;
            let (tx, rx) = mpsc::channel();
            thread::spawn(move || {
                let _ = tx.send(probe(Instant::now()));
            });
            self.probe_rx = Some(rx);
        }
    }
}

/// Try each source in probe order; the result names what was found for
/// logging. Runs on the detached probe thread (or once synchronously at
/// startup) — it can take a full HTTP round-trip and must not run inside a
/// capture tick.
fn probe(now: Instant) -> ProbeResult {
    if let Some(src) = HwinfoSource::connect(now) {
        return ProbeResult(Source::Hwinfo(src), Some("HWiNFO64 shared memory"));
    }
    if let Some(src) = LhmSource::connect(now) {
        return ProbeResult(
            Source::Lhm(src),
            Some("LibreHardwareMonitor /metrics (loopback:8085)"),
        );
    }
    ProbeResult(Source::Disconnected, None)
}

impl HwinfoSource {
    /// Attach only when the region is present, parses, and is still live —
    /// a stale leftover region is no better than none. Both failure modes are
    /// checked: a dead producer leaves the mutex unheld, and a frozen feed
    /// (the free edition's 12-hour cutoff) leaves the region readable with an
    /// old `last_update`. Without the age check a frozen mapping would be
    /// re-adopted on every re-probe and LHM would never get a turn.
    fn connect(now: Instant) -> Option<Self> {
        if !hwinfo::SharedMemory::producer_alive() {
            return None;
        }
        let shm = hwinfo::SharedMemory::open()?;
        let buf = shm.snapshot()?;
        let scan = hwinfo::scan(&buf)?;
        let age_secs = now_unix_secs() - stamp_unix_secs(scan.stamp);
        if age_secs > HWINFO_STALE.as_secs() as i64 {
            return None;
        }
        let stamp = scan.stamp;
        let mut src = Self {
            shm,
            stamp,
            advanced_at: now,
            frozen_since: None,
        };
        // Prove the feed produces a sample on connect; `scan` succeeded but
        // `select` may still find nothing on a non-CPU host, in which case the
        // LHM fallback is never reached while every column stays null.
        if src.poll(now)? == CpuSample::EMPTY {
            return None;
        }
        Some(src)
    }

    /// `Some` with this tick's sample while the feed advances, `None` once it
    /// has been frozen longer than [`HWINFO_STALE`].
    fn poll(&mut self, now: Instant) -> Option<CpuSample> {
        let scan = hwinfo::scan(&self.shm.snapshot()?)?;
        if scan.stamp != self.stamp {
            self.stamp = scan.stamp;
            self.advanced_at = now;
            self.frozen_since = None;
        } else if now.duration_since(self.advanced_at) > HWINFO_STALE {
            self.frozen_since.get_or_insert(now);
            return None;
        }
        Some(hwinfo::select(&scan))
    }

    /// Whether a frozen feed has stayed frozen long enough to abandon. A dead
    /// producer is abandoned immediately — the mapping object persists only
    /// because this process holds it open, and no amount of waiting revives it.
    fn gave_up(&self, now: Instant) -> bool {
        if !hwinfo::SharedMemory::producer_alive() {
            return true;
        }
        self.frozen_since
            .is_some_and(|since| now.duration_since(since) > HWINFO_GIVE_UP)
    }
}

impl LhmSource {
    /// One synchronous fetch proves the listener is actually LHM — a service
    /// that merely shares port 8085 is not a source — then hands fetching to
    /// a worker so a hung server never stalls a capture tick.
    fn connect(now: Instant) -> Option<Self> {
        let sample = fetch_sample()?;
        if sample == CpuSample::EMPTY {
            return None;
        }
        let shared = Arc::new(Mutex::new(LhmShared {
            sample,
            succeeded_at: now,
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let worker_shared = Arc::clone(&shared);
        let worker_stop = Arc::clone(&stop);
        thread::Builder::new()
            .name("cpu-lhm-fetch".into())
            .spawn(move || {
                while !worker_stop.load(Ordering::Relaxed) {
                    if let Ok(mut shared) = worker_shared.lock() {
                        match fetch_sample() {
                            Some(sample) => {
                                shared.sample = sample;
                                shared.succeeded_at = Instant::now();
                            }
                            // A failed fetch is not a stale value to keep
                            // serving — the columns go null until the server
                            // answers again.
                            None => shared.sample = CpuSample::EMPTY,
                        }
                    }
                    thread::sleep(LHM_REFRESH);
                }
            })
            .ok()?;
        Some(Self { shared, stop })
    }

    /// `Some` with the worker's latest sample while fetches keep completing;
    /// `None` once the worker has gone [`LHM_DEAD`] without a success.
    fn poll(&mut self, now: Instant) -> Option<CpuSample> {
        let shared = self.shared.lock().ok()?;
        (now.duration_since(shared.succeeded_at) <= LHM_DEAD).then_some(shared.sample)
    }
}

impl Drop for LhmSource {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Fetch and pick one `/metrics` sample; `None` when the server is absent,
/// stalled, or replies with nothing usable.
fn fetch_sample() -> Option<CpuSample> {
    let body = lhm::fetch(lhm::DEFAULT_ADDR)?;
    Some(lhm::select(&lhm::parse(&body), now_unix_ms(), LHM_STALE_MS))
}

/// `last_update` is a unix timestamp, but its units are not documented
/// precisely; normalise to seconds whether a given build writes s or ms.
fn stamp_unix_secs(stamp: i64) -> i64 {
    if stamp.abs() >= 1_000_000_000_000 {
        stamp / 1_000
    } else {
        stamp
    }
}

fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn now_unix_secs() -> i64 {
    now_unix_ms() / 1_000
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Same guarantee as the sysfs backend: no sensor host means no data, not
    /// zeros.
    #[test]
    fn a_monitor_without_a_source_reports_nothing_measured() {
        let mut monitor = CpuMonitor::disconnected();
        assert_eq!(monitor.poll(), CpuSample::EMPTY);
    }
}
