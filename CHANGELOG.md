# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Fixed

- **Stale manifest temporaries are reclaimed at startup.** In-process failures
  already clean up after themselves, but a `SIGKILL` (or power loss) between
  `create_new` and `rename` stranded the temporary permanently. A collector that
  restarts often accumulated them in the session directory indefinitely. The sweep
  runs under the exclusive session lock, so it can only ever claim files no live
  writer owns, and it matches only the exact shape it generates —
  `session_manifest.json.<pid>.<nanos>.<sequence>.tmp`, all three numeric. An
  operator's `session_manifest.json.backup.tmp` is deliberately spared. A
  temporary that cannot be deleted is reported rather than dropped.
- **`export_csv` leaked the operator's absolute path on error.** Redaction covered
  the contexts this repo writes, but not the errors underneath them: polars embeds
  the path in its own message (`No such file or directory (os error 2):
  /home/<user>/…`), and `scan_parquet` is lazy, so a missing or unreadable batch
  fails at `collect()` — outside every context that had been redacted. The
  dependency's message is now flattened through the redactor, and the binary
  redacts the whole error chain at its exit point, so no layer can leak regardless
  of which one produced the path.

- **GPU NVML telemetry recorded fabricated zeros.** Every NVML sensor field used
  `unwrap_or(0)` (or `map(...).unwrap_or(0)`) when a call failed, so a missed
  power/temp/clock/PCIe/fan/VRAM/encoder/decoder/pstate/throttle read became a
  plausible `0` in Parquet. Downstream ETL (`system_telemetry_v1`) refuses a
  literal `0` on `UNAVAILABLE_ZERO_FIELDS` (power, temp, clocks, VRAM total, CPU)
  and expects null for missing. Those GPU columns are now `Option`; a failed NVML
  call writes null. A successful read of `0` (idle encoder, idle PCIe, P0, no
  throttle, fan stopped) stays `0`.

  **Breaking for consumers:** GPU sensor columns can now be null in Parquet and
  empty in the exported CSV. Treating a null as `0` reintroduces the bug.
- **CPU telemetry recorded fabricated zeros.** `CpuMonitor` seeded its energy
  counter with `unwrap_or(0)` and fell back to the previous reading on every failed
  read, so when RAPL's `energy_uj` was unreadable — the common case, since it is
  typically root-only after CVE-2020-8694 — `cpu_package_power_w` differentiated to
  a stable, plausible `0.0 W` for the entire session, with no error and no log. The
  temperature readers collapsed "sensor absent" into `0.0 °C` the same way. All four
  CPU columns are now nullable: a null means "not measured", never "measured zero".
  The collector reports unavailable sensors on startup.

  **Breaking for consumers:** `cpu_tctl_c`, `cpu_ccd1_c`, `cpu_ccd2_c` and
  `cpu_package_power_w` can now be null in Parquet and empty in the exported CSV.
  Treating a null as `0.0` reintroduces the bug.
- **RAPL counter wraparound was unhandled.** `max_energy_range_uj` is ~65 kJ on a
  typical desktop, so the counter wraps roughly every 11 minutes at 100 W — many
  times per capture. Each wrap produced a spurious `0.0 W` sample; the delta is now
  unwrapped against the ceiling.
- The first poll no longer reports a power figure differentiated over an arbitrary
  startup window; a delta needs two samples, so the first is null.
- **hwmon temperatures are signed millidegrees**, but were parsed as unsigned, so
  a legitimate sub-zero reading failed to parse and was recorded as "sensor
  unavailable". They now parse as `i64`.
- A readable-but-frozen energy counter (VM passthrough, driver quirk) still
  differentiates to a plausible `0.0 W`. A run of zero deltas is now reported: even
  an idle package accumulates far more than RAPL counter resolution per tick at
  any poll interval this collector supports, so a stalled counter is not an idle
  CPU.
- A counter *reset* (S3/S4 resume, driver reload) to an arbitrary low value looked
  identical to a wrap — both are a backwards step — and unwrapped against the
  ceiling anyway, fabricating a huge, physically impossible reading instead of the
  small genuine delta. Implausibly high wattage (over 1000 W) is now rejected
  regardless of which branch produced it.
- Startup reporting covers each temperature input individually. CCD sensors do not
  exist on every k10temp SKU, and a single unreadable input previously left one
  column empty for a whole session with no notice.
- An unreadable `max_energy_range_uj` is now reported at startup: without it a wrap
  cannot be resolved, so the single tick where the counter wraps goes empty
  (roughly every 11 minutes at 100 W) — every other tick is unaffected.
- `query`'s CPU-spike listing read `cpu_ccd1_c`/`cpu_ccd2_c` as `f32`. Those
  columns are unfiltered by the `Tctl > 80` predicate and absent on single-CCD
  parts, so the first thermal spike aborted the whole command. They are read as
  nullable and rendered `n/a`.
- `query` reports unavailable CPU aggregates instead of failing. With nullable
  columns, `avg`/`max` over an all-null column return NULL, which the `f64`
  accessor rejected.
- RAPL discovery now requires a counter it can actually *read*. It previously
  accepted any path that merely existed, which selected an unreadable root-only
  file and froze the counter at its initial value.
- **A `SESSION_LABEL` that sanitizes away is no longer silent.** `sanitize_label`
  strips everything outside `A-Z a-z 0-9 _ - .`, so a mistyped label could reduce
  to the empty string and land every row in the same anonymous bucket as setting
  no label at all — only visible after the capture. The collector now says so at
  startup.
- **Label precedence was decided before sanitization.**
  `resolve_label_from_sources` filtered candidates on their *raw* emptiness, so a
  non-empty CLI label that sanitized away won precedence and silently discarded a
  valid `SESSION_LABEL`. Each candidate is sanitized first, then the first
  surviving one wins.
- Histogram bucket indices are computed with checked conversions instead of `as
  usize`. On a 32-bit target an extreme stall could wrap into a small index and be
  misfiled as a fast sample, corrupting the tail the histogram exists to measure.
- `TimingStats::new` asserts a non-zero cadence in debug builds. Zero makes every
  `skipped_tick_estimate` division return `None`, reporting zero skipped ticks
  forever rather than surfacing the misconfiguration.

- **The build was broken.** The dependency bump to `polars 0.55.2` changed
  `LazyFrame::scan_parquet` to take a `PlRefPath`, made `DataFrame::new` take an
  explicit height, and dropped `IntoIterator` for `&ChunkedArray`. No call site had
  been updated, so neither the collector nor `export_csv` compiled. All call sites
  now match the pinned APIs. (The parallel `sentry 0.49.2` breakage is moot — see
  Removed.)
- **Ctrl+C was not the only way out of the poll loop.** Exhausting the batch-ID
  namespace returned straight out of `main`, dropping the buffered samples and
  aborting in-flight Parquet writes with the `JoinSet`. Both that path and an
  unnumberable final batch now converge on the normal shutdown: drain outstanding
  writes, then finalize the manifest.
- A Parquet write task that **panicked** was counted as a failure but only printed
  to stderr, never reported. It now goes through the same redact-and-report path as
  an I/O error.
- **Restarting the collector in a populated session directory no longer overwrites
  `gpu_telemetry_v2_batch_1.parquet`.** Batch numbering resumes from the highest existing
  batch, parsed numerically rather than lexicographically (`batch_10` sorted before `batch_2`).
  A restart also continues the existing session — preserving `session_id` and
  `started_at_utc` while incrementing `restart_count` — instead of starting a new one
  ([#22](https://github.com/rmems/gaming-telemetry/issues/22))

### Changed

- **`export_csv` now carries `session_label`, and exports a whole session.**
  The canonical CSV is the documented bridge to `corinth-canal`, but it dropped
  the one column that separates titles — so the multi-game capture added in
  [#20](https://github.com/rmems/gaming-telemetry/issues/20) was unusable through
  the documented path. The label is appended **last**, keeping the original five
  columns positionally stable.

  The binary now accepts a session directory and emits every batch in batch order
  under one header. It previously took a single file, so exporting a session meant
  N invocations producing N headers, and any shell glob ordered `batch_10` before
  `batch_2` — silently scrambling the exported time series.

  The column contract and batch ordering moved into `gaming_telemetry::export`,
  which has tests; `bin/export_csv.rs` was previously all `main()` with no seam and
  no coverage at all.

- **`duckdb` is now optional**, behind an off-by-default `query` cargo feature
  ([#20](https://github.com/rmems/gaming-telemetry/issues/20)). Its `bundled` feature
  compiles the whole DuckDB C++ tree and dominated build time and peak RAM on a
  workstation that is also running the game being measured. A default build is now
  just the collector. Build the helper with `cargo run --features query --bin query`.
- `duckdb`'s unused `polars` feature (its Arrow↔Polars bridge) was dropped —
  `query.rs` only issues plain SQL through `Connection`/`row.get`.
- Parquet columns are now built with the column name and its source field on one
  line, removing the 18 separate passes over the sample slice and the per-row clone
  of the run-invariant `session_label`.
- Rust edition bumped to 2024 and MSRV to 1.98.0.
- **Relicensed from GPL-3.0 to `MIT OR Apache-2.0`**
  ([#19](https://github.com/rmems/gaming-telemetry/issues/19),
  [#23](https://github.com/rmems/gaming-telemetry/pull/23))

  The root `LICENSE` (GPL-3.0) was replaced by [`LICENSE-MIT`](LICENSE-MIT) and
  [`LICENSE-APACHE`](LICENSE-APACHE); `Cargo.toml`'s `license` field now reads
  `MIT OR Apache-2.0`; every source file carries an
  `SPDX-License-Identifier: MIT OR Apache-2.0` header.

  This aligns the collector with the permissive terms of its downstream consumers
  (`corinth-canal`, `Spikenaut-SNN`, `LiquidCortex.jl`). No code semantics changed.

  **If you require GPL-3.0 terms**, pin to commit `ea6dc6d` or earlier — everything
  through that commit remains available under GPL-3.0.

### Added

- **Session manifest** (`session_manifest.json`, `schema_version: 1`) written into every
  capture directory: session id/label, start and end timestamps, requested poll interval,
  collector version, git commit, host GPU/driver/CPU model, and workload class
  ([#22](https://github.com/rmems/gaming-telemetry/issues/22))
- **Timing-quality statistics** in the manifest — observed inter-sample interval p50/p95/max,
  late-sample and skipped-tick counts, and explicit monotonic-vs-wall-clock basis fields, so a
  consumer can tell whether a nominal 5 ms stream actually behaved like one
  ([#22](https://github.com/rmems/gaming-telemetry/issues/22))
- **`SESSION_DIR`** environment variable: the collector creates and owns the session
  directory instead of requiring the operator to `cd` into it. Unset preserves the previous
  write-to-cwd behavior ([#22](https://github.com/rmems/gaming-telemetry/issues/22))
- **`WORKLOAD_CLASS`** environment variable, defaulting to `gaming`
- `session_label` on every Parquet row, set via the `SESSION_LABEL` environment
  variable, so multi-title capture sessions can be separated downstream
  ([#20](https://github.com/rmems/gaming-telemetry/issues/20),
  [#21](https://github.com/rmems/gaming-telemetry/pull/21))

### Removed

- **Qodana**, entirely — `qodana.yaml`, `.github/workflows/qodana_code_quality.yml`,
  and the `QODANA_TOKEN_1849579870` Cloud scan. `qodana-rust` is Ultimate/EAP only;
  with Cloud membership expired the workflow cannot run usefully and there is no
  community Rust linter to fall back to. Remaining gates stay in `ci.yml`
  ([#42](https://github.com/rmems/gaming-telemetry/issues/42)).
- **Sentry, entirely** — the dependency, the ~100-line bootstrap in `main.rs`, the
  `SENTRY_*` environment variables, and the `sentry-release` workflow
  ([#20](https://github.com/rmems/gaming-telemetry/issues/20)). It was a hard
  dependency pulling an HTTP/TLS stack into a local 5 ms poll daemon, and its
  automatic panic integration captured exception and stack-frame data that bypassed
  `privacy::redact_personal_path` entirely. The collector now makes no outbound
  network calls. Write failures are reported to stderr, still redacted.
- The `verify_cyberpunk` workload verifier, its CI job, and verify-centric docs. The
  collector is game-agnostic: no game-install discovery, no Steam/Proton scanning, no
  mod scanning ([#21](https://github.com/rmems/gaming-telemetry/pull/21))
