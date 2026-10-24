@AGENTS.md

# CLAUDE.md

Claude-specific notes only. Layout, toolchain, the full CI command list, and commit conventions are in
`AGENTS.md` (imported above); do not duplicate them here.

## Commands

Run from the repo root. "Verified" means it ran successfully in the environment this file was written in
(Linux, no GPU, nearly full disk). `rust-toolchain.toml` pins Rust 1.98.0, which rustup could not install
here (no space left on device), so only Rust 1.99.0 was available and no build was attempted.

| Command | Status |
|---|---|
| `cargo +1.99.0 fmt --all -- --check` | verified (Rust 1.99.0, not the 1.98.0 pin; no diff) |
| `cargo fmt --all -- --check` | not run (the pinned 1.98.0 toolchain could not be installed: disk full) |
| `cargo clippy --locked --all-targets --all-features -- -D warnings` | not run (disk full; `--all-features` compiles bundled DuckDB) |
| `cargo check --locked`, `cargo test --locked` | not run (disk full; Polars and Tokio tree is large) |
| `cargo test --locked --all-features`, `cargo build --locked --all-features --bin gaming-telemetry --bin export_csv --bin query` | not run (disk full; builds DuckDB from C++ source) |
| `SESSION_LABEL=my-session cargo run --release --bin gaming-telemetry` | not run (needs an NVIDIA GPU and driver; it must not run against real gaming sessions) |

Run fmt and clippy before committing; CI fails on either. The MSRV guard in `ci.yml` also fails if
`Cargo.toml`, `rust-toolchain.toml`, `.agents/setup`, and every CI job pin drift apart.

## Architecture map

- `src/main.rs`: `gaming-telemetry` collector (NVML poll, buffer, Parquet batches). `src/lib.rs` is the shared library.
- `src/session.rs`, `src/manifest.rs`, `src/timing.rs`, `src/cpu.rs`: session labels and directories, manifest, timing, hwmon/RAPL sensors.
- `src/export.rs` + `src/bin/export_csv.rs`: canonical CSV export for `corinth-canal`. `src/bin/query.rs`: DuckDB query tool (feature `query`).
- `src/privacy.rs`: path redaction for anything printed or recorded. `src/build_info.rs` + `build.rs`: git SHA build info.
- `src/manifest_failure_tests.rs`: manifest failure-path tests. There is no `tests/` directory.
- `.agents/setup` is the Amp orb bootstrap script, not a build step.

## Invariants and non-goals (from README and `AGENTS.md`)

- Privacy: the collector makes no outbound network calls and never walks `$HOME`, Steam libraries, or Proton prefixes.
  Keep it that way, and keep paths redacted through `src/privacy.rs` in logs and query output.
- Do not run the collector against real gaming sessions or capture real user telemetry from an agent session.
  Tests must use synthetic data only.
- Unmeasured sensor columns are null, never `0`. `session_label` and `timestamp_ms` stay required.
- Game-agnostic by design: no game-install verifier, Steam/Proton discovery, or mod scanner. The removed Cyberpunk workload verifier stays removed.
- Keep `query` (bundled DuckDB) off by default so a default build stays small and cheap on the gaming workstation.
- Generated `*.parquet`, `*.csv`, and `neuromorphic_data/` are gitignored. Do not commit captures.
- New `.rs` files start with `// SPDX-License-Identifier: MIT OR Apache-2.0`. `build.rs` and `src/manifest_failure_tests.rs` currently lack it.

## Gotchas

- Never run `cargo publish`. `Cargo.toml` has no `exclude` or `include` list, so adding this file needs no manifest change.
- `.claude/` is gitignored, so keep shared agent guidance in this file or `AGENTS.md`.
- The collector exits at startup without an NVIDIA GPU and driver (`Nvml::init()?`); build and tests do not need one.
- `Cargo.lock` is tracked, so `--locked` is correct.
- Edit only what the task needs.
