**Status**: draft
**Owner**: @michael (human:mh)
**Date**: 2026-06-15
**Related**: dossier task `firecracker-under-jailer` (id: `tsk_01KSDN5VTSNE820WW3EKF2QCKY`), [docs/follow-ups.md](../../follow-ups.md), retroactive security review 2026-05-24 (findings #4 + #8)

# Run firecracker under jailer with a dedicated system user — design spec

## Scope

| Bucket | Files | Est. LOC | Weighted |
|---|---|---|---|
| Production source (1×) | `src/firecracker.rs` (jailer invocation), `src/doctor.rs` (jail checks), `scripts/setup-rooms-host.sh` (firecracker user), `scripts/setup-tap.sh` (TAP owner) | ~160 | 160 |
| Tests (0.5×) | jailer-arg construction unit test + doctor jail-check unit tests | ~70 | 35 |
| Docs (0×) | `scripts/README.md` / gotchas note | ~15 | 0 |

Band: **ideal** (~195 weighted). Split only if the jailer plumbing in `firecracker.rs` balloons: PR-A = `firecracker.rs` + doctor; PR-B = the two setup scripts.

## Goal

`firecracker.rs::boot()` shells out to `firecracker` directly, running as the operator's user (`mh`). A guest escape via a Firecracker CVE lands in the operator's account — full access to `~/.ssh/`, `~/.cargo/credentials`, `~/.npm/_authToken`, Cursor SDK config. The `jailer` binary (chroot + cgroup + seccomp wrapper) is **already installed** by `setup-rooms-host.sh` — it's just unused. This is the primary "isolation actually means something" upgrade and is non-negotiable for the cloud trajectory: run firecracker jailed, as a dedicated unprivileged system user, so an escape is contained.

## Behavior / fix

### `scripts/setup-rooms-host.sh`
Create a dedicated `firecracker` system user (uid in the 100–999 system range, no login shell, no home-dir contents). Idempotent — skip if it already exists.

### `src/firecracker.rs::boot()`
Invoke firecracker via jailer instead of directly:
`jailer --id <room_id> --uid <firecracker-uid> --gid <firecracker-gid> --exec-file $(which firecracker) -- --api-sock <socket>`.
Account for jailer's chroot semantics: jailer pivots into `<chroot_base>/firecracker/<id>/root`, so kernel / rootfs / socket paths handed to firecracker are resolved **relative to that jail root**. Bind-mount or copy (per jailer's documented model) the kernel + rootfs into the jail, and resolve the API socket path the host connects to accordingly. Keep the layered-module discipline (this is `firecracker` mechanism; no policy leaks in).

### `scripts/setup-tap.sh`
Create the TAP with `user firecracker`, not `user mh`, so the operator's account never holds TAP rights. (This file is also edited by `harden-tap-rules`; sequence this task after that lands and rebase.)

### File access
Ensure every path passed to firecracker (kernel, rootfs, sockets) is readable by the `firecracker` user — chown / ACL as needed in setup or at boot-time jail staging.

### `src/doctor.rs`
Add checks: `jailer` on PATH, the `firecracker` system user exists, that user can read the kernel + rootfs files, and can open the TAP. Surface clear, actionable failures (`rooms doctor` is the operator's pre-flight).

## Acceptance

- `setup-rooms-host.sh` creates the `firecracker` system user idempotently.
- `firecracker.rs::boot()` launches through `jailer` with the dedicated uid/gid; kernel/rootfs/socket paths resolve correctly under the jail root.
- `setup-tap.sh` creates the TAP owned by `firecracker`.
- `rooms doctor` reports jailer-present, firecracker-user-exists, file-readable, TAP-openable.
- Unit tests cover the jailer argv construction (correct flags / paths assembled from a `NetworkConfig` + room id) and the doctor jail checks, mirroring the existing `parse_firecracker_version` test style.
- `make check` green (build + clippy `-D warnings` + unit tests, Windows + Linux matrix).
- **Host e2e (rooms-host, gates merge):** boot + ping/curl still works through the jail; the room runs as the `firecracker` user (verify via `ps`/cgroup), not `mh`.

## Test plan

- `firecracker.rs`: unit test asserting the jailer command line is assembled correctly (flags, uid/gid, exec-file, `--` separator, resolved socket/kernel/rootfs paths) without spawning anything — pure argv construction.
- `doctor.rs`: unit tests for each new check (present/missing jailer, user exists/absent, file readable/not) with the same warn-vs-fail conventions the doctor module already uses.
- E2E (host-only, the merge gate): `cargo test --features e2e` boot path still passes under jailer; manual `ps -o user= -p <fc pid>` shows `firecracker`.

## Non-goals

- A full seccomp-profile audit or custom jailer cgroup tuning — default jailer confinement is the v0 win.
- Read-only rootfs + overlay — that's `readonly-rootfs-with-overlay` (depends on rootfs-builder), separate task.
- Network namespace isolation per room — `harden-tap-rules` covers the iptables-level LAN isolation; this task only changes TAP ownership.

## Validation / drive notes

**Cloud-written, host-gated merge.** The Rust changes (`firecracker.rs`, `doctor.rs`) carry unit tests and pass `make check` in cloud CI; the bash changes and the jailer boot path can only be verified on `rooms-host` (KVM/Firecracker absent in cloud VMs), so the operator runs the host e2e before merge. **Sequencing:** this task edits `setup-tap.sh` (shared with `harden-tap-rules`) and `setup-rooms-host.sh` (shared with `supply-chain-checksums`); it runs in batch 2, after both batch-1 streams merge, and rebases onto their versions of those scripts.
