# Proposed Permanent Production-Ready CI Workflow

**Context**: Follow-up to #7 (Cargo + CP2077 privacy) and the new issue #15.  
Current basic workflow lives at `.github/workflows/ci.yml` (temporary feature-branch trigger for the #7 work).  
This proposal is **autonomously drafted** without modifying the live ci.yml (permission requested from user first).

## Goals
- Permanent: triggers only on `main` + PRs to `main` (no more `"fix/issue-7-*"` after merge).
- Enforce the strict `-D warnings` discipline that saved #7.
- Support the privacy-safe CP2077 story (#7, #10, #14): explicit paths, redaction testing, future verify.
- Rust hygiene: fmt, clippy, caching, audit.
- Secure & efficient (minimal permissions, good caching, parallel jobs).
- Easy to extend for the missing `verify_cyberpunk` (see #9).

## Proposed `ci.yml` content (ready to replace the current one once permission is given)

```yaml
name: CI

on:
  push:
    branches: [ main ]
  pull_request:
    branches: [ main ]
  schedule:
    # Weekly audit / health check
    - cron: '0 6 * * 1'

permissions:
  contents: read
  pull-requests: read

jobs:
  fmt:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          components: rustfmt
      - name: rustfmt
        run: cargo fmt --all -- --check

  clippy:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          components: clippy
      - uses: Swatinem/rust-cache@v2
      - name: clippy
        run: cargo clippy --all-targets -- -D warnings

  check-strict:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2
      - name: Cargo check (normal)
        run: cargo check
      - name: Cargo check (strict -D warnings)   # Protects #7 build/privacy invariants
        run: RUSTFLAGS="-D warnings" cargo check

  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2
      - name: Test
        run: cargo test

  build:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2
      - name: Build release
        run: cargo build --release
      - name: Build bins
        run: cargo build --bin gaming-telemetry --bin export_csv --bin query

  privacy-and-verify-guard:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2
      - name: Exercise privacy redaction (foundation for #7 / #14)
        run: |
          cargo test privacy -- --quiet
          # Example of safe usage (never touches real $HOME/Steam)
          # When verify is restored, add:
          # cargo run --bin verify_cyberpunk -- --game-path ./tests/fixtures/fake-cp2077 --format text --dry-run
          # and assert no raw personal paths leak in output
      - name: Cargo audit (non-blocking for now)
        run: cargo install cargo-audit && cargo audit || true

  # Future: when #9 (verify sources) + privacy-safe verify is done,
  # add a job that runs the verifier only against the test fixtures
  # using explicit paths + redaction checks.
```

## Migration notes (for when we have permission)
1. Replace the content of `.github/workflows/ci.yml` with the above (after user approval).
2. Remove the temporary `"fix/issue-7-*"` branch filter.
3. Update README badges if we add them.
4. Once the real `verify_cyberpunk` binary + privacy-safe implementation lands, expand the `privacy-and-verify-guard` job (never auto-discover paths, always pass explicit `--game-path` pointing only at `tests/fixtures/...` or synthetic data).
5. Consider adding a self-hosted runner label later for real NVIDIA telemetry smoke tests (out of scope for pure CI).

## Why this design supports the #7 goals
- The strict check job guarantees we never regress the dead-code / edition / build hygiene fixes.
- The dedicated privacy job gives a place to continuously test the redaction helpers (`src/privacy.rs`) and (future) verify without ever scanning the runner's $HOME or real Steam installs.
- Caching + separate jobs = fast feedback while keeping the "permanent" quality bar high.
- Explicit permissions + no broad write scopes.
- Schedule job keeps dependencies and audits fresh.

This proposal was generated autonomously on the feature branch while respecting the explicit request to ask permission before any direct commands/edits to the live `ci.yml`.

