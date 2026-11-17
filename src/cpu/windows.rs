// SPDX-License-Identifier: MIT OR Apache-2.0

//! Windows `CpuMonitor`: CPU telemetry through a running sensor host.
//!
//! Probe order is HWiNFO64 shared memory, then LibreHardwareMonitor's
//! `/metrics` server, then nothing — matching how much setup each asks of the
//! operator and how fast each source can be read at capture tick rates. A
//! monitor that found nothing keeps re-probing so a sensor host started
//! mid-session is picked up without a collector restart.

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

/// Minimum interval between `/metrics` fetches. LHM refreshes its sensor tree
/// roughly once a second; polling faster buys nothing and hammers its HTTP
/// server during a 5 ms capture tick.
const LHM_REFRESH: Duration = Duration::from_millis(500);

/// A `/metrics` sample older than this counts as not measured.
const LHM_STALE_MS: i64 = 10_000;

/// Tracks the energy-independent sensor host selected at probe time.
pub struct CpuMonitor {
    source: Source,
    next_probe_at: Instant,
}

enum Source {
    Hwinfo(HwinfoSource),
    Lhm(LhmSource),
    Disconnected,
}

struct HwinfoSource {
    shm: hwinfo::SharedMemory,
    /// Last seen `last_update` stamp and when it last changed.
    stamp: i64,
    advanced_at: Instant,
    /// When the current frozen stretch began, if any.
    frozen_since: Option<Instant>,
}

struct LhmSource {
    /// Last fetch attempt — successes and failures both rate-limit.
    fetched_at: Instant,
    sample: CpuSample,
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
        let mut monitor = Self {
            source: Source::Disconnected,
            next_probe_at: now,
        };
        match monitor.probe(now) {
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
            Source::Lhm(src) => src.poll(now),
            Source::Disconnected => {
                if now >= self.next_probe_at
                    && let Some(name) = self.probe(now)
                {
                    eprintln!("CPU telemetry via {name}.");
                }
                CpuSample::EMPTY
            }
        }
    }

    /// Try each source in probe order; on success the monitor switches to it.
    /// Returns the source name for logging.
    fn probe(&mut self, now: Instant) -> Option<&'static str> {
        self.next_probe_at = now + PROBE_RETRY;
        if let Some(src) = HwinfoSource::connect(now) {
            self.source = Source::Hwinfo(src);
            return Some("HWiNFO64 shared memory");
        }
        if let Some(src) = LhmSource::connect(now) {
            self.source = Source::Lhm(src);
            return Some("LibreHardwareMonitor /metrics (loopback:8085)");
        }
        self.source = Source::Disconnected;
        None
    }
}

impl HwinfoSource {
    /// Attach only when the region is present, parses, and is still advancing —
    /// a stale leftover region is no better than none.
    fn connect(now: Instant) -> Option<Self> {
        let shm = hwinfo::SharedMemory::open()?;
        let buf = shm.snapshot()?;
        let scan = hwinfo::scan(&buf)?;
        let stamp = scan.stamp;
        let mut src = Self {
            shm,
            stamp,
            advanced_at: now,
            frozen_since: None,
        };
        // Prove the feed produces a sample on connect; `scan` succeeded but
        // `select` may still find nothing on a non-CPU host.
        let _ = src.poll(now)?;
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
    fn connect(now: Instant) -> Option<Self> {
        let body = lhm::fetch(lhm::DEFAULT_ADDR)?;
        let sample = lhm::select(&lhm::parse(&body), now_unix_ms(), LHM_STALE_MS);
        // Connecting to a web server that happens to share the port is not
        // enough — it must actually be serving LibreHardwareMonitor metrics.
        if sample == CpuSample::EMPTY {
            return None;
        }
        Some(Self {
            fetched_at: now,
            sample,
        })
    }

    fn poll(&mut self, now: Instant) -> CpuSample {
        if now.duration_since(self.fetched_at) >= LHM_REFRESH {
            self.fetched_at = now;
            self.sample = lhm::fetch(lhm::DEFAULT_ADDR)
                .map(|body| lhm::select(&lhm::parse(&body), now_unix_ms(), LHM_STALE_MS))
                // A failed fetch is not a stale value to keep serving — the
                // columns go null until the server answers again.
                .unwrap_or(CpuSample::EMPTY);
        }
        self.sample
    }
}

fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
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
