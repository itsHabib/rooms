## Cursor Cloud specific instructions

### Overview

`rooms` is a Rust CLI (single crate, no workspace) that spawns disposable Firecracker microVMs. Dev workflow is pure Rust — no Docker, no external services required for build/lint/test.

### Quick reference

- **Single CI command:** `make check` (runs `fmt-check` → `lint` → `test`)
- See `Makefile` and `CLAUDE.md` for the full set of targets and coding conventions.

### Toolchain

The Cloud Agent VM ships with an older Rust pinned as the default (1.83.0 at time of writing). The update script runs `rustup default stable && rustup update stable` so the latest stable toolchain is active. If `cargo` commands fail with a "requires Rust X.Y or later" message or a missing-stable-feature error, the old default is still active — run `rustup default stable && rustc --version` to fix and confirm.

### E2E tests

`cargo test --features e2e` requires a real Firecracker binary, `/dev/kvm`, and kernel + rootfs images — none of which are available in Cloud Agent VMs. Only unit tests (`make test` / `cargo test` without `--features e2e`) run here. This is expected and not a setup failure. (The e2e test itself boots with no network, so TAP devices aren't on the requirements list — but `rooms run` with `--command` is, see `CLAUDE.md`.)

### Running the CLI

The binary is `rooms`. Both subcommands need the rooms-host stack (Firecracker + KVM + a rootfs at `--image`), so they only produce meaningful output on a properly set-up host — but they compile and parse arguments correctly in any environment. Example invocation:

```sh
cargo run -- run --image ~/rooms/images/rootfs.ext4 --command 'uname -a'
cargo run -- doctor   # stub
cargo run -- run --help   # for the full argument list
```
