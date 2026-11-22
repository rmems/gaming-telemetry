// SPDX-License-Identifier: MIT OR Apache-2.0

//! CPU telemetry.
//!
//! Every reading is an `Option`. A sensor that is absent, unreadable, or not yet
//! primed yields `None` — never `0.0`. A fabricated zero is indistinguishable
//! from a genuinely idle CPU once it reaches a training set, and silently
//! teaches a model that CPU power is constant.
//!
//! Linux reads k10temp (hwmon) temperatures and powercap RAPL energy counters
//! directly. On Windows none of those paths exist, so the monitor consumes a
//! sensor host: first a bundled LibreHardwareMonitor sidecar the collector
//! itself spawns and owns, then — opportunistically — a host the operator is
//! already running (HWiNFO64's shared-memory feed, then LHM's `/metrics` web
//! server). With nothing bound, every column stays `None` — the same contract
//! as a Linux boot without readable sensors.

#[cfg(windows)]
use std::sync::Mutex;
#[cfg(windows)]
use std::time::Instant;

#[cfg(not(windows))]
mod sysfs;
#[cfg(windows)]
mod windows;

// `hwinfo`/`lhm`/`sidecar` hold the wire-format parsers. They are compiled on
// every platform under `cfg(test)` so the Linux CI test job exercises them
// with byte fixtures; production Windows builds are the only ones that call
// them live.
#[cfg(any(windows, test))]
mod hwinfo;
#[cfg(any(windows, test))]
mod lhm;
#[cfg(any(windows, test))]
pub(crate) mod sidecar;

#[cfg(not(windows))]
pub use sysfs::CpuMonitor;
#[cfg(windows)]
pub use windows::CpuMonitor;

/// One tick of CPU telemetry. `None` means "not measured", never "zero".
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CpuSample {
    pub tctl_c: Option<f32>,
    pub ccd1_c: Option<f32>,
    pub ccd2_c: Option<f32>,
    pub package_power_w: Option<f32>,
}

impl CpuSample {
    /// A tick where nothing was measured.
    pub const EMPTY: Self = Self {
        tctl_c: None,
        ccd1_c: None,
        ccd2_c: None,
        package_power_w: None,
    };
}

/// No metered CPU package draws anywhere close to this much power. On Linux, a
/// delta implying more means the counter didn't wrap, it *reset* — S3/S4
/// resume, driver reload — and got misread as one: `energy_delta_uj` cannot
/// tell a genuine wrap (previous reading near the ceiling) from a reset to an
/// arbitrary low value using the two counter readings alone, so implausible
/// results are caught where the elapsed time makes an implied wattage
/// available to check. The same bound rejects a torn or misparsed reading from
/// a Windows sensor host.
pub(crate) const MAX_PLAUSIBLE_PACKAGE_POWER_W: f64 = 1000.0;

/// `true` when `needle` occurs in `hay` (ASCII case-insensitive) and is not
/// immediately followed by a digit — `"ccd1"` must not match `"ccd10"`, or a
/// many-CCD part would populate `cpu_ccd1_c` from the wrong die.
#[cfg(any(windows, test))]
pub(crate) fn contains_token(hay: &str, needle: &str) -> bool {
    let (h, n) = (hay.as_bytes(), needle.as_bytes());
    if h.len() < n.len() {
        return false;
    }
    (0..=h.len() - n.len()).any(|i| {
        h[i..i + n.len()].eq_ignore_ascii_case(n)
            && h.get(i + n.len()).is_none_or(|b| !b.is_ascii_digit())
    })
}

/// A plausible CPU temperature. Torn or corrupt reads land far outside this.
#[cfg(any(windows, test))]
pub(crate) fn plausible_temp(v: f64) -> Option<f32> {
    (v.is_finite() && (-50.0..=200.0).contains(&v)).then_some(v as f32)
}

/// A plausible package power: finite, non-negative, and below the same
/// ceiling the RAPL path uses to catch counter resets.
#[cfg(any(windows, test))]
pub(crate) fn plausible_power(v: f64) -> Option<f32> {
    (v.is_finite() && (0.0..=MAX_PLAUSIBLE_PACKAGE_POWER_W).contains(&v)).then_some(v as f32)
}

/// Mailbox between a detached sensor worker and the capture tick: the LHM
/// `/metrics` fetch loop and the sidecar's stdout reader both publish the
/// latest sample here, so `poll` never blocks on another process.
#[cfg(windows)]
pub(crate) struct FeedState {
    /// Most recently published sample; `EMPTY` after a read that yielded
    /// nothing usable.
    pub sample: CpuSample,
    /// When the feed last proved it is alive. What "alive" means is the
    /// worker's call: the LHM fetch loop counts only a non-empty sample
    /// (a server answering with stale readings is not a live sensor feed),
    /// while the sidecar reader counts any parsed line, since a flowing
    /// line already proves the child is updating.
    pub alive_at: Option<Instant>,
}

#[cfg(windows)]
impl FeedState {
    pub fn new() -> Self {
        Self {
            sample: CpuSample::EMPTY,
            alive_at: None,
        }
    }
}

/// Store one worker result. `alive` refreshes the liveness stamp; the sample
/// is always stored so a feed producing empties serves `EMPTY`, not a stale
/// last-good value — the columns go null until real data arrives again.
#[cfg(windows)]
pub(crate) fn publish(feed: &Mutex<FeedState>, alive: bool, sample: CpuSample) {
    let Ok(mut feed) = feed.lock() else {
        return;
    };
    if alive {
        feed.alive_at = Some(Instant::now());
    }
    feed.sample = sample;
}
