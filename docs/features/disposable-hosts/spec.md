# Disposable rooms hosts (`scripts/box.sh`)

**Status:** implemented; proven on a real Lima box and a real GCP box (evidence below).
**Owner:** @itsHabib
**Date:** 2026-09-11

## Problem

Every rooms host so far has been one hand-built, long-lived VM (Hyper-V, then
Lima). Getting rooms onto other compute, a cloud VM today and bare metal later,
had no repeatable path, and nothing proved that a freshly built host was
actually ready before work was placed on it.

## What ships

`scripts/box.sh` creates, provisions, checks, and deletes disposable hosts
("boxes"):

```sh
scripts/box.sh up <name> --backend lima|gcp [--project <gcp-project>]
scripts/box.sh provision <name> [--rev <git-rev>]
scripts/box.sh check <name>
scripts/box.sh ssh <name> [<command>...]
scripts/box.sh down <name>
```

The seam is deliberately small. A backend's only job is to produce an Ubuntu
24.04 VM with a usable `/dev/kvm` and hand back an SSH config file plus a host
alias. Everything after that runs over SSH and is identical on every backend:

- **provision** ships the committed tree at an exact revision (`git archive`,
  never the working tree), records it in `~/rooms/.box-revision`, runs
  `setup-rooms-host.sh` and `setup-tap.sh --host`, then builds and installs
  `rooms`. The box's `images/` and `target/` survive a re-provision.
- **check** runs `sudo -E rooms doctor --json` with the conventional image and
  accepts the box only when every check is `ok`. `sudo -E` keeps the user's
  HOME (so doctor probes the user's state base) while root lets it actually
  read the `ROOMS_FWD` chain, which a non-root doctor can only warn about.
  Warnings pass, a failed check fails, and an unreadable report fails closed.
  The report is saved next to the box's state as evidence.

## Backends

| Backend | Where | Notes |
| --- | --- | --- |
| `lima` | Apple Silicon Mac (M3+, macOS 15+) | Reuses `scripts/lima-rooms-host.yaml` with its mounts removed, so a box never sees the operator's checkout. Free. |
| `gcp` | Compute Engine | Intel `n2-standard-4` Spot VM with `--enable-nested-virtualization`, `--instance-termination-action=DELETE`, and `--max-run-duration=3h`, so a forgotten box deletes itself. Labeled `purpose=rooms-box`. SSH uses a per-box key injected through instance metadata. |

GCP never falls back to gcloud's active project: `--project` or
`ROOMS_BOX_GCP_PROJECT` is required. Zone, machine type, and maximum run time
are `ROOMS_BOX_GCP_ZONE`, `ROOMS_BOX_GCP_MACHINE`, and `ROOMS_BOX_GCP_MAX_RUN`.
Compute Engine refuses nested virtualization on E2, Arm, and most AMD machine
types, so the default stays Intel.

## Safety

- State lives under `${ROOMS_BOX_STATE:-~/.rooms-box}/<name>/`. `down` only
  deletes boxes recorded there, so it cannot remove a VM (for example the
  long-lived `rooms-host`) that `box.sh` did not create.
- GCP state is written before the instance is created, so `down` can clean up
  a half-created box. A missing instance (Spot preemption or maximum run time)
  counts as already gone; a failed lookup keeps the state for a retry.
- Box names must be valid for both Lima and Compute Engine.

## Acceptance

1. `tests/box_shell.rs` drives every command against fake `limactl`, `gcloud`,
   and `ssh` programs, so `make check` covers the command paths without a VM
   or a cloud account.
2. A real Lima box and a real GCP box each go `up` → `provision` → `check`
   (every `rooms doctor` check ok) → `down`.

## Evidence (2026-09-11)

**Lima** (`rooms-box-lima`, aarch64, M5 Mac): provisioned at `144b1fc`; all 15
`rooms doctor` checks ok (`kvm`, `firecracker`, `jailer`, `firecracker_user`,
`jailer_file_access`, `tun_device`, `rooms_fwd`, `slots_dir`, `orphaned_taps`,
`kernel`, `kernel_vsock`, `rootfs`, `anthropic_api_key` as a warning,
`nested_virt`, `sha_drift`).

## Out of scope

Each is recorded in [`docs/follow-ups.md`](../../follow-ups.md):

- Placing work on a box through runway over SSH (runway's rooms adapter must
  run next to `rooms`, so the hop goes above runway).
- Building the canonical Alpine agent image and running `make e2e` on a box.
- Pinning GCP host keys from the instance's guest attributes instead of
  trusting them on first use.
- Bare-metal performance. Lima and GCP boxes are both nested, so neither can
  answer the phase-2 readiness question from `p2-readiness-profile.md`.
