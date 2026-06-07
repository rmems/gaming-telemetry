//! CPU telemetry via Linux k10temp (hwmon) and powercap energy counters.
//!
//! Uses time-delta calculation for accurate package power measurement.

use std::fs;
use std::time::Instant;

/// Helper to track time-delta for CPU package power calculation.
pub struct CpuMonitor {
    last_energy_uj: u64,
    last_time: Instant,
    k10temp_base_path: Option<std::path::PathBuf>,
    rapl_path: Option<std::path::PathBuf>,
}

impl CpuMonitor {
    /// Create a new CpuMonitor with discovered paths.
    pub fn new() -> Self {
        let k10temp_base_path = Self::discover_k10temp_path();
        let rapl_path = Self::discover_rapl_path();

        let initial_energy = rapl_path
            .as_ref()
            .and_then(|p| Self::read_u64_file(p))
            .unwrap_or(0);

        Self {
            last_energy_uj: initial_energy,
            last_time: Instant::now(),
            k10temp_base_path,
            rapl_path,
        }
    }

    /// Poll CPU temperature and power.
    /// Returns (tctl_c, power_w) tuple.
    pub fn poll(&mut self) -> (f32, f32) {
        let tctl_c = self.read_tctl();
        let power_w = self.read_power_delta();
        (tctl_c, power_w)
    }

    /// Read Tctl temperature (package temperature).
    fn read_tctl(&self) -> f32 {
        if let Some(ref base) = self.k10temp_base_path {
            let path = base.join("temp1_input");
            Self::read_milli_celsius(&path)
        } else {
            0.0
        }
    }

    /// Read CCD1 temperature if available.
    pub fn read_ccd1(&self) -> f32 {
        if let Some(ref base) = self.k10temp_base_path {
            let path = base.join("temp3_input");
            Self::read_milli_celsius(&path)
        } else {
            0.0
        }
    }

    /// Read CCD2 temperature if available.
    pub fn read_ccd2(&self) -> f32 {
        if let Some(ref base) = self.k10temp_base_path {
            let path = base.join("temp4_input");
            Self::read_milli_celsius(&path)
        } else {
            0.0
        }
    }

    /// Read power from energy counter delta over time.
    fn read_power_delta(&mut self) -> f32 {
        let current_energy = self
            .rapl_path
            .as_ref()
            .and_then(|p| Self::read_u64_file(p))
            .unwrap_or(self.last_energy_uj);

        let now = Instant::now();
        let elapsed_sec = now.duration_since(self.last_time).as_secs_f32();

        let power_w = if elapsed_sec > 0.0 && current_energy >= self.last_energy_uj {
            let delta_uj = current_energy - self.last_energy_uj;
            (delta_uj as f32 / 1_000_000.0) / elapsed_sec
        } else {
            0.0
        };

        self.last_energy_uj = current_energy;
        self.last_time = now;

        power_w.max(0.0)
    }

    /// Discover k10temp hwmon path by searching /sys/class/hwmon.
    fn discover_k10temp_path() -> Option<std::path::PathBuf> {
        let entries = fs::read_dir("/sys/class/hwmon").ok()?;
        for entry in entries.flatten() {
            let name_path = entry.path().join("name");
            if let Ok(name) = fs::read_to_string(&name_path) {
                if name.trim() == "k10temp" {
                    return Some(entry.path());
                }
            }
        }
        None
    }

    /// Discover RAPL energy counter path.
    fn discover_rapl_path() -> Option<std::path::PathBuf> {
        let paths = [
            "/sys/class/powercap/amd-energy:0/energy_uj",
            "/sys/class/powercap/intel-rapl:0/energy_uj",
            "/sys/class/powercap/intel-rapl/intel-rapl:0/energy_uj",
        ];

        for path in &paths {
            if std::path::Path::new(path).exists() {
                return Some(path.into());
            }
        }
        None
    }

    /// Read a file containing an integer value.
    fn read_u64_file(path: &std::path::Path) -> Option<u64> {
        fs::read_to_string(path)
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
    }

    /// Read temperature in millicelsius and convert to celsius.
    fn read_milli_celsius(path: &std::path::Path) -> f32 {
        Self::read_u64_file(path)
            .map(|milli| milli as f32 / 1000.0)
            .unwrap_or(0.0)
    }
}

