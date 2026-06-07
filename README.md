# Gaming Telemetry: Neuromorphic Data Collector for SNN Training

## Overview
This high-performance Rust daemon is designed to capture high-fidelity GPU telemetry data from demanding gaming workloads. Specifically optimized for systems running **Resident Evil 4** and **Cyberpunk 2077** with **Path Tracing** and **DLSS 4.0**, it provides the rich, high-frequency time-series data required to train **Spiking Neural Networks (SNNs)** and Liquid State Machines.

The collector identifies "excitatory" spikes (e.g., PCIe bus floods during asset loading) and "inhibitory" signals (e.g., thermal throttling or power caps), mimicking the dynamics of biological neural systems.

## Key Features
- **Ultra-Low Latency Polling**: Captures metrics at **5-millisecond intervals** using the NVIDIA Management Library (NVML).
- **Asynchronous I/O**: To prevent performance drops during heavy gaming (Path Tracing), data is buffered in memory and written to versioned **Parquet** files (`gpu_telemetry_v1_batch_N.parquet`) asynchronously using `tokio` and `polars`.
- **DuckDB Integration**: Includes a built-in query utility for instant analysis of the captured Parquet batches.
- **Rich Metric Suite**: Captures complex hardware states beyond simple temperature and power.

## Captured Metrics
The telemetry captures a blend of fast-moving transients and slow-moving momentum metrics:
- **PCIe Rx/Tx Throughput**: Detects data floods from the CPU/Memory (e.g., BVH structure updates for Path Tracing).
- **Power Usage & Temperature**: High-frequency transients.
- **Graphics & Memory Clocks**: Tracking the "firing rate" of the silicon.
- **Throttle Reasons**: Captures bitmasks for Power, Thermal, and Sync limits (Inhibitory signals).
- **Fan Speed (RPM)**: A slow-moving physical momentum metric.
- **VRAM Utilization**: Tracks spatial memory pressure and allocation spikes.

## Prerequisites
- **OS**: Fedora 43 Linux
- **GPU**: NVIDIA (Optimized for RTX 50-series, compatible with others)
- **Drivers**: Proprietary NVIDIA drivers with NVML support.
- **Build Tools**: Rust (Cargo)

## Usage

### 1. Start the Telemetry Daemon
Run the daemon in release mode to ensure minimal overhead and maximum timing accuracy.
```bash
cargo run --release --bin gaming-telemetry
```
The daemon continuously polls telemetry and writes versioned batches as:

`gpu_telemetry_v1_batch_N.parquet`

CPU package power is recorded from the `CpuMonitor` time-delta energy-counter path.

### 2. Export Canonical CSV for `corinth-canal`
Convert one v1 Parquet batch into the stable 5-column replay schema:
```bash
cargo run --bin export_csv gpu_telemetry_v1_batch_1.parquet canonical.csv
```

Canonical CSV header (exact order):

`timestamp_ms,gpu_temp_c,gpu_power_w,cpu_tctl_c,cpu_package_power_w`

`gpu_power_w` is exported as `power_usage_mw / 1000.0`. CPU columns come from recorded parquet columns.

### 3. Optional: Analyze Data with DuckDB
Use the query utility for ad-hoc analysis:
```bash
cargo run --bin query gpu_telemetry_v1_batch_1.parquet
```

## Replay Contract

One-way flow:

`collector -> gpu_telemetry_v1_batch_N.parquet -> export_csv -> canonical.csv -> corinth-canal/examples/csv_replay`

Consumer command in `corinth-canal`:
```bash
cargo run --example csv_replay canonical.csv
```

## Privacy and Safe Cyberpunk 2077 Telemetry Capture

The core `gaming-telemetry` collector performs **only hardware telemetry** (NVIDIA NVML GPU metrics at 5 ms + CPU power/temps via hwmon/RAPL) and writes Parquet/CSV outputs to the **current working directory**. It does **not** scan user home directories, discover Steam libraries, read Proton prefixes, or embed personal paths anywhere in its operation or data.

### Recommended Safe Workflow for Cyberpunk 2077 (Path Tracing + DLSS 4.0)
1. Run the collector from a clean, dedicated working directory (or `cd` into one) so that generated `gpu_telemetry_v1_batch_*.parquet` files stay isolated and do not mix with personal data.
2. Launch Cyberpunk 2077 with MangoHud enabled (the collector detects `mangohud_active` and records it for correlation).
3. Use `cargo run --bin export_csv ...` and the DuckDB `query` bin for analysis — all outputs remain under your control.
4. Keep telemetry sessions in version-controlled or ephemeral directories when sharing data for SNN training (e.g. with `corinth-canal`).

**Note on setup verification:** A `verify_cyberpunk` tool previously existed to validate a CP2077 install for the exact workload (`cp2077_modded_ultraplus_pt_dlss4_hdtextures_highcrowd` — checking Path Tracing, DLSS Transformer, UltraPlus/CET mods, crowd density, HD textures, etc.). Its full sources are currently absent from the tree (only the binary + some test fixtures remain). 

When the verifier is restored (see #9 and #14), it will:
- Require an **explicit `--game-path`** (or `GAME_PATH`) — no auto-discovery of `$HOME`, `~/.steam`, `~/.local/share/Steam`, or Proton `compatdata/<id>/pfx/...`.
- Redact or placeholder all personal path components in text/JSON/debug/"forensic" output by default (e.g. `$HOME/...` or `[REDACTED_HOME]`). Full paths only via an opt-in flag.
- Produce reports usable for confirming a reproducible PT telemetry session **without leaking the operator's home directory or Steam/Proton layout**.

This directly addresses the requirement to make the repo ready for Cyberpunk 2077 telemetry data tracking **without exposing any home directories**. See the parent issue #7 and related #10 (privacy leaks), #14 (implementation plan).

## Architecture for SNNs
The data collected is structured to be directly useful for Neuromorphic computing:
- **Excitatory Inputs**: PCIe throughput and VRAM allocation rate.
- **Firing Rates**: Clock speeds and Power transients.
- **Inhibitory Inputs**: Thermal/Power throttling bitmasks.
- **State/Momentum**: Fan speeds and absolute VRAM usage.

## License
GPL-3.0 License. See [LICENSE](LICENSE) for details.
