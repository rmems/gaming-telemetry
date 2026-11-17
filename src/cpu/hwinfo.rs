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
//! Reads are unsynchronised: the producer mutex is used for liveness, not
//! locking, so a torn 8-byte value is possible in principle. The plausibility
//! gates in [`select`] turn any such garbage into `None` rather than persisting
//! it.

use super::{CpuSample, MAX_PLAUSIBLE_PACKAGE_POWER_W};

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

/// Everything decoded from one snapshot of the shared region.
pub struct Scan {
    /// `last_update` verbatim. Its units are not relied on; the caller compares
    /// successive stamps to detect whether the feed is still advancing.
    pub stamp: i64,
    /// Sensor-element names (the parent hardware node of each entry).
    sensors: Vec<String>,
    entries: Vec<Entry>,
}

/// One reading element, decoded only as far as selection needs.
struct Entry {
    ty: u32,
    /// Index into `Scan::sensors` — *not* the element's `id` field.
    sensor_index: u32,
    /// `[name_original, name_user]` — users can rename readings in HWiNFO, so
    /// both are matched.
    names: [String; 2],
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
    let layout = Layout {
        stamp: i64_at(buf, 0x0C)?,
        sensor_off: u32_at(buf, 0x14)? as usize,
        sensor_size: u32_at(buf, 0x18)? as usize,
        sensor_count: u32_at(buf, 0x1C)? as usize,
        entry_off: u32_at(buf, 0x20)? as usize,
        entry_size: u32_at(buf, 0x24)? as usize,
        entry_count: u32_at(buf, 0x28)? as usize,
    };
    let plausible = layout.sensor_size >= MIN_SENSOR_SIZE
        && layout.entry_size >= MIN_ENTRY_SIZE
        && layout.sensor_count <= MAX_ELEMENTS
        && layout.entry_count <= MAX_ELEMENTS;
    let in_bounds = section_in_bounds(
        buf.len(),
        layout.sensor_off,
        layout.sensor_size,
        layout.sensor_count,
    ) && section_in_bounds(
        buf.len(),
        layout.entry_off,
        layout.entry_size,
        layout.entry_count,
    );
    (plausible && in_bounds).then_some(layout)
}

/// Decode every sensor and reading element of a snapshot whose header
/// validates. `None` on a bad magic, a truncated buffer, or a layout that
/// cannot hold the fields this reader uses.
pub fn scan(buf: &[u8]) -> Option<Scan> {
    let l = parse_layout(buf)?;

    let sensors = (0..l.sensor_count)
        .map(|i| read_cstr(&buf[l.sensor_off + i * l.sensor_size + 0x08..][..128]))
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

/// First value whose entry type and a name needle match, trying each needle in
/// order. Entries under a `CPU [...]` parent node are preferred; an unscoped
/// pass follows so a differently-labelled CPU node still resolves.
fn find(scan: &Scan, ty: u32, needles: &[&str]) -> Option<f64> {
    let mut unscoped = None;
    for needle in needles {
        for entry in &scan.entries {
            if entry.ty != ty || !entry.matches(needle) {
                continue;
            }
            let scoped = scan
                .sensors
                .get(entry.sensor_index as usize)
                .is_some_and(|parent| parent.to_ascii_lowercase().starts_with("cpu"));
            if scoped {
                return Some(entry.value);
            }
            unscoped = unscoped.or(Some(entry.value));
        }
    }
    unscoped
}

impl Entry {
    fn matches(&self, needle: &str) -> bool {
        self.names
            .iter()
            .any(|name| name.to_ascii_lowercase().contains(needle))
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
/// lossily — every needle this reader matches on is plain ASCII.
fn read_cstr(field: &[u8]) -> String {
    let end = field.iter().position(|&b| b == 0).unwrap_or(field.len());
    String::from_utf8_lossy(&field[..end]).into_owned()
}

#[cfg(windows)]
mod imp {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Memory::{
        FILE_MAP_READ, MEMORY_BASIC_INFORMATION, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile,
        OpenFileMappingW, UnmapViewOfFile, VirtualQuery,
    };
    use windows_sys::Win32::System::Threading::{OpenMutexW, SYNCHRONIZATION_SYNCHRONIZE};

    use super::HEADER_SIZE;

    const MAPPING_NAME: &str = "Global\\HWiNFO_SENS_SM2";
    const MUTEX_NAME: &str = "Global\\HWiNFO_SM2_MUTEX";

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

        /// Copy the mapped region out for parsing. Copying rather than holding
        /// a borrow keeps every later bounds check against a stable buffer —
        /// the producer can rewrite the region at any moment.
        pub fn snapshot(&self) -> Option<Vec<u8>> {
            (self.len >= HEADER_SIZE).then(|| unsafe {
                std::slice::from_raw_parts(self.view.Value as *const u8, self.len).to_vec()
            })
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
        let sensor_off = HEADER_SIZE;
        let sensor_size = MIN_SENSOR_SIZE;
        let entry_off = sensor_off + sensor_size;
        let entry_size = MIN_ENTRY_SIZE;
        let mut buf = vec![0u8; entry_off + entry_size * entries.len()];

        buf[0..4].copy_from_slice(&MAGIC.to_le_bytes());
        buf[4..8].copy_from_slice(&1u32.to_le_bytes());
        buf[8..12].copy_from_slice(&1u32.to_le_bytes());
        buf[0x0C..0x14].copy_from_slice(&1_763_340_000i64.to_le_bytes());
        for (off, v) in [
            (0x14, sensor_off as u32),
            (0x18, sensor_size as u32),
            (0x1C, 1u32),
            (0x20, entry_off as u32),
            (0x24, entry_size as u32),
            (0x28, entries.len() as u32),
        ] {
            buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
        }

        buf[sensor_off..sensor_off + 4].copy_from_slice(&0u32.to_le_bytes());
        buf[sensor_off + 0x08..][.."CPU [#0]: AMD Ryzen 9 9950X".len()]
            .copy_from_slice(b"CPU [#0]: AMD Ryzen 9 9950X");

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

    /// A motherboard "CPU" temperature lives under a different parent node; the
    /// scoped pass must not mistake it for the CPU package.
    #[test]
    fn parent_scope_disambiguates_same_named_readings() {
        // Two sensors: index 0 is a Super I/O chip, index 1 the CPU node.
        let sensor_off = HEADER_SIZE;
        let sensor_size = MIN_SENSOR_SIZE;
        let entry_off = sensor_off + sensor_size * 2;
        let entry_size = MIN_ENTRY_SIZE;
        let mut buf = vec![0u8; entry_off + entry_size];
        buf[0..4].copy_from_slice(&MAGIC.to_le_bytes());
        for (off, v) in [
            (0x14, sensor_off as u32),
            (0x18, sensor_size as u32),
            (0x1C, 2u32),
            (0x20, entry_off as u32),
            (0x24, entry_size as u32),
            (0x28, 1u32),
        ] {
            buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
        }
        buf[sensor_off + 0x08..][..14].copy_from_slice(b"Nuvoton NCT679");
        buf[sensor_off + sensor_size + 0x08..][..9].copy_from_slice(b"CPU [#0]:");
        // The only Tctl-looking entry sits under the motherboard node: the
        // scoped match fails and the unscoped fallback still reports it.
        buf[entry_off..entry_off + 4].copy_from_slice(&TYPE_TEMPERATURE.to_le_bytes());
        buf[entry_off + 0x0C..][..4].copy_from_slice(b"Tctl");
        buf[entry_off + 0x11C..entry_off + 0x124].copy_from_slice(&42.0f64.to_le_bytes());

        let sample = select(&scan(&buf).unwrap());
        assert_eq!(sample.tctl_c, Some(42.0), "unscoped fallback keeps it");
    }
}
