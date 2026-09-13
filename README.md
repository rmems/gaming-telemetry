# Gaming Telemetry: Neuromorphic Data Collector for SNN Training

[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

## Overview

High-frequency GPU/CPU telemetry for a workstation (optimized for **RTX 5080** under max-settings games). The collector is **game-agnostic**: it reads NVIDIA NVML + Linux CPU sensors and writes Parquet batches for neuromorphic / SNN training.

Signals map roughly to an artificial “nervous system” for models that need to learn how compute load moves a GPU:

- **Excitatory**: PCIe floods, power/clock spikes, VRAM allocation jumps
- **Inhibitory**: thermal / power throttle bitmasks
- **State / momentum**: fan speed, absolute VRAM usage

There is **no** game-install verifier, Steam/Proton discovery, or mod scanner. Graphics settings are an **operator checklist**, not code. New titles only need a new `SESSION_LABEL` and a play session.

## Captured metrics

- Power usage & temperature
- Graphics & memory clocks
- PCIe Rx/Tx throughput
- Performance state & throttle reasons
- Fan speed, VRAM used/total
- Encoder/decoder utilization
- CPU Tctl / CCD temps and package power (hwmon + RAPL energy delta) — **nullable**,
  see [CPU sensor availability](#cpu-sensor-availability)
- **`session_label`** (string; same for every row in a run)

MangoHud (or any overlay) is **not** recorded. You may still run it yourself for on-screen monitoring; the collector only writes hardware telemetry.

## Prerequisites

- **OS**: Linux (developed on Fedora)
- **GPU**: NVIDIA with NVML (RTX 50-series preferred)
- **Build**: Rust 1.98+ / Cargo

### Cargo features

A default build is just the collector: `poll hardware -> buffer -> Parquet`. The
optional extras are off because they are expensive, not because they are broken.

| Feature | Default | Adds | Cost |
|---------|---------|------|------|
| *(none)* | ✅ | `gaming-telemetry`, `export_csv` | — |
| `query` | ❌ | the `query` binary | compiles **bundled DuckDB from C++ source**; the dominant build time and peak RAM in this repo |

```bash
cargo build --release                   # collector only (fast)
cargo build --release --features query  # plus the DuckDB helper
```

Build `query` on a workstation that is also running the game you are measuring
with a capped job count, e.g. `cargo build --features query -j 8`.

## Usage

### 1. Capture a labeled session

Set `SESSION_DIR` and the collector creates that directory and writes everything into it, so batches from different runs never overwrite each other. No `cd` required.

```bash
# Examples: kcd2, re2r, re3r, re4r, re_requiem, cp2077, …
export SESSION_DIR="neuromorphic_data/kcd2_$(date +%Y%m%d_%H%M%S)"
SESSION_LABEL=kcd2 cargo run --release --bin gaming-telemetry

export SESSION_DIR="neuromorphic_data/re2r_$(date +%Y%m%d_%H%M%S)"
SESSION_LABEL=re2r cargo run --release --bin gaming-telemetry
```

`SESSION_DIR` is optional — unset, the collector writes to the current directory as it always did.

Then:

1. Set the game to the highest graphics settings available.
2. Play the session while the collector runs (default poll: **5 ms**, override with `POLL_INTERVAL_MS`).
3. Ctrl+C to flush the last batch and exit.

Each session directory contains:

```text
neuromorphic_data/kcd2_20260816_101500/
├── session_manifest.json          # what was captured, and how well
└── gpu_telemetry_v2_batch_N.parquet
```

Restarting the collector into an existing session directory **continues** that session:
`session_id`, `started_at_utc`, `session_label`, and `workload` are preserved, `restart_count`
increments, and batch numbering resumes from the highest existing batch instead of overwriting
batch 1. Only one collector may write a `SESSION_DIR` at once; a second process exits rather than
racing the manifest or batch numbers.

A directory containing legacy Parquet batches but no `session_manifest.json` is rejected rather
than silently assigning its old data a new session identity. Move those batches to a separate
directory or restore their original manifest before resuming.

The export and query examples below reuse the `$SESSION_DIR` variable from the capture block you ran. If you used a different directory, substitute its name.

### The session manifest

Written at start so the directory is self-describing during capture, then finalized on clean
shutdown with the end time and timing statistics. The temp file name includes the process ID, is
flushed to disk before rename, and the directory is synced after rename; a crash mid-write can
never publish truncated JSON.

```json
{
  "schema_version": 1,
  "session_id": "kcd2_20260816_101500",
  "session_label": "kcd2",
  "started_at_utc": "2026-08-16T10:15:00.123Z",
  "run_started_at_utc": "2026-08-16T10:15:00.123Z",
  "ended_at_utc": "2026-08-16T11:02:31.887Z",
  "poll_interval_ms_requested": 5,
  "collector_version": "0.1.0",
  "git_commit": "54f5b74",
  "restart_count": 0,
  "unclean_restart_count": 0,
  "host": { "gpu_name": "NVIDIA GeForce RTX 5080", "driver": "580.00", "cpu_model": "…" },
  "workload": { "class": "gaming", "label": "kcd2" },
  "parquet_write_failures": 0,
  "prior_runs": [],
  "timing": {
    "scope": "latest_process",
    "poll_interval_ms_requested": 5,
    "sample_count": 1440000,
    "observed_interval_ms": { "p50": 5.1, "p95": 5.4, "max": 41.2 },
    "late_sample_count": 812,
    "skipped_tick_estimate": 190,
    "elapsed_basis": "monotonic",
    "row_timestamp_basis": "wall_clock_utc"
  }
}
```

**Why the timing block matters.** A nominal 5 ms stream does not necessarily behave like one.
`observed_interval_ms` reports what actually happened, `late_sample_count` counts intervals
exceeding 1.5× the requested one, and `skipped_tick_estimate` counts ticks dropped under
`MissedTickBehavior::Skip`. The two `*_basis` fields exist because intervals are measured on the
**monotonic** clock while row `timestamp_ms` comes from the **wall** clock — an NTP step moves one
and not the other, and a consumer aligning them needs to know that. Percentiles are upper bucket
edges (100 microseconds through 100 ms, then 1 ms), so they can be slightly above the exact
`max`. `late_sample_count` and `skipped_tick_estimate` are separate, non-additive indicators: a
single delayed interval can contribute to both.

`timing.scope` is `latest_process`: after a collector restart, timing describes that process only.
`run_started_at_utc` and the root version, build, host, and polling fields describe the current
process. Every earlier process is retained in `prior_runs` with its corresponding metadata and
timing summary, so a session with changed hardware, build, or poll interval remains reproducible.
`unclean_restart_count` records prior processes that did not finalize the manifest, which means
their in-memory tail may not have been published. `timing.sample_count` counts samples acquired by
the collector. If
`parquet_write_failures` is nonzero, one or more acquired batches were not persisted, so consumers
must account for that data loss; this total is retained across restarts. `ended_at_utc` marks when
collection stopped, before any remaining batch writes are drained.

`workload.class` defaults to `gaming`; override with `WORKLOAD_CLASS`.

The manifest deliberately records no usernames, home paths, Steam identifiers, or machine
inventory — only hardware model names.

### 2. Export canonical CSV for `corinth-canal`

Stable **5-column** replay schema (unchanged; `session_label` stays in Parquet):

```bash
cargo run --bin export_csv -- "$SESSION_DIR/gpu_telemetry_v2_batch_1.parquet" canonical.csv
```

Header:

`timestamp_ms,gpu_temp_c,gpu_power_w,cpu_tctl_c,cpu_package_power_w`

`gpu_power_w` is `power_usage_mw / 1000.0`. `cpu_tctl_c` and `cpu_package_power_w`
are empty when no valid measurement was obtained for that sample — see below.

## CPU sensor availability

The four CPU columns (`cpu_tctl_c`, `cpu_ccd1_c`, `cpu_ccd2_c`,
`cpu_package_power_w`) are **nullable**. A null means *no valid measurement was
obtained for that sample*. It never means the CPU measured zero.

For the temperatures, that is always a read that did not succeed: no k10temp
device, an input the SKU does not have (CCD sensors are absent on some parts), or
an unreadable file.

`cpu_package_power_w` is a **derived** value — the difference between two energy
counter readings over the interval between them — so it is null in more cases than
"sensor missing":

| Null because | When |
|---|---|
| No readable RAPL counter | `energy_uj` absent or permission-denied (see below) |
| A single failed read | that tick's counter read did not succeed |
| **No previous reading** | the **first** poll of a run — a delta needs two samples |
| Wrap with no ceiling | the counter went backwards and `max_energy_range_uj` is unreadable |
| Reading out of range | either counter value exceeds `max_energy_range_uj` |
| Unusable interval | elapsed time was not positive and finite |

Expect one startup null at the beginning of **each collector run**, even on a
fully working machine — the energy counter is re-seeded by every new process. A
session that was restarted therefore contains one startup null *per run* — total
runs, and so total startup nulls, equal `restart_count + 1` in the manifest (the
first run plus one per restart), not `restart_count` alone.

Treat a null as "unknown for this sample", not "sensor absent".

This matters most for package power. Since
[CVE-2020-8694](https://nvd.nist.gov/vuln/detail/CVE-2020-8694), RAPL's
`energy_uj` is typically root-only (`0400`):

```console
$ ls -l /sys/class/powercap/intel-rapl:0/energy_uj
-r--------. 1 root root 4096 /sys/class/powercap/intel-rapl:0/energy_uj
```

Unless the collector can read that file, `cpu_package_power_w` is null for the
whole session. The collector says so on startup rather than leaving you to find
out after the capture:

```text
CPU package power unavailable: no readable RAPL energy counter. ...
```

To record CPU power, run the collector as root, or grant read access to the
counter for your user.

**Consumers must handle nulls.** Treating a null as `0.0` reintroduces exactly the
bug this avoids: a model trained on zero-filled CPU power learns that CPU power is
constant. Drop the rows, mask them, or impute deliberately.

### 3. Optional: DuckDB query helper

Behind the `query` feature, since it compiles bundled DuckDB from source:

```bash
cargo run --features query --bin query -- "$SESSION_DIR/gpu_telemetry_v2_batch_1.parquet"
```

## Replay contract

```text
collector -> neuromorphic_data/<session>/gpu_telemetry_v2_batch_N.parquet -> export_csv -> canonical.csv -> corinth-canal/examples/csv_replay
```

```bash
# From the corinth-canal checkout (not this repo root):
cargo run --example csv_replay --manifest-path path/to/corinth-canal/Cargo.toml -- canonical.csv
```

For multi-title training mixes, group by Parquet `session_label` (or by folder under `neuromorphic_data/`).

## Operator checklist (not code)

| Title | Suggested `SESSION_LABEL` |
|-------|---------------------------|
| Kingdom Come Deliverance 2 | `kcd2` |
| Resident Evil 2 Remake | `re2r` |
| Resident Evil 3 Remake | `re3r` |
| Resident Evil 4 Remake | `re4r` |
| Resident Evil Requiem | `re_requiem` |
| Cyberpunk 2077 | `cp2077` |

Max settings only. No install path is required by this repo.

## Design notes

- Collector never walks `$HOME`, Steam libraries, or Proton prefixes.
- No telemetry, crash reporting, or error data leaves the machine: the collector
  makes no outbound network calls at all.
- Path redaction helpers remain for error logs / query display only.
- The old Cyberpunk **workload verifier** direction (PR #6 and residual skeleton/CI) was removed; see issue #20 / Linear RM-174.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.

Previous releases (up to and including commit `ea6dc6d`) were published under GPL-3.0.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in
this work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any
additional terms or conditions.
