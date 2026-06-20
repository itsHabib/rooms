**Status**: draft
**Owner**: @michael (human:mh)
**Date**: 2026-06-19
**Related**: dossier task `readonly-rootfs-with-overlay` (id: `tsk_01KSDNM7D0RQH6J823RFZ1S9EJ`), [docs/follow-ups.md](../../follow-ups.md), retroactive security review 2026-05-24 (finding #3), [firecracker-under-jailer/spec.md](../firecracker-under-jailer/spec.md) (#44 — left the rootfs `g+rw` shared)

# Read-only rootfs with a tmpfs overlay — design spec

## Goal

The rootfs drive is mounted **writable** (`is_read_only: false`, `src/firecracker.rs::configure_vm`). A compromised guest can persist a backdoor into the image that survives into the next room and every snapshot/redistribution. Worse, #44's jailer opens the shared rootfs `g+rw` so concurrent rooms write the *same* image (the source-of-mutation I hit during the ship#143 host verification — a custom image needs `chgrp firecracker + chmod g+rw`).

Mount the rootfs **read-only at the block level**, with a **writable `/` backed by a tmpfs overlay** inside the guest. Every boot is pristine; writes evaporate on shutdown; one immutable image is safely shareable across rooms (the jailer opens it `RO` + shared, killing the `g+rw` wart).

A naive `is_read_only: true` alone breaks the guest: openrc + sshd need a writable `/var/run`, `/run`, `/var/log`, `/tmp`, `/etc/...` — all `EROFS` on a RO `/`. So the rootfs is RO at the device level but the guest sees a writable overlay.

## Key decision — overlay mechanism (init-wrapper vs initramfs)

The Alpine guest boots **BusyBox `init` as PID 1** (kernel mounts the ext4 root directly; no initrd; `/etc/inittab` → openrc runlevels). Two ways to interpose the overlay before openrc/sshd run:

| | **A. init-wrapper (recommended)** | **B. initramfs** |
|---|---|---|
| Mechanism | Bake `/sbin/overlay-init` into the rootfs; add `init=/sbin/overlay-init` to `boot_args`. As PID 1 it builds the overlay then `pivot_root`s into BusyBox `/sbin/init`. | Build a small initrd that mounts the overlay + `switch_root`s into the RO-ext4-as-lower; add `initrd_path` to `/boot-source`. |
| Artifacts | **One** (the rootfs). | **Two** (rootfs + initrd to build, pin in `checksums.txt`, distribute, stage into the jail). |
| Robustness | The PID-1 `pivot_root` dance is fiddly (see Risks) but self-contained. | `initramfs`+`switch_root` is the textbook RO-root pattern; more standard, but more moving parts + a new firecracker boot-source field + jailer staging of a second file. |
| Fit | Aligns with "minimal, no premature deps, single sharp artifact." | More machinery; closer to a distro's stock approach. |

**Recommendation: A (init-wrapper).** One artifact, no new initrd to build/pin/distribute/jail-stage, and it keeps all guest-boot logic in the rootfs builder where the rest of the init config already lives. The tradeoff is the `pivot_root` sequence below has to be exactly right — which is what the host-e2e gate is for.

## Scope

| Bucket | Files | Est. LOC | Weighted |
|---|---|---|---|
| Production source (1×) | `src/firecracker.rs` (`is_read_only: true` + `init=` in `build_boot_args`), `scripts/build-rootfs-alpine.sh` (bake `/sbin/overlay-init`), `scripts/lib/overlay-init.sh` (the wrapper) | ~90 | 90 |
| Tests (0.5×) | `build_boot_args` carries `init=`; drive is `is_read_only: true`; (host-e2e is the real gate) | ~40 | 20 |
| Docs (0×) | `scripts/README.md` + a gotchas note | ~15 | 0 |

Band: **amazing** (~110 weighted). Single PR.

## Behavior / fix

### `scripts/lib/overlay-init.sh` → baked to `/sbin/overlay-init` (PID 1)

BusyBox `ash`. Kernel mounted `/` read-only. Build a tmpfs overlay and hand off to the real init via **`pivot_root` (NOT `switch_root`)** — `switch_root` recursively deletes the old root, which would corrupt the RO lowerdir / the tmpfs holding upper+work.

```sh
#!/bin/sh
# /sbin/overlay-init — PID 1 under a read-only rootfs. Build a tmpfs-backed
# overlay (RO root = lowerdir) and pivot into BusyBox /sbin/init.
set -e
mount -t proc     none /proc
mount -t sysfs    none /sys
mount -t devtmpfs none /dev 2>/dev/null || true

mount -t tmpfs tmpfs /mnt            # /mnt exists in the image; holds upper+work+newroot
mkdir -p /mnt/upper /mnt/work /mnt/newroot
mount -t overlay overlay \
  -o lowerdir=/,upperdir=/mnt/upper,workdir=/mnt/work \
  /mnt/newroot

# Move the live pseudo-fs into the new root; keep the old RO root reachable as
# /oldroot (it is the overlay lowerdir, so it MUST stay mounted).
mkdir -p /mnt/newroot/proc /mnt/newroot/sys /mnt/newroot/dev /mnt/newroot/oldroot
mount --move /proc /mnt/newroot/proc
mount --move /sys  /mnt/newroot/sys
mount --move /dev  /mnt/newroot/dev

cd /mnt/newroot
pivot_root . oldroot                 # old RO root + the tmpfs land under /oldroot
exec chroot . /sbin/init </dev/console >/dev/console 2>&1
```

Notes for the implementer:
- The tmpfs (upper/work) ends up under `/oldroot/mnt` after pivot — still mounted, so the overlay keeps working (the kernel holds the vfsmount references). Do **not** unmount `/oldroot`.
- `/etc/inittab` is unchanged — it runs under the real `/sbin/init` on the writable overlay.
- Keep the wrapper dependency-free (BusyBox builtins only): `mount`, `mkdir`, `pivot_root`, `chroot`, `exec`. Confirm the guest kernel has `CONFIG_OVERLAY_FS=y` (the FC CI 6.1.155 kernel does; assert in the host-e2e).

### `src/firecracker.rs`
- `boot` gains a `readonly_rootfs: bool`; `rootfs_drive_payload` and `build_boot_args` take it. When true: `/drives/rootfs` → `"is_read_only": true` and boot args append `init=/sbin/overlay-init`. When false: writable drive, no forced init — any image (incl. ones without the wrapper) boots unchanged.
- **Opt-in gating (policy in `main`):** `run_room` sets `readonly_rootfs = matches!(args.runner, RunnerKind::Cursor)` — the RO+overlay hardening applies to the untrusted **cursor agent** path only; a plain `rooms run --command` (dev, quickstart/legacy images, the `--features e2e` boot test) keeps a writable rootfs. `firecracker` is mechanism; it just obeys the bool. (Resolves codex's PR #45 review: forcing the wrapper on every image panics any image that lacks it.)
- Layered discipline: this is `firecracker` mechanism; no policy leaks in. (Jailer interaction: with `is_read_only: true` the jailer/firecracker opens the rootfs RO — the `g+rw` requirement from #44 relaxes to read+shared; note it, but the jail-staging perms change is a follow-on, not required for this PR's acceptance.)

### `scripts/build-rootfs-alpine.sh`
Install `scripts/lib/overlay-init.sh` to `/sbin/overlay-init` (mode 0755) in the chroot-config phase, alongside the existing inittab/sshd setup. Ensure `/mnt` and `/oldroot` exist in the image (mountpoints).

## Acceptance

- `firecracker.rs` sends `is_read_only: true` for the rootfs drive and `init=/sbin/overlay-init` in `boot_args`.
- Guest boots; `/` is writable via the tmpfs overlay; sshd starts and accepts pubkey connections (host keys are baked, so no first-boot regen needed — but the overlay must allow `/var/run/sshd` etc.).
- `mount | grep ' / '` inside the guest shows `overlay`.
- **Ephemerality:** `echo test > /foo` in the guest; reboot the room; `/foo` is gone.
- **Host immutability:** the rootfs file's mtime on the host does **not** advance across boots (proves the device is truly RO).
- `make check` green (build + clippy `-D warnings` + unit tests, Windows + Linux matrix).
- **Host e2e (rooms-host, gates merge):** a real `sudo -E rooms run` boots, SSHes in, shows `overlay` as `/`, the write-then-reboot-gone check passes, and the host rootfs mtime is stable. This is the only thing that proves the `pivot_root` sequence is correct — unit tests can't boot a VM.

## Risks

- **The `pivot_root` dance bricks the boot if wrong** (overlay not writable, sshd can't start, or PID 1 exits → kernel panic `Attempted to kill init`). This is the whole risk surface; the host-e2e gate is mandatory and non-negotiable here. Iterate on the host, not in CI.
- **`/run` interplay:** openrc's `bootmisc` scaffolds `/run`; ensure the overlay is the parent so those writes succeed (they will, since `/` is the overlay).
- **Kernel overlay support:** assert `CONFIG_OVERLAY_FS` in the host-e2e; a kernel without it fails closed at `mount -t overlay`.

## Out of scope

- `secret-injection-via-vsock` (parked; separate task).
- Jailer jail-staging permission changes to exploit RO sharing across concurrent rooms (a follow-on once RO lands — note it, don't build it here).
- Multi-room shared-image distribution / content-addressed images.
