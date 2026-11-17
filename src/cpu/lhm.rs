// SPDX-License-Identifier: MIT OR Apache-2.0

//! LibreHardwareMonitor `/metrics` sensor feed.
//!
//! LibreHardwareMonitor 0.9.5 removed the `ROOT\LibreHardwareMonitor` WMI
//! namespace; its built-in web server is the only out-of-process interface on
//! current releases. The server is off by default — the operator turns it on
//! under Options → Remote Web Server, and it listens on port 8085.
//!
//! The `/metrics` endpoint speaks Prometheus text format, one line per sensor
//! sample:
//!
//! ```text
//! lhm_cpu_temperature_celsius {"sensorName"="Core (Tctl/Tdie)", "sensorId"="/temperature/2", "hardwareId"="/amdcpu/0", ...} 65.5 1763340000000
//! ```
//!
//! `?timestamps=1` is requested so each line carries a millisecond epoch,
//! giving the caller a staleness check that survives a frozen feed. The fetch
//! is a loopback `TcpStream` — a deliberate exception to the collector's
//! no-network rule, kept to exactly one hardcoded loopback address carrying
//! only sensor data the operator chose to publish.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use super::{CpuSample, MAX_PLAUSIBLE_PACKAGE_POWER_W};

/// LHM's default listener. Only loopback is ever contacted.
#[cfg(windows)]
pub const DEFAULT_ADDR: &str = "127.0.0.1:8085";
const METRICS_PATH: &str = "/metrics?timestamps=1";

/// A refused loopback connect returns fast; a blackholed one must not stall a
/// telemetry tick, so both timeouts stay well under a second.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(300);
const IO_TIMEOUT: Duration = Duration::from_millis(800);
/// A real response is tens of kilobytes; anything far larger is not LHM.
const MAX_RESPONSE: usize = 1 << 20;

const METRIC_TEMPERATURE: &str = "lhm_cpu_temperature_celsius";
const METRIC_POWER: &str = "lhm_cpu_power_watts";

/// One parsed sample line.
#[derive(Debug)]
pub struct Reading {
    pub metric: String,
    pub sensor_name: String,
    pub value: f64,
    /// Millisecond epoch when LHM sampled the value, when the server sent one.
    pub ts_ms: Option<i64>,
}

/// `GET` the metrics endpoint and return the response body. `None` when the
/// server is unreachable, replies with a non-200 status, or sends too much.
pub fn fetch(addr: &str) -> Option<String> {
    let raw = http_get(addr, METRICS_PATH)?;
    let (headers, body) = split_response(&raw)?;
    if !headers.starts_with("HTTP/")
        || !headers[..headers.find('\r').unwrap_or(20).min(headers.len())].contains(" 200")
    {
        return None;
    }
    let body = if headers
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        dechunk(body)?
    } else {
        body.to_vec()
    };
    String::from_utf8(body).ok()
}

fn http_get(addr: &str, path: &str) -> Option<Vec<u8>> {
    let mut stream = TcpStream::connect_timeout(&addr.parse().ok()?, CONNECT_TIMEOUT).ok()?;
    stream.set_read_timeout(Some(IO_TIMEOUT)).ok()?;
    stream.set_write_timeout(Some(IO_TIMEOUT)).ok()?;
    stream
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .ok()?;

    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.len() > MAX_RESPONSE {
                    return None;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            // WouldBlock/TimedOut: the server stalled mid-body — take whatever
            // arrived only if the headers were complete? No: a partial body is
            // just truncated sensor lines, which are individually valid. Keep
            // what we have; the parser skips anything malformed.
            Err(_) => break,
        }
    }
    Some(buf)
}

/// Split a raw HTTP response into (header section, body).
fn split_response(raw: &[u8]) -> Option<(&str, &[u8])> {
    let pos = raw.windows(4).position(|w| w == b"\r\n\r\n")?;
    let headers = std::str::from_utf8(&raw[..pos]).ok()?;
    Some((headers, &raw[pos + 4..]))
}

/// Decode `Transfer-Encoding: chunked`. `None` on malformed framing.
fn dechunk(body: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut rest = body;
    loop {
        let line_end = rest.windows(2).position(|w| w == b"\r\n")?;
        let size_text = std::str::from_utf8(&rest[..line_end]).ok()?;
        let size = usize::from_str_radix(size_text.trim().split(';').next()?, 16).ok()?;
        rest = &rest[line_end + 2..];
        if size == 0 {
            return Some(out);
        }
        let crlf_end = size.checked_add(2)?;
        if rest.len() < crlf_end {
            return None;
        }
        out.extend_from_slice(&rest[..size]);
        rest = &rest[crlf_end..]; // skip trailing CRLF
    }
}

/// Parse every well-formed sample line. Comment (`#`) and malformed lines are
/// skipped; where a sensor appears more than once (archived history), only the
/// newest timestamp survives.
pub fn parse(body: &str) -> Vec<Reading> {
    let mut out: Vec<Reading> = Vec::new();
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(reading) = parse_line(line) {
            upsert(&mut out, reading);
        }
    }
    out
}

/// Keep the newest sample per `(metric, sensor_name)` — LHM 0.9.5 lists
/// archived history per sensor, newest timestamp wins.
fn upsert(out: &mut Vec<Reading>, reading: Reading) {
    match out
        .iter_mut()
        .find(|r| r.metric == reading.metric && r.sensor_name == reading.sensor_name)
    {
        Some(existing) if reading.ts_ms > existing.ts_ms => *existing = reading,
        Some(_) => {}
        None => out.push(reading),
    }
}

fn parse_line(line: &str) -> Option<Reading> {
    let (head, rest) = line.split_once('{')?;
    let (labels, rest) = rest.split_once('}')?;
    let metric = head.trim().to_string();
    if !metric.starts_with("lhm_") {
        return None;
    }
    let mut fields = rest.split_whitespace();
    let value: f64 = fields.next()?.parse().ok()?;
    if !value.is_finite() {
        return None;
    }
    let ts_ms = fields.next().and_then(|t| t.parse::<i64>().ok());

    let mut sensor_name = String::new();
    let mut rest_labels = labels;
    while let Some((_, after_key)) = rest_labels.split_once('"') {
        let Some((key, after_key)) = after_key.split_once('"') else {
            break;
        };
        let Some((_, value_part)) = after_key.split_once('"') else {
            break;
        };
        let Some((val, remaining)) = value_part.split_once('"') else {
            break;
        };
        if key == "sensorName" {
            sensor_name = val.to_string();
        }
        rest_labels = remaining;
    }

    Some(Reading {
        metric,
        sensor_name,
        value,
        ts_ms,
    })
}

/// Pick the four telemetry columns. `now_ms` gates freshness: a reading whose
/// timestamp is further than `stale_ms` from `now_ms` in *either* direction
/// counts as not measured — a far-future stamp pins a column just as a stale
/// one does.
pub fn select(readings: &[Reading], now_ms: i64, stale_ms: i64) -> CpuSample {
    let fresh: Vec<&Reading> = readings
        .iter()
        .filter(|r| r.ts_ms.is_none_or(|ts| (now_ms - ts).abs() <= stale_ms))
        .collect();
    CpuSample {
        tctl_c: pick(&fresh, METRIC_TEMPERATURE, &["tctl", "package"]),
        ccd1_c: pick(&fresh, METRIC_TEMPERATURE, &["ccd1"]),
        ccd2_c: pick(&fresh, METRIC_TEMPERATURE, &["ccd2"]),
        package_power_w: pick(&fresh, METRIC_POWER, &["package"]),
    }
}

fn pick(readings: &[&Reading], metric: &str, needles: &[&str]) -> Option<f32> {
    for needle in needles {
        if let Some(r) = readings
            .iter()
            .find(|r| r.metric == metric && r.sensor_name.to_ascii_lowercase().contains(needle))
        {
            return plausible(r.value, metric == METRIC_TEMPERATURE);
        }
    }
    None
}

fn plausible(v: f64, is_temp: bool) -> Option<f32> {
    if is_temp {
        (v.is_finite() && (-50.0..=200.0).contains(&v)).then_some(v as f32)
    } else {
        (v.is_finite() && (0.0..=MAX_PLAUSIBLE_PACKAGE_POWER_W).contains(&v)).then_some(v as f32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_763_340_000_000;
    const STALE: i64 = 10_000;

    fn body(lines: &[&str]) -> String {
        lines.join("\n")
    }

    fn parse_and_select(body: &str) -> CpuSample {
        select(&parse(body), NOW, STALE)
    }

    #[test]
    fn a_full_ryzen_metrics_page_yields_all_four_columns() {
        let body = body(&[
            "# TYPE lhm_cpu_temperature_celsius gauge",
            &format!(
                "lhm_cpu_temperature_celsius {{\"sensorName\"=\"Core (Tctl/Tdie)\", \"sensorId\"=\"/temperature/2\", \"hardwareId\"=\"/amdcpu/0\"}} 61.5 {NOW}"
            ),
            &format!(
                "lhm_cpu_temperature_celsius {{\"sensorName\"=\"CCD1 (Tdie)\", \"sensorId\"=\"/temperature/3\", \"hardwareId\"=\"/amdcpu/0\"}} 68.0 {NOW}"
            ),
            &format!(
                "lhm_cpu_temperature_celsius {{\"sensorName\"=\"CCD2 (Tdie)\", \"sensorId\"=\"/temperature/4\", \"hardwareId\"=\"/amdcpu/0\"}} 55.0 {NOW}"
            ),
            "# TYPE lhm_cpu_power_watts gauge",
            &format!(
                "lhm_cpu_power_watts {{\"sensorName\"=\"Package\", \"sensorId\"=\"/power/0\", \"hardwareId\"=\"/amdcpu/0\"}} 142.5 {NOW}"
            ),
        ]);
        let sample = parse_and_select(&body);
        assert_eq!(sample.tctl_c, Some(61.5));
        assert_eq!(sample.ccd1_c, Some(68.0));
        assert_eq!(sample.ccd2_c, Some(55.0));
        assert_eq!(sample.package_power_w, Some(142.5));
    }

    #[test]
    fn intel_package_names_also_resolve() {
        let body = body(&[
            &format!(
                "lhm_cpu_temperature_celsius {{\"sensorName\"=\"CPU Package\", \"hardwareId\"=\"/intelcpu/0\"}} 72.0 {NOW}"
            ),
            &format!(
                "lhm_cpu_power_watts {{\"sensorName\"=\"CPU Package\", \"hardwareId\"=\"/intelcpu/0\"}} 95.0 {NOW}"
            ),
        ]);
        let sample = parse_and_select(&body);
        assert_eq!(sample.tctl_c, Some(72.0));
        assert_eq!(sample.package_power_w, Some(95.0));
    }

    #[test]
    fn missing_sensors_stay_none() {
        let body = body(&[&format!(
            "lhm_cpu_temperature_celsius {{\"sensorName\"=\"Core (Tctl/Tdie)\"}} 61.5 {NOW}"
        )]);
        let sample = parse_and_select(&body);
        assert_eq!(sample.ccd1_c, None);
        assert_eq!(sample.package_power_w, None);
    }

    /// LHM 0.9.5 emits archived history per sensor; the newest timestamp wins.
    #[test]
    fn archived_samples_keep_the_newest() {
        let body = body(&[
            &format!(
                "lhm_cpu_temperature_celsius {{\"sensorName\"=\"Core (Tctl/Tdie)\"}} 99.0 {}",
                NOW - 3000
            ),
            &format!(
                "lhm_cpu_temperature_celsius {{\"sensorName\"=\"Core (Tctl/Tdie)\"}} 61.5 {NOW}"
            ),
        ]);
        assert_eq!(parse_and_select(&body).tctl_c, Some(61.5));
    }

    #[test]
    fn stale_timestamps_count_as_unmeasured() {
        let body = body(&[&format!(
            "lhm_cpu_temperature_celsius {{\"sensorName\"=\"Core (Tctl/Tdie)\"}} 61.5 {}",
            NOW - STALE - 1
        )]);
        assert_eq!(parse_and_select(&body).tctl_c, None);
    }

    #[test]
    fn implausible_values_become_none() {
        let body = body(&[
            &format!(
                "lhm_cpu_temperature_celsius {{\"sensorName\"=\"Core (Tctl/Tdie)\"}} 9999.0 {NOW}"
            ),
            &format!("lhm_cpu_power_watts {{\"sensorName\"=\"Package\"}} -5.0 {NOW}"),
        ]);
        let sample = parse_and_select(&body);
        assert_eq!(sample.tctl_c, None);
        assert_eq!(sample.package_power_w, None);
    }

    #[test]
    fn chunked_bodies_decode() {
        let chunked = b"20\r\nlhm_cpu_temperature_celsius 61.5\r\n0\r\n\r\n";
        assert_eq!(
            dechunk(chunked).unwrap(),
            b"lhm_cpu_temperature_celsius 61.5"
        );
        assert!(dechunk(b"zz\r\nx\r\n").is_none());
    }

    /// The loopback fetch against a real listener: the whole request/response
    /// path, not just the parser.
    #[test]
    fn fetch_reads_a_live_loopback_server() {
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let page =
            "HTTP/1.1 200 OK\r\nContent-Length: 36\r\n\r\nlhm_cpu_temperature_celsius {} 61.5\n";
        std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let mut req = [0u8; 1024];
            let _ = conn.read(&mut req);
            conn.write_all(page.as_bytes()).unwrap();
        });
        let body = fetch(&addr).expect("server answered");
        assert!(body.contains("lhm_cpu_temperature_celsius"));
    }
}
