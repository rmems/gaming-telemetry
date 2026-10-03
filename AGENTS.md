# AGENTS.md

Guidance for coding agents (Amp, Codex, Cursor, Claude Code, and others) working in this repository.

## Purpose

`gaming-telemetry` is a high-frequency GPU/CPU telemetry collector for SNN training data (see
`README.md`). It is game-agnostic: it reads NVIDIA NVML and Linux CPU sensors (hwmon, RAPL) and
writes Parquet batches labelled with a `session_label`. There is no game-install verifier,
Steam/Proton discovery or mod scanner, and graphics settings are an operator checklist, not code.

## Layout

| Path | Contents |
|------|----------|
| `src/main.rs`, `src/lib.rs` | `gaming-telemetry` collector binary and library |
| `src/bin/export_csv.rs` | `export_csv`: canonical CSV export for `corinth-canal` |
| `src/bin/query.rs` | `query` binary (needs the `query` feature, which builds bundled DuckDB) |
| `src/session.rs`, `src/manifest.rs`, `src/export.rs`, `src/privacy.rs`, `src/cpu.rs`, `src/timing.rs` | Session/manifest handling, export, redaction, CPU sensors, timing |
| `build.rs`, `src/build_info.rs` | Embeds the git SHA (`AGENTOS_GIT_SHA` override) as build info |

## Toolchain

- Rust **1.98.0** (`rust-toolchain.toml` = `rust-version`). CI's fmt job fails if they drift, and
  every CI job pins `toolchain: "1.98.0"`.
- Feature `query` (off by default) compiles bundled DuckDB from C++ source, which is the dominant
  build time and RAM cost.
- **GPU:** `nvml-wrapper` loads NVML at runtime, so it builds and tests without a GPU (CI runs on
  hosted runners with no GPU). Running the collector needs an NVIDIA GPU and driver: without them
  it exits at startup (`Nvml::init()?` in `src/main.rs`). Columns are null only when an individual
  sensor read fails.

## Commands (from `.github/workflows/ci.yml`)

```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo check --locked
RUSTFLAGS="-D warnings" cargo check --locked --all-features
cargo test --locked
cargo test --locked --all-features
cargo build --locked --release
cargo build --locked --all-features --bin gaming-telemetry --bin export_csv --bin query
```

Run the collector: `SESSION_LABEL=my-session cargo run --release --bin gaming-telemetry`
(`SESSION_DIR` optionally sets the output directory).

## Conventions visible in the repo

- A sensor column that wasn't measured is **null**, not `0`. Don't record fabricated zeros
  (README "Missing measurements"). `session_label` and `timestamp_ms` stay required.
- Most Rust sources carry SPDX license identifier headers; `build.rs` and
  `src/manifest_failure_tests.rs` currently do not.
- `CHANGELOG.md` is maintained. Commit subjects follow Conventional Commits; scopes
  (`fix(privacy):`, `feat(export):`) and PR numbers are optional.
