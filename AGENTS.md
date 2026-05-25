## Cursor Cloud specific instructions

### Overview

`rooms` is a Rust CLI (single crate, no workspace) that spawns disposable Firecracker microVMs. Dev workflow is pure Rust — no Docker, no external services required for build/lint/test.

### Quick reference

- **Single CI command:** `make check` (runs `fmt-check` → `lint` → `test`)
- See `Makefile` and `CLAUDE.md` for the full set of targets and coding conventions.

### Toolchain

The VM ships with an older Rust (1.83.0) pinned as the default. The update script runs `rustup default stable && rustup update stable` to ensure the latest stable toolchain (currently 1.95.0) is active. If `cargo` commands fail with "feature `edition2024` is required" or similar, the old default is still active — run `rustup default stable` to fix.

### E2E tests

`cargo test --features e2e` requires a real Firecracker binary, `/dev/kvm`, kernel + rootfs images, and TAP networking — none of which are available in Cloud Agent VMs. Only unit tests (`make test` / `cargo test` without `--features e2e`) run here. This is expected and not a setup failure.

### Running the CLI

The binary is `rooms`. In dev: `cargo run -- <subcommand>`. Available subcommands: `run` (needs Firecracker + KVM), `doctor` (stub). Neither produces meaningful output without the Firecracker stack, but both compile and execute correctly.
