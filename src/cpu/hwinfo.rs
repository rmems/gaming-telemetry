// SPDX-License-Identifier: MIT OR Apache-2.0

//! HWiNFO64 shared-memory sensor feed.
//!
//! When HWiNFO64 runs with "Shared Memory Support" enabled it publishes every
//! sensor reading in a memory-mapped file named `Global\HWiNFO_SENS_SM2` and
//! holds a mutex named `Global\HWiNFO_SM2_MUTEX` for as long as the process is
//! alive. The free edition stops updating the region (the data freezes in
//! place) after 12 hours of continuous use.
//!
//! The wire format below is a reimplementation from the published layout:
//! a 44-byte header, an array of 264-byte sensor elements, and an array of
//! 316-byte reading elements. Element sizes and offsets come from the header,
//! never assumed — a future HWiNFO build that grows an element keeps working as
//! long as the fields this reader needs stay at their documented offsets.
//!
//! Snapshots are taken while holding the producer mutex, so a torn 8-byte
//! value cannot slip into a column: HWiNFO holds the lock only for the brief
//! span of its own update, keeping the wait effectively uncontended. The
//! plausibility gates in [`select`] remain the last line of defence for
//! corrupt data.

use std::borrow::Cow;

use super::{CpuSample, MAX_PLAUSIBLE_PACKAGE_POWER_W, contains_token};

/// `HWiNFO_HEADER_MAGIC`, the multi-character constant `'SiWH'` — the bytes
/// `"HWiS"` read as a little-endian `u32`.
const MAGIC: u32 = 0x5369_5748;

const HEADER_SIZE: usize = 44;
/// Smallest element sizes that still contain every field this reader reads.
const MIN_SENSOR_SIZE: usize = 264;
const MIN_ENTRY_SIZE: usize = 316;

/// Reject a feed implausibly large — a corrupt `entry_element_count` would
/// otherwise turn into a huge (or over-read) section.
const MAX_ELEMENTS: usize = 4096;

// HWiNFO `SensorType` discriminants.
const TYPE_TEMPERATURE: u32 = 1;
const TYPE_POWER: u32 = 5;

/// Everything decoded from one snapshot of the shared region. Names borrow
/// the snapshot rather than copying — a 5 ms tick should not pay hundreds of
/// string allocations per poll.
pub struct Scan<'a> {
    /// `last_update` verbatim. Its units are not relied on; the caller compares
    /// successive stamps to detect whether the feed is still advancing.
    pub stamp: i64,
    /// Sensor-element `[name_original, name_user]` (the parent hardware node
    /// of each entry).
    sensors: Vec<[Cow<'a, str>; 2]>,
    entries: Vec<Entry<'a>>,
}

/// One reading element, decoded only as far as selection needs.
struct Entry<'a> {
    ty: u32,
    /// Index into `Scan::sensors` — *not* the element's `id` field.
    sensor_index: u32,
    /// `[name_original, name_user]` — users can rename readings in HWiNFO, so
    /// both are matched.
    names: [Cow<'a, str>; 2],
    value: f64,
}

/// Section offsets, sizes and counts taken from the header.
struct Layout {
    stamp: i64,
    sensor_off: usize,
    sensor_size: usize,
    sensor_count: usize,
    entry_off: usize,
    entry_size: usize,
    entry_count: usize,
}

/// Validate the header: right magic, elements big enough to hold the fields
/// this reader uses, counts small enough to be plausible, and every section
/// inside the buffer. `None` rejects anything else.
fn parse_layout(buf: &[u8]) -> Option<Layout> {
    if buf.len() < HEADER_SIZE || u32_at(buf, 0)? != MAGIC {
        return None;
    }
    // One collect() applies the `?` to all six fields at once — a corrupt
    // header yields None without six early-return branches.
    let fields: Option<Vec<usize>> = [0x14, 0x18, 0x1C, 0x20, 0x24, 0x28]
        .iter()
        .map(|&off| u32_at(buf, off).map(|v| v as usize))
        .collect();
    let f = fields?;
    Layout {
        stamp: i64_at(buf, 0x0C)?,
        sensor_off: f[0],
        sensor_size: f[1],
        sensor_count: f[2],
        entry_off: f[3],
        entry_size: f[4],
        entry_count: f[5],
    }
    .validate(buf.len())
}

impl Layout {
    /// The decoded layout must have plausible element sizes/counts and keep
    /// every declared section inside the buffer.
    fn validate(self, buf_len: usize) -> Option<Self> {
        let plausible = self.sensor_size >= MIN_SENSOR_SIZE
            && self.entry_size >= MIN_ENTRY_SIZE
            && self.sensor_count <= MAX_ELEMENTS
            && self.entry_count <= MAX_ELEMENTS;
        let in_bounds =
            section_in_bounds(
                buf_len,
                self.sensor_off,
                self.sensor_size,
                self.sensor_count,
            ) && section_in_bounds(buf_len, self.entry_off, self.entry_size, self.entry_count);
        (plausible && in_bounds).then_some(self)
    }
}

/// Decode every sensor and reading element of a snapshot whose header
/// validates. `None` on a bad magic, a truncated buffer, or a layout that
/// cannot hold the fields this reader uses.
pub fn scan(buf: &[u8]) -> Option<Scan<'_>> {
    let l = parse_layout(buf)?;

    let sensors = (0..l.sensor_count)
        .map(|i| {
            let s = &buf[l.sensor_off + i * l.sensor_size..];
            [read_cstr(&s[0x08..][..128]), read_cstr(&s[0x88..][..128])]
        })
        .collect();

    let entries = (0..l.entry_count)
        .map(|i| {
            let e = &buf[l.entry_off + i * l.entry_size..];
            Entry {
                ty: u32_at(e, 0x00).unwrap_or(0),
                sensor_index: u32_at(e, 0x04).unwrap_or(0),
                names: [read_cstr(&e[0x0C..][..128]), read_cstr(&e[0x8C..][..128])],
                value: f64_at(e, 0x11C).unwrap_or(f64::NAN),
            }
        })
        .collect();

    Some(Scan {
        stamp: l.stamp,
        sensors,
        entries,
    })
}

/// Pick the four telemetry columns out of a scan. Absent or implausible
/// readings come back `None`.
pub fn select(scan: &Scan) -> CpuSample {
    CpuSample {
        tctl_c: find(scan, TYPE_TEMPERATURE, &["tctl", "cpu package"]).and_then(plausible_temp),
        ccd1_c: find(scan, TYPE_TEMPERATURE, &["ccd1"]).and_then(plausible_temp),
        ccd2_c: find(scan, TYPE_TEMPERATURE, &["ccd2"]).and_then(plausible_temp),
        package_power_w: find(scan, TYPE_POWER, &["package", "ppt"]).and_then(plausible_power),
    }
}

/// First value whose entry type and a name needle match, trying each needle
/// in order. Only entries under a `CPU` parent node qualify: a motherboard
/// "Tctl" or fan-controller "Package" is a real reading of the *wrong* thing,
/// and this contract prefers a null column to misattributed data.
fn find(scan: &Scan, ty: u32, needles: &[&str]) -> Option<f64> {
    for needle in needles {
        for entry in &scan.entries {
            if entry.ty != ty || !entry.matches(needle) {
                continue;
            }
            let scoped = scan
                .sensors
                .get(entry.sensor_index as usize)
                .is_some_and(|parent| parent.iter().any(|n| contains_token(n, "cpu")));
            if scoped {
                return Some(entry.value);
            }
        }
    }
    None
}

impl Entry<'_> {
    fn matches(&self, needle: &str) -> bool {
        self.names.iter().any(|name| contains_token(name, needle))
    }
}

/// A plausible CPU temperature. Torn or corrupt reads land far outside this.
fn plausible_temp(v: f64) -> Option<f32> {
    (v.is_finite() && (-50.0..=200.0).contains(&v)).then_some(v as f32)
}

/// A plausible package power: finite, non-negative, and below the same ceiling
/// the RAPL path uses to catch counter resets.
fn plausible_power(v: f64) -> Option<f32> {
    (v.is_finite() && (0.0..=MAX_PLAUSIBLE_PACKAGE_POWER_W).contains(&v)).then_some(v as f32)
}

fn section_in_bounds(buf_len: usize, off: usize, size: usize, count: usize) -> bool {
    size.checked_mul(count)
        .and_then(|span| off.checked_add(span))
        .is_some_and(|end| end <= buf_len)
}

fn u32_at(buf: &[u8], off: usize) -> Option<u32> {
    buf.get(off..off + 4)
        .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
}

fn i64_at(buf: &[u8], off: usize) -> Option<i64> {
    buf.get(off..off + 8)
        .map(|b| i64::from_le_bytes(b.try_into().unwrap()))
}

fn f64_at(buf: &[u8], off: usize) -> Option<f64> {
    buf.get(off..off + 8)
        .map(|b| f64::from_le_bytes(b.try_into().unwrap()))
}

/// Names in the region are fixed-width, NUL-terminated, and not guaranteed
/// UTF-8 (units can carry a `°` sign in a legacy code page), so decode
/// lossily — every needle this reader matches on is plain ASCII. Names that
/// are already valid UTF-8 borrow the snapshot instead of copying.
fn read_cstr(field: &[u8]) -> Cow<'_, str> {
    let end = field.iter().position(|&b| b == 0).unwrap_or(field.len());
    String::from_utf8_lossy(&field[..end])
}

#[cfg(windows)]
mod imp {
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_ABANDONED, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Memory::{
        FILE_MAP_READ, MEMORY_BASIC_INFORMATION, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile,
        OpenFileMappingW, UnmapViewOfFile, VirtualQuery,
    };
    use windows_sys::Win32::System::Threading::{
        MUTEX_MODIFY_STATE, OpenMutexW, ReleaseMutex, SYNCHRONIZATION_SYNCHRONIZE,
        WaitForSingleObject,
    };

    use super::HEADER_SIZE;

    const MAPPING_NAME: &str = "Global\\HWiNFO_SENS_SM2";
    const MUTEX_NAME: &str = "Global\\HWiNFO_SM2_MUTEX";

    /// HWiNFO holds its sensor mutex only for the span of a write, so the
    /// wait is uncontended in practice; the bound exists for a wedged lock —
    /// this tick reports nothing rather than copy a mid-update region.
    const MUTEX_WAIT_MS: u32 = 50;

    /// A read-only view of HWiNFO's shared sensor region.
    ///
    /// The handle is held open deliberately — it is also what keeps the mapping
    /// object (and its now-frozen contents) alive after HWiNFO exits, which the
    /// producer-liveness check in [`SharedMemory::producer_alive`] exists to
    /// detect.
    pub struct SharedMemory {
        handle: windows_sys::Win32::Foundation::HANDLE,
        view: MEMORY_MAPPED_VIEW_ADDRESS,
        len: usize,
    }

    // The view is read-only shared memory; polling it from another thread is
    // no worse than polling it from this one.
    unsafe impl Send for SharedMemory {}

    impl SharedMemory {
        /// Attach to a running HWiNFO instance's sensor region, or `None` when
        /// none is publishing (HWiNFO not running, or shared memory disabled).
        pub fn open() -> Option<Self> {
            let name: Vec<u16> = MAPPING_NAME
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            unsafe {
                let handle = OpenFileMappingW(FILE_MAP_READ, 0, name.as_ptr());
                if handle.is_null() {
                    return None;
                }
                let view = MapViewOfFile(handle, FILE_MAP_READ, 0, 0, 0);
                if view.Value.is_null() {
                    CloseHandle(handle);
                    return None;
                }
                // The mapping's committed size is the only safe bound — the
                // header's counts are producer data and could be corrupt.
                let mut info: MEMORY_BASIC_INFORMATION = std::mem::zeroed();
                let got = VirtualQuery(
                    view.Value,
                    &mut info,
                    std::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
                );
                if got == 0 || info.RegionSize < HEADER_SIZE {
                    UnmapViewOfFile(view);
                    CloseHandle(handle);
                    return None;
                }
                Some(Self {
                    handle,
                    view,
                    len: info.RegionSize,
                })
            }
        }

        /// Copy the mapped region out for parsing, under the producer mutex.
        /// Copying rather than holding a borrow keeps every later bounds
        /// check against a stable buffer, and holding the mutex for the copy
        /// keeps a mid-update region — torn `f64`s and all — out of the
        /// telemetry columns. `None` when the mutex is absent (producer gone)
        /// or cannot be taken in time (wedged) — both mean "no trustworthy
        /// sample this tick", which is what `None` is for.
        pub fn snapshot(&self) -> Option<Vec<u8>> {
            if self.len < HEADER_SIZE {
                return None;
            }
            let name: Vec<u16> = MUTEX_NAME
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            unsafe {
                // The handle is opened per snapshot so it never keeps the
                // mutex object alive on its own — `producer_alive` must keep
                // meaning "HWiNFO is running", not "we once attached".
                let mutex = OpenMutexW(
                    SYNCHRONIZATION_SYNCHRONIZE | MUTEX_MODIFY_STATE,
                    0,
                    name.as_ptr(),
                );
                if mutex.is_null() {
                    return None;
                }
                let waited = WaitForSingleObject(mutex, MUTEX_WAIT_MS);
                let buf = (waited == WAIT_OBJECT_0 || waited == WAIT_ABANDONED).then(|| {
                    std::slice::from_raw_parts(self.view.Value as *const u8, self.len).to_vec()
                });
                if buf.is_some() {
                    ReleaseMutex(mutex);
                }
                CloseHandle(mutex);
                buf
            }
        }

        /// Whether a process currently holds HWiNFO's sensor mutex — i.e.
        /// HWiNFO itself is running. A held mapping handle would otherwise make
        /// a dead producer indistinguishable from a live one: the object outlives
        /// the process, still full of final readings.
        pub fn producer_alive() -> bool {
            let name: Vec<u16> = MUTEX_NAME
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            unsafe {
                let mutex = OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, 0, name.as_ptr());
                if mutex.is_null() {
                    false
                } else {
                    CloseHandle(mutex);
                    true
                }
            }
        }
    }

    impl Drop for SharedMemory {
        fn drop(&mut self) {
            unsafe {
                UnmapViewOfFile(self.view);
                CloseHandle(self.handle);
            }
        }
    }
}

#[cfg(windows)]
pub use imp::SharedMemory;

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal but spec-accurate region: one CPU sensor node plus the
    /// given `(type, sensor_index, name, value)` entries.
    fn region(entries: &[(u32, u32, &str, f64)]) -> Vec<u8> {
        region_with_sensors(&["CPU [#0]: AMD Ryzen 9 9950X"], entries)
    }

    /// Same as [`region`] but with caller-named sensor nodes, so tests can
    /// put identically-named readings under different parents.
    fn region_with_sensors(sensors: &[&str], entries: &[(u32, u32, &str, f64)]) -> Vec<u8> {
        let sensor_off = HEADER_SIZE;
        let sensor_size = MIN_SENSOR_SIZE;
        let entry_off = sensor_off + sensor_size * sensors.len();
        let entry_size = MIN_ENTRY_SIZE;
        let mut buf = vec![0u8; entry_off + entry_size * entries.len()];

        buf[0..4].copy_from_slice(&MAGIC.to_le_bytes());
        buf[4..8].copy_from_slice(&1u32.to_le_bytes());
        buf[8..12].copy_from_slice(&1u32.to_le_bytes());
        buf[0x0C..0x14].copy_from_slice(&1_763_340_000i64.to_le_bytes());
        for (off, v) in [
            (0x14, sensor_off as u32),
            (0x18, sensor_size as u32),
            (0x1C, sensors.len() as u32),
            (0x20, entry_off as u32),
            (0x24, entry_size as u32),
            (0x28, entries.len() as u32),
        ] {
            buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
        }

        for (i, name) in sensors.iter().enumerate() {
            let s = sensor_off + i * sensor_size;
            buf[s + 0x08..][..name.len()].copy_from_slice(name.as_bytes());
        }

        for (i, (ty, parent, name, value)) in entries.iter().enumerate() {
            let e = entry_off + i * entry_size;
            buf[e..e + 4].copy_from_slice(&ty.to_le_bytes());
            buf[e + 4..e + 8].copy_from_slice(&parent.to_le_bytes());
            buf[e + 0x0C..][..name.len()].copy_from_slice(name.as_bytes());
            buf[e + 0x11C..e + 0x124].copy_from_slice(&value.to_le_bytes());
        }
        buf
    }

    #[test]
    fn a_full_ryzen_node_yields_all_four_columns() {
        let buf = region(&[
            (TYPE_TEMPERATURE, 0, "CPU (Tctl/Tdie)", 61.5),
            (TYPE_TEMPERATURE, 0, "CPU CCD1 (Tdie)", 68.0),
            (TYPE_TEMPERATURE, 0, "CPU CCD2 (Tdie)", 55.0),
            (TYPE_POWER, 0, "CPU Package Power", 142.5),
        ]);
        let scan = scan(&buf).expect("valid region");
        assert_eq!(scan.stamp, 1_763_340_000);
        let sample = select(&scan);
        assert_eq!(sample.tctl_c, Some(61.5));
        assert_eq!(sample.ccd1_c, Some(68.0));
        assert_eq!(sample.ccd2_c, Some(55.0));
        assert_eq!(sample.package_power_w, Some(142.5));
    }

    #[test]
    fn missing_readings_stay_none() {
        let buf = region(&[(TYPE_TEMPERATURE, 0, "CPU (Tctl/Tdie)", 61.5)]);
        let sample = select(&scan(&buf).unwrap());
        assert_eq!(sample.ccd1_c, None);
        assert_eq!(sample.ccd2_c, None);
        assert_eq!(sample.package_power_w, None);
    }

    #[test]
    fn bad_magic_is_rejected() {
        let mut buf = region(&[]);
        buf[0] = b'X';
        assert!(scan(&buf).is_none());
    }

    #[test]
    fn corrupt_counts_are_rejected_without_overreading() {
        let mut buf = region(&[]);
        buf[0x28..0x2C].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(scan(&buf).is_none());
    }

    #[test]
    fn implausible_values_become_none_not_data() {
        let buf = region(&[
            (TYPE_TEMPERATURE, 0, "CPU (Tctl/Tdie)", 9_999.0),
            (TYPE_POWER, 0, "CPU Package Power", f64::NAN),
        ]);
        let sample = select(&scan(&buf).unwrap());
        assert_eq!(sample.tctl_c, None);
        assert_eq!(sample.package_power_w, None);
    }

    /// A motherboard "Tctl" is a real reading of the wrong thing: only an
    /// entry under a CPU parent node may fill `cpu_tctl_c`.
    #[test]
    fn parent_scope_disambiguates_same_named_readings() {
        // The CPU node's same-named entry wins over the motherboard's.
        let buf = region_with_sensors(
            &["Nuvoton NCT6798D", "CPU [#0]: AMD Ryzen 9 9950X"],
            &[
                (TYPE_TEMPERATURE, 0, "Tctl", 42.0),
                (TYPE_TEMPERATURE, 1, "Tctl", 61.5),
            ],
        );
        assert_eq!(select(&scan(&buf).unwrap()).tctl_c, Some(61.5));

        // With no CPU-parent entry at all, the column stays null — a
        // motherboard reading is never attributed to the package.
        let buf = region_with_sensors(
            &["Nuvoton NCT6798D"],
            &[(TYPE_TEMPERATURE, 0, "Tctl", 42.0)],
        );
        assert_eq!(select(&scan(&buf).unwrap()).tctl_c, None);
    }

    /// Token-boundary matching: a many-CCD part's `CCD10` must not populate
    /// `cpu_ccd1_c`.
    #[test]
    fn ccd10_does_not_fill_ccd1() {
        let buf = region(&[(TYPE_TEMPERATURE, 0, "CPU CCD10 (Tdie)", 77.0)]);
        let sample = select(&scan(&buf).unwrap());
        assert_eq!(sample.ccd1_c, None);
        assert_eq!(sample.ccd2_c, None);
    }
}
