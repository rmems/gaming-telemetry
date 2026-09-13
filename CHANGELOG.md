# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Fixed

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
