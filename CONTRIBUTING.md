# Contributing to rooms

Thanks for helping improve rooms. This document is the first stop for humans contributing code or docs. For agent-oriented workflow notes (MCP servers, work-driver, architecture layers), see [CLAUDE.md](CLAUDE.md) — also linked in the "Further reading" section below.

## Dev setup

### Rust-only changes (CI parity)

Most PRs only touch Rust. You do not need Firecracker or KVM for the default check loop:

1. Install [Rust](https://rustup.rs/) (stable toolchain).
2. Clone the repo and from the repo root run:

```sh
make check
```

`make check` runs `fmt-check` → `lint` → `test` — the same sequence CI uses. Unit tests run without the `e2e` feature; end-to-end tests that boot microVMs are rooms-host only.

Other useful targets: `make fmt` (apply rustfmt), `make lint`, `make test`, `make build`.

### Full rooms-host (microVMs, SSH, e2e)

Running `rooms run` or `cargo test --features e2e` requires a Linux host with `/dev/kvm`. The v0 dev layout is an Ubuntu Server VM under Hyper-V on Windows (`rooms-host`), with nested virtualization enabled so `/dev/kvm` exists inside the guest.

Bootstrap the in-VM stack with [`scripts/setup-rooms-host.sh`](scripts/setup-rooms-host.sh) (Firecracker, images, Rust, TAP, work-dir layout). The agent binary (`claude-code`) is baked into the guest rootfs by [`scripts/build-rootfs-alpine.sh`](scripts/build-rootfs-alpine.sh), not installed on the host. If `/dev/kvm` is missing inside the Hyper-V VM, enable nested virt on the host processor settings, reboot, and verify `ls /dev/kvm`.

We do not yet ship a one-size-fits-all dev environment for non-Hyper-V hosts; that is a separate engineering effort.

## Lint discipline

CI treats Clippy warnings as errors (`-D warnings`). Locally, `make lint` runs:

```sh
cargo clippy --all-targets --all-features -- -D warnings
```

Project defaults (see `Cargo.toml` and [clippy.toml](clippy.toml)):

- `clippy::all`, `pedantic`, `nursery`, and `cargo` lints warn by default.
- Restrictions in non-test code: no `panic!`, `unwrap`, indexing/slicing, `dbg!`, `print_stdout`, `todo!`, or `unimplemented!`.
- `unsafe_code` is forbidden.
- Complexity caps in `clippy.toml`: cognitive complexity 20, function length 100 lines, 6 arguments.

Do not add `#[allow(...)]` without a one-line justification comment in code.

## PR sizing

Keep PRs reviewable using weighted line counts:

| Band | Limit (weighted LOC) |
| --- | --- |
| amazing | < 500 |
| ideal | < 700 |
| stretch | < 1000 |

Weights: production source **1.0×**, tests and fixtures **0.5×**, lockfiles, configs, and docs **0×**.

## Reviewers

For each PR:

1. Request **GitHub Copilot** review.
2. Comment **`@codex review`** on the PR.
3. Comment **`@claude review`** on the PR.

**CI must be green** (`make check` on `ubuntu-latest`) before merge. Address review feedback and re-run checks as needed.

## Where to start

- **Deferred work and open questions:** [docs/follow-ups.md](docs/follow-ups.md) (in-repo status sink; may land in a sibling PR).
- **Planned production work:** [docs/features/01-productionization/driver.md](docs/features/01-productionization/driver.md) — batched specs under `docs/features/<slug>/spec.md`.
- **v0 behavior contract:** [docs/features/rooms-v0/spec.md](docs/features/rooms-v0/spec.md).

Feature changes should have a spec doc (`what` / `why` / acceptance) before large implementation PRs.

## Deeper context

[CLAUDE.md](CLAUDE.md) — architecture layers, conventions, common gotchas, and agent tooling used by maintainers.
