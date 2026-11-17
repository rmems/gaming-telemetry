// SPDX-License-Identifier: MIT OR Apache-2.0

//! CPU telemetry.
//!
//! Every reading is an `Option`. A sensor that is absent, unreadable, or not yet
//! primed yields `None` — never `0.0`. A fabricated zero is indistinguishable
//! from a genuinely idle CPU once it reaches a training set, and silently
//! teaches a model that CPU power is constant.
//!
//! Linux reads k10temp (hwmon) temperatures and powercap RAPL energy counters
//! directly. On Windows none of those paths exist, so the monitor instead
//! consumes a sensor host the operator is already running: HWiNFO64's
//! shared-memory feed first, then LibreHardwareMonitor's `/metrics` web server.
//! With neither present, every column stays `None` — the same contract as a
//! Linux boot without readable sensors.

#[cfg(not(windows))]
mod sysfs;
#[cfg(windows)]
mod windows;

// `hwinfo`/`lhm` hold the wire-format parsers. They are compiled on every
// platform under `cfg(test)` so the Linux CI test job exercises them with byte
// fixtures; production Windows builds are the only ones that call them live.
#[cfg(any(windows, test))]
mod hwinfo;
#[cfg(any(windows, test))]
mod lhm;

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
