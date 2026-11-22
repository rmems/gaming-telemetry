// SPDX-License-Identifier: MIT OR Apache-2.0

//! Windows `CpuMonitor`: CPU telemetry through a sensor host.
//!
//! Probe order is the bundled LibreHardwareMonitor sidecar — which the
//! collector itself spawns and owns — then an operator-run host: HWiNFO64
//! shared memory, then LibreHardwareMonitor's `/metrics` server, then
//! nothing. The sidecar comes first because it is the only source whose
//! lifecycle the collector controls: the other two are opportunistic
//! fallbacks for a box where the sidecar cannot run. A monitor that found
//! nothing keeps re-probing so a host started mid-session is picked up
//! without a collector restart.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::{CpuSample, FeedState, hwinfo, lhm, publish, sidecar};

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

/// How long a worker-fed source may go without proving liveness before it is
/// abandoned and another probe is tried — the same patience the HWiNFO path
/// allows a frozen feed. Covers both the LHM fetch loop and the sidecar's
/// stdout reader.
const FEED_DEAD: Duration = Duration::from_secs(10);

/// At connect, how long a spawned sidecar may take to produce its first
/// non-empty sample before it is killed and the probe falls through to
/// HWiNFO/LHM. An unelevated or driverless helper exits within seconds; only
/// the happy path — driver load plus first sensor update — takes any real
/// time, and even that is normally a few seconds.
const SIDECAR_CONNECT: Duration = Duration::from_secs(20);

/// Poll cadence while waiting out [`SIDECAR_CONNECT`].
const SIDECAR_CONNECT_POLL: Duration = Duration::from_millis(20);

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
    /// Set once the loud "no source bound" report has fired — the first probe
    /// result that finds nothing warns once, not on every re-probe.
    warned_unbound: bool,
    /// Manifest name of the last source that ever bound. A feed that dies
    /// before shutdown must still leave its name on the run's CPU columns;
    /// `Disconnected` alone would overwrite real provenance with `null`.
    last_bound: Option<&'static str>,
}

enum Source {
    Sidecar(SidecarSource),
    Hwinfo(HwinfoSource),
    Lhm(LhmSource),
    Disconnected,
}

impl Source {
    /// The name recorded in the session manifest's `cpu_source` field.
    fn manifest_name(&self) -> Option<&'static str> {
        match self {
            Self::Sidecar(_) => Some("lhm-sidecar"),
            Self::Hwinfo(_) => Some("hwinfo-shm"),
            Self::Lhm(_) => Some("lhm-metrics"),
            Self::Disconnected => None,
        }
    }
}

/// What a source's `poll` returned, without borrowing `self` any longer —
/// the death paths below mutate the monitor itself.
enum PollOutcome {
    /// A real or all-null sample this tick.
    Sample(CpuSample),
    /// No sample, but the source may recover on its own (a HWiNFO freeze
    /// still inside its give-up window): keep it bound, return `EMPTY`.
    Stalled,
    /// The feed is dead; drop the source and re-probe. The string is the
    /// stderr report — a dying source is announced, not silent.
    Dead(&'static str),
}

/// The sidecar child exited or stopped producing; a dead helper cannot
/// recover on its own, so re-probe — the next cycle respawns it or finds
/// HWiNFO/LHM meanwhile.
const SIDECAR_DIED: &str = "Bundled LibreHardwareMonitor sidecar stopped producing samples; CPU columns going empty while another source is probed.";

/// The feed froze and stayed frozen (HWiNFO exited, or the free edition's
/// 12-hour shared-memory window closed). Re-probe: LHM may be running, or
/// HWiNFO may come back under a fresh mapping.
const HWINFO_DIED: &str = "HWiNFO64 sensor feed stopped updating; CPU columns going empty while another source is probed.";

/// The server stopped answering (exited, or stopped its web server).
/// Re-probe: HWiNFO may have appeared meanwhile.
const LHM_DIED: &str = "LibreHardwareMonitor /metrics stopped answering; CPU columns going empty while another source is probed.";

struct SidecarSource {
    sidecar: sidecar::Sidecar,
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
    shared: Arc<Mutex<FeedState>>,
    /// Asked on drop so a superseded worker exits instead of fetching forever.
    stop: Arc<AtomicBool>,
}

impl Default for CpuMonitor {
    fn default() -> Self {
        Self::new()
    }
}

impl CpuMonitor {
    /// Start probing for a sensor host on the same detached thread the
    /// re-probe path uses. The first probe cannot run synchronously here:
    /// [`SidecarSource::connect`] may wait out [`SIDECAR_CONNECT`] for a
    /// spawned helper that never produces a sample, which would stall the
    /// manifest write and the first capture ticks. The result — a bound
    /// source, or the loud no-source report — lands on the first `poll`
    /// after the probe finishes instead.
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(probe());
        });
        Self {
            source: Source::Disconnected,
            next_probe_at: Instant::now() + PROBE_RETRY,
            probe_rx: Some(rx),
            warned_unbound: false,
            last_bound: None,
        }
    }

    /// A monitor bound to no source; every reading is `None`.
    #[cfg(test)]
    fn disconnected() -> Self {
        Self {
            source: Source::Disconnected,
            next_probe_at: Instant::now() + Duration::from_secs(3600),
            probe_rx: None,
            warned_unbound: false,
            last_bound: None,
        }
    }

    /// The sensor host that fed this run's columns — the current binding, or
    /// the last one that bound when it has since died. `None` only when
    /// nothing ever bound, which is itself the answer to "why is every cpu_*
    /// column null". The manifest records the last binding because every
    /// connected source produced at least one real sample to get bound.
    pub fn bound_source(&self) -> Option<&'static str> {
        self.source.manifest_name().or(self.last_bound)
    }

    /// Read every CPU sensor for this tick.
    pub fn poll(&mut self) -> CpuSample {
        let now = Instant::now();
        let outcome = match &mut self.source {
            Source::Sidecar(src) => src
                .poll(now)
                .map_or(PollOutcome::Dead(SIDECAR_DIED), PollOutcome::Sample),
            Source::Hwinfo(src) => match src.poll(now) {
                Some(sample) => PollOutcome::Sample(sample),
                None if src.gave_up(now) => PollOutcome::Dead(HWINFO_DIED),
                None => PollOutcome::Stalled,
            },
            Source::Lhm(src) => src
                .poll(now)
                .map_or(PollOutcome::Dead(LHM_DIED), PollOutcome::Sample),
            Source::Disconnected => {
                self.drive_probe(now);
                return CpuSample::EMPTY;
            }
        };
        match outcome {
            PollOutcome::Sample(sample) => sample,
            PollOutcome::Stalled => CpuSample::EMPTY,
            PollOutcome::Dead(message) => {
                eprintln!("{message}");
                self.source = Source::Disconnected;
                self.next_probe_at = now;
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
                        self.last_bound = self.source.manifest_name();
                    } else if !self.warned_unbound {
                        self.warned_unbound = true;
                        warn_unbound();
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
                let _ = tx.send(probe());
            });
            self.probe_rx = Some(rx);
        }
    }
}

/// Try each source in probe order; the result names what was found for
/// logging. Always runs on a detached probe thread — it can wait out a
/// sidecar connect plus a full HTTP round-trip and must never run inside
/// `new` or a capture tick. Every connect seeds its freshness clocks from a
/// timestamp taken at *its own* attempt: a `now` captured before the
/// sidecar's connect wait would mark a just-fetched fallback sample
/// already-dead.
fn probe() -> ProbeResult {
    if let Some(src) = SidecarSource::connect() {
        return ProbeResult(
            Source::Sidecar(src),
            Some("bundled LibreHardwareMonitor sidecar"),
        );
    }
    if let Some(src) = HwinfoSource::connect() {
        return ProbeResult(Source::Hwinfo(src), Some("HWiNFO64 shared memory"));
    }
    if let Some(src) = LhmSource::connect() {
        return ProbeResult(
            Source::Lhm(src),
            Some("LibreHardwareMonitor /metrics (loopback:8085)"),
        );
    }
    ProbeResult(Source::Disconnected, None)
}

/// The first probe that finds nothing reports it loudly, once: without this,
/// an empty CPU column for a whole session is indistinguishable from a
/// broken build.
fn warn_unbound() {
    eprintln!(
        "CPU telemetry unavailable: no Windows CPU sensor source bound. \
         Place the bundled {SIDECAR} next to the collector and run \
         elevated (it needs the PawnIO driver), or run HWiNFO64 with \
         \"Shared Memory Support\" enabled (Settings → General/User \
         Interface), or LibreHardwareMonitor with its remote web \
         server running (Options → Remote Web Server → Run, port \
         8085). The cpu_* columns will be empty for every tick until a \
         source binds; probing again every {}s.",
        PROBE_RETRY.as_secs(),
        SIDECAR = sidecar::SIDECAR_FILENAME
    );
}

impl SidecarSource {
    /// Spawn the bundled LHM helper and hold it bound once it proves it can
    /// produce at least one real reading. An all-null stream is not a bind —
    /// the helper that found no target sensors exits on its own, and one that
    /// merely starts slowly gets [`SIDECAR_CONNECT`] before the probe falls
    /// through to the operator-run hosts.
    fn connect() -> Option<Self> {
        let mut sidecar = sidecar::spawn()?;
        let deadline = Instant::now() + SIDECAR_CONNECT;
        loop {
            if sidecar.published().is_some_and(|s| s != CpuSample::EMPTY) {
                return Some(Self { sidecar });
            }
            // `sidecar` drops here on either failure path, which kills the
            // child — a spawn that could not produce must not leak.
            if sidecar.exited() || Instant::now() >= deadline {
                return None;
            }
            thread::sleep(SIDECAR_CONNECT_POLL);
        }
    }

    /// `Some` with the latest sample while the child keeps producing; `None`
    /// once it has exited or gone [`FEED_DEAD`] without a line.
    fn poll(&mut self, now: Instant) -> Option<CpuSample> {
        if self.sidecar.exited() {
            return None;
        }
        self.sidecar.fresh(now, FEED_DEAD)
    }
}

impl HwinfoSource {
    /// Attach only when the region is present, parses, and is still live —
    /// a stale leftover region is no better than none. Both failure modes are
    /// checked: a dead producer leaves the mutex unheld, and a frozen feed
    /// (the free edition's 12-hour cutoff) leaves the region readable with an
    /// old `last_update`. Without the age check a frozen mapping would be
    /// re-adopted on every re-probe and LHM would never get a turn.
    fn connect() -> Option<Self> {
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
        // `advanced_at` starts at this connect — not at the moment the probe
        // chain began — so time burned on an earlier source cannot make a
        // live feed look stale.
        let mut src = Self {
            shm,
            stamp,
            advanced_at: Instant::now(),
            frozen_since: None,
        };
        // Prove the feed produces a sample on connect; `scan` succeeded but
        // `select` may still find nothing on a non-CPU host, in which case the
        // LHM fallback is never reached while every column stays null.
        if src.poll(Instant::now())? == CpuSample::EMPTY {
            return None;
        }
        Some(src)
    }

    /// `Some` with this tick's sample while the feed advances, `None` once it
    /// has been frozen longer than [`HWINFO_STALE`].
    fn poll(&mut self, now: Instant) -> Option<CpuSample> {
        // A snapshot or scan failure — mutex held, mapping gone, corrupt
        // layout — is "no trustworthy sample this tick". It counts toward the
        // frozen clock the same way an unchanged stamp does, so a
        // persistently unreadable feed still gets abandoned rather than
        // re-polled forever.
        let Some(buf) = self.shm.snapshot() else {
            self.frozen_since.get_or_insert(now);
            return None;
        };
        let Some(scan) = hwinfo::scan(&buf) else {
            self.frozen_since.get_or_insert(now);
            return None;
        };
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
    fn connect() -> Option<Self> {
        let sample = fetch_sample()?;
        if sample == CpuSample::EMPTY {
            return None;
        }
        // Freshness is stamped when the fetch completed — not when the probe
        // chain started — or a sidecar that burned its whole connect window
        // would leave this just-fetched sample already expired.
        let shared = Arc::new(Mutex::new(FeedState {
            sample,
            alive_at: Some(Instant::now()),
        }));
        let stop = Arc::new(AtomicBool::new(false));
        thread::Builder::new()
            .name("cpu-lhm-fetch".into())
            .spawn({
                let shared = Arc::clone(&shared);
                let stop = Arc::clone(&stop);
                move || fetch_loop(shared, stop)
            })
            .ok()?;
        Some(Self { shared, stop })
    }

    /// `Some` with the worker's latest sample while fetches keep completing;
    /// `None` once the worker has gone [`FEED_DEAD`] without a success.
    fn poll(&mut self, now: Instant) -> Option<CpuSample> {
        let shared = self.shared.lock().ok()?;
        shared
            .alive_at
            .is_some_and(|t| now.duration_since(t) <= FEED_DEAD)
            .then_some(shared.sample)
    }
}

impl Drop for LhmSource {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Fetch `/metrics` forever until `stop`, publishing each result into
/// `shared`. The fetch blocks up to the request deadline, so it runs here —
/// *outside* the lock and off the capture tick.
fn fetch_loop(shared: Arc<Mutex<FeedState>>, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Relaxed) {
        // An answered request that yields nothing is not liveness — a server
        // whose readings all go stale must still fall out of use so the
        // monitor can re-probe another source. A failed fetch is not a stale
        // value to keep serving either: the columns go null until it answers.
        match fetch_sample() {
            Some(sample) => publish(shared.as_ref(), sample != CpuSample::EMPTY, sample),
            None => publish(shared.as_ref(), false, CpuSample::EMPTY),
        }
        thread::sleep(LHM_REFRESH);
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

    /// A disconnected monitor with no probe in flight; tests override the
    /// fields they exercise.
    fn monitor() -> CpuMonitor {
        CpuMonitor {
            source: Source::Disconnected,
            next_probe_at: Instant::now() + Duration::from_secs(3600),
            probe_rx: None,
            warned_unbound: false,
            last_bound: None,
        }
    }

    /// Serialises tests that bind or probe `127.0.0.1:8085`: a probe that
    /// finds a stub server is no longer a "found nothing" probe.
    static NET_LOCK: Mutex<()> = Mutex::new(());

    /// Same guarantee as the sysfs backend: no sensor host means no data, not
    /// zeros.
    #[test]
    fn a_monitor_without_a_source_reports_nothing_measured() {
        let mut monitor = CpuMonitor::disconnected();
        assert_eq!(monitor.poll(), CpuSample::EMPTY);
    }

    /// The first probe runs detached: neither construction nor the first tick
    /// may wait out a stalled sidecar connect. On a box where the helper
    /// exists but cannot produce, a synchronous probe would stall capture
    /// start for the whole `SIDECAR_CONNECT` — this pins the regression.
    #[test]
    fn new_and_the_first_poll_do_not_wait_on_the_probe() {
        let started = Instant::now();
        let mut monitor = CpuMonitor::new();
        let _ = monitor.poll();
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    /// The deterministic stall case the previous test can only approximate:
    /// a probe channel that never delivers is the worst blocking probe —
    /// ticks must still return `EMPTY` immediately and `bound_source` must
    /// honestly report that nothing is feeding the columns.
    #[test]
    fn a_stalled_probe_never_blocks_a_tick() {
        let (_tx, rx) = mpsc::channel();
        let mut monitor = CpuMonitor {
            probe_rx: Some(rx),
            ..monitor()
        };
        let started = Instant::now();
        for _ in 0..100 {
            assert_eq!(monitor.poll(), CpuSample::EMPTY);
        }
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(monitor.bound_source().is_none());
    }

    /// A finished probe that found no source is consumed, reports once, and
    /// leaves the monitor disconnected — the second empty result must not
    /// re-fire the loud warning.
    #[test]
    fn an_empty_probe_result_warns_once() {
        let (tx, rx) = mpsc::channel();
        tx.send(ProbeResult(Source::Disconnected, None)).unwrap();
        let mut monitor = CpuMonitor {
            probe_rx: Some(rx),
            ..monitor()
        };
        let _ = monitor.poll();
        assert!(monitor.probe_rx.is_none());
        assert!(monitor.warned_unbound);
        assert!(monitor.bound_source().is_none());

        let (tx, rx) = mpsc::channel();
        tx.send(ProbeResult(Source::Disconnected, None)).unwrap();
        monitor.probe_rx = Some(rx);
        let _ = monitor.poll();
        assert!(monitor.warned_unbound);
    }

    /// Provenance survives a disconnect: once a source fed real columns the
    /// manifest keeps its name even after the feed is gone.
    #[test]
    fn bound_source_keeps_the_last_source_that_fed() {
        let monitor = CpuMonitor {
            last_bound: Some("hwinfo-shm"),
            ..monitor()
        };
        assert_eq!(monitor.bound_source(), Some("hwinfo-shm"));
    }

    /// A live binding outranks `last_bound`; the LHM variant is the one that
    /// constructs without a sensor host.
    #[test]
    fn bound_source_prefers_the_current_binding() {
        let monitor = CpuMonitor {
            source: Source::Lhm(LhmSource {
                shared: Arc::new(Mutex::new(FeedState::new())),
                stop: Arc::new(AtomicBool::new(false)),
            }),
            last_bound: Some("hwinfo-shm"),
            ..monitor()
        };
        assert_eq!(monitor.bound_source(), Some("lhm-metrics"));
    }

    /// An expired probe window launches a detached probe, and on a box with
    /// no sensor host the probe reports back and warns. Also the cheapest way
    /// to run `probe()` itself: every connect's early-fail path — no sidecar
    /// beside the test binary, no HWiNFO region, no :8085 listener — fires in
    /// the same pass.
    #[test]
    fn an_expired_probe_window_probes_detached_and_finds_nothing() {
        let _net = NET_LOCK.lock().unwrap();
        let mut monitor = CpuMonitor {
            next_probe_at: Instant::now() - Duration::from_secs(1),
            ..monitor()
        };
        let _ = monitor.poll();
        assert!(monitor.probe_rx.is_some());

        let deadline = Instant::now() + Duration::from_secs(30);
        while monitor.probe_rx.is_some() && Instant::now() < deadline {
            let _ = monitor.poll();
            thread::sleep(Duration::from_millis(50));
        }
        assert!(monitor.probe_rx.is_none());
        // On a box with a live sensor host the probe legitimately binds a
        // source instead of warning; the contract under test is only that a
        // found-nothing probe warns once.
        if matches!(monitor.source, Source::Disconnected) {
            assert!(monitor.warned_unbound);
        }
    }

    /// A bound source whose mailbox is still live yields its sample through
    /// `poll` — the `PollOutcome::Sample` arm — and a mailbox that stopped
    /// proving liveness transitions the monitor to Dead: announced on stderr,
    /// source dropped, re-probe due immediately.
    #[test]
    fn an_lhm_source_serves_then_dies_on_a_quiet_mailbox() {
        let live = CpuSample {
            tctl_c: Some(61.5),
            ..CpuSample::EMPTY
        };
        let mut monitor = CpuMonitor {
            source: Source::Lhm(LhmSource {
                shared: Arc::new(Mutex::new(FeedState {
                    sample: live,
                    alive_at: Some(Instant::now()),
                })),
                stop: Arc::new(AtomicBool::new(false)),
            }),
            last_bound: Some("lhm-metrics"),
            ..monitor()
        };
        assert_eq!(monitor.poll(), live);

        monitor.source = Source::Lhm(LhmSource {
            shared: Arc::new(Mutex::new(FeedState {
                sample: live,
                alive_at: Some(Instant::now() - FEED_DEAD - Duration::from_secs(1)),
            })),
            stop: Arc::new(AtomicBool::new(false)),
        });
        assert_eq!(monitor.poll(), CpuSample::EMPTY);
        assert!(matches!(monitor.source, Source::Disconnected));
        // The dead feed is scheduled for an immediate re-probe.
        assert!(monitor.next_probe_at <= Instant::now());
    }

    /// `last_update` arrives in seconds on some builds and milliseconds on
    /// others; the normaliser accepts both.
    #[test]
    fn stamp_unix_secs_normalises_millisecond_stamps() {
        assert_eq!(stamp_unix_secs(1_763_340_000), 1_763_340_000);
        assert_eq!(stamp_unix_secs(1_763_340_000_000), 1_763_340_000);
    }

    /// The env override is honoured only while unelevated; either way an
    /// unspawnable target must not panic or fabricate a source.
    #[test]
    fn spawn_tolerates_an_unspawnable_env_override() {
        let _net = NET_LOCK.lock().unwrap();
        let fake = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("gaming-telemetry-fake-sidecar.exe");
        std::fs::write(&fake, b"not an executable").unwrap();

        // The variable is process-shared state — it and the file go away even
        // when the assertion below panics.
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                // SAFETY: same serialisation argument as the set_var below.
                // nosemgrep: rust.lang.security.unsafe-usage.unsafe-usage
                unsafe { std::env::remove_var("CPU_SIDECAR_PATH") };
                let _ = std::fs::remove_file(&self.0);
            }
        }
        let _cleanup = Cleanup(fake.clone());

        // SAFETY: while NET_LOCK is held no probe is in flight, and the other
        // env-reading tests reach CPU_SIDECAR_PATH only through resolve_path —
        // a reader sees either this value or the unset state, both of which
        // resolve to "no spawnable sidecar" on this box.
        // nosemgrep: rust.lang.security.unsafe-usage.unsafe-usage
        unsafe { std::env::set_var("CPU_SIDECAR_PATH", &fake) };

        if !sidecar::elevated() {
            // Unelevated, the override is actually honoured: the resolver
            // picks the fake rather than falling back to co-located names.
            assert_eq!(sidecar::resolve_path().as_deref(), Some(fake.as_path()));
        }
        assert!(sidecar::spawn().is_none());
    }

    /// Serve one canned Ryzen `/metrics` body per connection until `stop`
    /// drops it — a stand-in for an operator-run LHM on its default port.
    /// Callers must join the thread after setting `stop`: a server that
    /// outlived the test would let a later probe bind a ghost LHM source.
    fn serve_stub_metrics(listener: std::net::TcpListener, stop: Arc<AtomicBool>) {
        use std::io::{Read, Write};

        let deadline = Instant::now() + Duration::from_secs(15);
        while !stop.load(Ordering::Relaxed) && Instant::now() < deadline {
            let Ok((mut conn, _)) = listener.accept() else {
                thread::sleep(Duration::from_millis(10));
                continue;
            };
            let mut req = [0u8; 4096];
            let _ = conn.set_read_timeout(Some(Duration::from_millis(500)));
            let _ = conn.read(&mut req);
            let body = format!(
                "lhm_cpu_temperature_celsius {{\"sensorName\"=\"Core (Tctl/Tdie)\", \"sensorId\"=\"/temperature/2\", \"hardwareId\"=\"/amdcpu/0\"}} 61.5 {}\nlhm_cpu_power_watts {{\"sensorName\"=\"Package\", \"sensorId\"=\"/power/0\", \"hardwareId\"=\"/amdcpu/0\"}} 142.5 {}\n",
                now_unix_ms(),
                now_unix_ms()
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = conn.write_all(response.as_bytes());
        }
    }

    /// The whole LHM path against a stub `/metrics` server: `connect`'s proof
    /// fetch, the worker thread it spawns, the mailbox publish, and `poll`
    /// serving the sample — every line an operator-run LHM would hit.
    #[test]
    fn an_lhm_server_binds_through_connect_and_serves() {
        use std::net::TcpListener;

        let _net = NET_LOCK.lock().unwrap();
        let Ok(listener) = TcpListener::bind(lhm::DEFAULT_ADDR) else {
            // Another LHM (or a parallel run) owns the port — nothing to
            // prove here, and no assertion is safe either way.
            return;
        };
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let server = thread::spawn({
            let stop = Arc::clone(&stop);
            move || serve_stub_metrics(listener, stop)
        });

        let mut src = LhmSource::connect().expect("a live /metrics server binds");
        // The worker keeps the mailbox fresh; poll serves the bound sample.
        let deadline = Instant::now() + Duration::from_secs(5);
        let sample = loop {
            match src.poll(Instant::now()) {
                Some(sample) if sample != CpuSample::EMPTY => break sample,
                _ if Instant::now() >= deadline => panic!("no sample from a bound LHM source"),
                _ => thread::sleep(Duration::from_millis(20)),
            }
        };
        assert_eq!(sample.tctl_c, Some(61.5));
        assert_eq!(sample.package_power_w, Some(142.5));
        drop(src);
        stop.store(true, Ordering::Relaxed);
        server.join().unwrap();
    }
}
