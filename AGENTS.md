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
| `src/bin/export_csv.rs` | `export_csv`: corinth-ingest CSV (default) or `--labelled` six-column CSV |
| `src/bin/query.rs` | `query` binary (needs the `query` feature, which builds bundled DuckDB) |
| `src/corinth_csv.rs` | Frozen vahtisiru telemetry CSV contract: writer, reader mirror, fail-loud gates |
| `src/session.rs`, `src/manifest.rs`, `src/export.rs`, `src/privacy.rs`, `src/timing.rs` | Session/manifest handling, export, redaction, timing |
| `src/cpu/` | `CpuMonitor` facade (`mod.rs`): `sysfs.rs` (Linux k10temp/RAPL), `windows.rs` (probe + dispatch), `hwinfo.rs` (HWiNFO64 shared memory), `lhm.rs` (LibreHardwareMonitor `/metrics`) |
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
- PRs for this repo go on the **Theseus Machine Physiology** board
  (`github.com/users/rmems/projects/7`): add each new PR with
  `gh project item-add 7 --owner rmems --url <pr>` and set its status (`In review`
  while open). Apply the usual metadata too — assignee `rmems`, existing labels
  only, milestone only when one already exists.
