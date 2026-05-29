**Status**: draft
**Owner**: @michael (human:mh)
**Date**: 2026-05-29
**Related**: dossier task `agent-rootfs-alpine-kernel` (id: `tsk_01KSS57NYAAW3NBFEFW6BSHJMF`), [v0 spec](../rooms-v0/spec.md), [rootfs-builder](../rootfs-builder/spec.md), [cursor-sdk-runner](../cursor-sdk-runner/spec.md), [follow-ups](../../follow-ups.md)

# Alpine agent rootfs + Firecracker-tuned kernel — design spec

## Scope

| Bucket | Files | Est. LOC | Weighted |
|---|---|---|---|
| Production source (1×) | `scripts/build-rootfs-alpine.sh` (~230), `scripts/setup-rooms-host.sh` kernel-fetch change (~25), `scripts/lib/rootfs-helpers.sh` touch (~10) | ~265 | 265 |
| Tests (0.5×) | `scripts/test-rootfs-alpine.sh` (boot smoke; ~90) | ~90 | 45 |
| Configs / docs (0×) | `scripts/README.md`, `README.md`, `docs/follow-ups.md` | ~40 | 0 |
| **Total weighted** | | | **~310** |

Band: **amazing** (< 500).

## Goal

Replace the artisanal Ubuntu-noble debootstrap image (`build-rootfs.sh`) with a lean **Alpine 3.21** (musl / busybox / openrc — no systemd) agent rootfs, paired with a **Firecracker-tuned guest kernel** (6.1.155, virtio-rng built in). The image bakes openssh (key-only **root**, `AcceptEnv ANTHROPIC_API_KEY`), git, ca-certificates, and `claude-code` (native musl binary). DNS works out of the box, boot is < 5 s, and the image is well under 300 MB.

> Acceptance (from the dossier task): `rooms run --image agent-alpine.ext4 --keep` boots in < ~20 s, sshd reachable, `getent hosts github.com` resolves with no manual fix; claude-code runs in-guest; `git clone` over https works; image well under the 300 MB noble image.

## Why now (validated)

The first end-to-end dogfood (2026-05-29, PR #34; seams logged in `docs/follow-ups.md`) proved the substrate works and pinned the rootfs as the weak link:

1. noble's **systemd-resolved** clobbered the baked `/etc/resolv.conf` → guest DNS broke. Alpine has no systemd-resolved; a static resolv.conf just persists.
2. noble booted **~105 s** on the bionic-quickstart `vmlinux.bin` (Linux 4.14, no `random.trust_cpu`, no virtio-rng). A tuned kernel + openrc boots in 1–2 s.
3. the noble image was **300 MB+**.

**De-risk spike (run on rooms-host 2026-05-29) — Alpine is a confirmed GO:**

- `apk add claude-code` from the official Anthropic apk repo (`stable` channel) installed **2.1.148-r1**; `claude --version` → exit 0, no relocation errors.
- `ldd /usr/bin/claude` links **only** against musl (`ld-musl-x86_64.so.1`) — zero glibc dependency. The recurring `posix_getdents` glibc-symbol regression is **not** present in this stable build.
- claude binary = **220 MB**; full rootfs (21 apk packages, **no Node**) = **~240–250 MB**.
- Anthropic apk signing-key sha256 `395759c1…68b6` confirmed against the official docs.
- The FC CI kernel `firecracker-ci/v1.15/x86_64/vmlinux-6.1.155` (44 MB, uncompressed ELF) was confirmed live, with `CONFIG_HW_RANDOM_VIRTIO=y` in its paired `.config`.

## Functional

### Builder — `scripts/build-rootfs-alpine.sh`

A new, dedicated bash script (does **not** touch the debootstrap `build-rootfs.sh`). Requires root/sudo; reuses `assert_root` + `cleanup_mount` from `lib/rootfs-helpers.sh`.

```
sudo ./scripts/build-rootfs-alpine.sh \
  --out images/agent-alpine.ext4 \
  --ssh-key ~/.ssh/id_rooms.pub \
  [--alpine-version 3.21.7] \
  [--claude-version 2.1.148-r1] \
  [--size 512M] \
  [--extend <script>]      # runs in the chroot after baseline installs (cursor-sdk-runner hook)
```

Ordered steps (trap-guarded cleanup on every exit path):

1. Validate prereqs (`mkfs.ext4`, `losetup`, `mount`, `chroot`, `curl`/`wget`, `sha256sum`, `tar`); require `--ssh-key`.
2. Fetch + **sha256-verify** the pinned `alpine-minirootfs-${ALPINE}-x86_64.tar.gz` from `dl-cdn.alpinelinux.org/alpine/v3.21/releases/x86_64` (read the `.sha256` sidecar; abort on mismatch).
3. `truncate -s ${SIZE}` sparse file → `mkfs.ext4 -F` → loop-mount.
4. Extract minirootfs (`tar -xzf … --numeric-owner`).
5. Bind `/dev /proc /sys`; copy host `/etc/resolv.conf` into the chroot for the apk fetch phase.
6. In chroot: pin `/etc/apk/repositories` to `v3.21` `main` + `community`; `apk add --no-cache` the package set (below); add the Anthropic apk key (**verify sha256**) + repo; `apk add --no-cache claude-code=${CLAUDE_VERSION}`.
7. Configure the guest (inittab, openrc runlevels, sshd, resolv.conf, ca-certificates, `settings.json`) — see below.
8. `ssh-keygen -A` (host keys at build time); install `/root/.ssh/authorized_keys` from `--ssh-key` (700/600).
9. **Smoke gate (hard fail):** `chroot … claude --version` must exit 0 **and** emit no `symbol not found` / `Error relocating`. A bad binary never reaches `images/`.
10. Run `--extend` script in chroot if provided.
11. Clean apk cache; unmount in reverse; `e2fsck -fy` + `resize2fs -M` to shrink; print sha256.

**apk package set** (no Node — see ED-4):

```
alpine-base openrc openssh-server git ca-certificates bash \
libgcc libstdc++ ripgrep \
claude-code=<pinned>
```

`bash` is included because claude-code's Bash tool has refused busybox `ash` historically; `/bin/sh` stays busybox.

### Kernel — FC CI prebuilt 6.1.155

- **Source:** `https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/v1.15/x86_64/vmlinux-6.1.155` (uncompressed ELF vmlinux, 44 MB, `CONFIG_HW_RANDOM_VIRTIO=y`). Pinned URL; S3 objects are not deleted.
- **Lands as** `images/vmlinux.bin` (the rooms sibling-of-image convention; `main.rs` derives the kernel as `<image dir>/vmlinux.bin`).
- **`setup-rooms-host.sh`** fetches it (replacing the bionic quickstart kernel) and asserts it is `ELF 64-bit` (not bzImage) + that the paired `.config` has `CONFIG_HW_RANDOM_VIRTIO=y`.
- Build-from-source (firecracker `resources/guest_configs/microvm-kernel-ci-x86_64-6.1.config`) stays documented as a fallback only.

### Boot config (fast openrc)

`/etc/inittab` — ttyS0 only, no virtual gettys:

```
::sysinit:/sbin/openrc sysinit
::sysinit:/sbin/openrc boot
::wait:/sbin/openrc default
ttyS0::respawn:/sbin/getty -L 115200 ttyS0 vt100
::ctrlaltdel:/sbin/reboot
::shutdown:/sbin/openrc shutdown
```

openrc runlevels:

```
# enable
rc-update add devfs sysinit; rc-update add dmesg sysinit
rc-update add procfs boot;   rc-update add sysfs boot
rc-update add bootmisc boot; rc-update add hostname boot
rc-update add sshd default            # the entry point — 'default', not 'boot'
# disable / never add
rc-update del networking boot   # kernel ip= owns eth0
rc-update del hwclock boot      # no RTC in Firecracker
rc-update del modules boot      # all drivers built-in
```

`/etc/rc.conf`: `rc_parallel="YES"` (safe on this small acyclic set; OpenRC 0.55.x in Alpine 3.21 is clear of the 0.62.x parallel-boot regression). Networking is configured by the kernel `ip=` arg; ship a loopback-only `/etc/network/interfaces` so ifupdown never touches eth0.

### SSH / root / DNS / certs

`/etc/ssh/sshd_config` (key-only root, env passthrough, no reverse-DNS stall):

```
PermitRootLogin prohibit-password
PubkeyAuthentication yes
PasswordAuthentication no
AcceptEnv ANTHROPIC_API_KEY
UseDNS no
```

- Host keys generated at build (`ssh-keygen -A`) — mandatory for fast boot (no first-boot entropy stall).
- `/etc/resolv.conf` static: `nameserver 1.1.1.1` / `8.8.8.8`. Nothing rewrites it (no DHCP client runs).
- `apk add ca-certificates` runs the trigger that writes `/etc/ssl/certs/ca-certificates.crt`; build asserts it is non-empty.
- `/root/.claude/settings.json`: `{"env":{"USE_BUILTIN_RIPGREP":"0","DISABLE_AUTOUPDATER":"1"}}`.

### Smoke test — `scripts/test-rootfs-alpine.sh`

Boots the image under Firecracker with the real `ip=` boot args, SSHes in **as root** with `~/.ssh/id_rooms`, asserts (hard-fail):

1. Root SSH (key-only) succeeds; password auth is refused (negative).
2. `AcceptEnv`: `ssh -o SendEnv=ANTHROPIC_API_KEY … 'echo $ANTHROPIC_API_KEY'` round-trips.
3. DNS: `getent hosts github.com` resolves with no manual fix.
4. Tooling: `command -v git claude` resolve; `claude --version` exit 0 and **no** `symbol not found` / `Error relocating` (runtime mirror of the build gate).
5. TLS: `curl -fsS https://api.anthropic.com/ -o /dev/null` (ca bundle works).
6. virtio-rng: `test -e /dev/hwrng` (confirms the python entropy seed is obsolete).
7. Boot-time guard: InstanceStart → first SSH **< 20 s** (warn ≥ 5 s).
8. Size guard: content **< 300 MB** (warn ≥ 270 MB); `images/vmlinux.bin` is ELF.

## Tradeoffs

- **New script vs `--base alpine` flag.** New script. The Alpine path shares almost nothing with debootstrap (minirootfs+chroot vs debootstrap, apk vs apt, openrc vs systemd). Bolting a mode switch onto the noble builder couples two unrelated flows; keep each single-purpose (matches the minimal-core preference).
- **apk repo vs npm.** Official apk `stable` channel: it skips releases with major regressions (the built-in mitigation for the musl-symbol pattern), apk-installed binaries don't auto-update, and it avoids the npm "both glibc+musl optional deps installed" binary-selection bug. npm stays the documented fallback.
- **No Node vs bake Node.** No Node (ED-4) — the native binary needs none, and Node would push the image to ~300 MB+ (at the ceiling).
- **Pin Alpine 3.21 vs latest.** Pin 3.21 — 3.22+ ships OpenRC 0.62.x with a 3× boot regression.

## EDs (engineering decisions)

- **ED-1: Alpine 3.21.7, pinned.** Not `latest-stable` (OpenRC boot regression in 3.22+; size figures quoted against 3.21).
- **ED-2: New `scripts/build-rootfs-alpine.sh`.** Noble `build-rootfs.sh` untouched; retire it later in a separate cleanup PR after the Alpine image soaks (ED-7 / out-of-scope).
- **ED-3: `claude-code` via official Anthropic apk repo, pinned `2.1.148-r1`.** Validated clean on musl (ldd pure-musl, `--version` exit 0). Key sha256 `395759c1…68b6` verified at build. Bump procedure: re-run the smoke gate on the new version, then move the pin.
- **ED-4: No Node in the base image.** claude-code is a native musl binary; Node is unused at runtime and would breach the size budget. A `--extend` hook lets `cursor-sdk-runner` add Node + `@cursor/sdk` itself (mirrors `build-rootfs.sh` ED-6). Decision delegated by the operator 2026-05-29.
- **ED-5: Guest user = root, key-only** (`PermitRootLogin prohibit-password`). Aligns the image to what `runner.rs` already SSHes as (`root@`). This **resolves follow-up #3** without a runner change.
- **ED-6: Kernel = FC CI `vmlinux-6.1.155` (v1.15 path).** virtio-rng built in; obsoletes the python `seed_entropy` path. Build-from-source documented as fallback only.
- **ED-7: SSH host keys baked at build (`ssh-keygen -A`); static resolv.conf; `USE_BUILTIN_RIPGREP=0` + system ripgrep + bash; `DISABLE_AUTOUPDATER=1`.**
- **ED-8: Networking via kernel `ip=` only.** No openrc `networking` service; loopback-only `/etc/network/interfaces`.
- **ED-9: ext4 512 MiB sparse**, shrunk with `resize2fs -M`. Never `strip` the claude binary (self-contained Bun ELF — stripping corrupts it).
- **ED-10: Build-time + runtime smoke gate** on `claude --version` (hard fail on relocation errors) — a musl regression must never reach `images/` or pass CI-of-the-image.

## Runner seams — follow-ups (NOT in this PR)

Per the smaller-reviewable-units convention, two runner-side items are logged in `docs/follow-ups.md`, not bundled here:

- **`seed_entropy` removal.** The 6.1.155 kernel's virtio-rng (+ `random.trust_cpu=on`) seeds the guest CRNG directly, so `runner.rs::seed_entropy` is obsolete. It currently shells a **python** ioctl over SSH — and Alpine ships no python, so `rooms run --command` against the Alpine image would fail at that step until it is removed/guarded. The `--keep` acceptance path and the manual dogfood flow do **not** call `seed_entropy`, so this PR's acceptance is unaffected. The removal belongs in `runner.rs` (natural home: the `cursor-sdk-runner` task, which already edits `runner.rs`).
- **Follow-up #3 (guest-user alignment)** is *resolved* by ED-5 (image accepts key-only root, matching `runner.rs`). No code change; close it once this image is the default.

## Validation

- Build: run `build-rootfs-alpine.sh`; the build-time smoke gate must pass.
- Boot smoke: `scripts/test-rootfs-alpine.sh images/agent-alpine.ext4` (assertions above).
- Dogfood: `rooms run --image images/agent-alpine.ext4 --keep`, then from the host `ssh -i ~/.ssh/id_rooms root@172.16.0.2`, clone a repo + run a small `claude -p` task with `ANTHROPIC_API_KEY` forwarded, confirm a patch is produced.
- `make check` stays green (no Rust changes in this PR).

## Risks

- **R1 — musl regression recurrence.** Mitigated by the `stable` channel (skips regressions) + pinned version + build/runtime smoke gates + `DISABLE_AUTOUPDATER=1`. `--version` is necessary but not sufficient; the dogfood (real agent FS walk) is the full proof.
- **R2 — size near the ceiling.** ~250 MB content; the binary has grown release-to-release. The 512 MiB capacity is headroom; the size guard warns ≥ 270 MB.
- **R3 — Anthropic apk repo / key availability** at build time. Mitigation: npm fallback documented; key sha verified before trust.
- **R4 — kernel S3 dependency.** Pinned URL stays live; the ELF assertion catches a format surprise.
- **R5 — static host keys** identical across VMs from one image. Acceptable for single-tenant ephemeral rooms.

## Out-of-scope

- Nix flake input (separate `nix-flake-input` task).
- Any `runner.rs` change (seed_entropy removal is a follow-up).
- Retiring / deleting the noble `build-rootfs.sh` (separate cleanup PR after soak).
- arm64 / multi-arch (rooms-host is x86_64).
- A package-profile DSL (YAGNI until a second profile exists; the `--extend` hook covers cursor-sdk-runner).

## Implementation plan

1. `scripts/build-rootfs-alpine.sh` + small `lib/rootfs-helpers.sh` reuse; iterate on rooms-host until a built image exists and the build-time smoke gate passes.
2. `scripts/setup-rooms-host.sh`: fetch `vmlinux-6.1.155` → `images/vmlinux.bin` (+ ELF / virtio-rng assertions), keep the bionic kernel as a named backup.
3. `scripts/test-rootfs-alpine.sh` (boot smoke with the 8 assertions).
4. Build on rooms-host; boot via `rooms run --keep`; confirm `/dev/hwrng`, DNS, root SSH, claude, boot time, size.
5. Dogfood a small `claude -p` task end-to-end; produce a patch.
6. Docs: `scripts/README.md` (Alpine builder section), `README.md` pointer, `docs/follow-ups.md` (seed_entropy + #3 entries).
7. PR (Windows side; no `gh` on host). Reviewers: Copilot, `@codex review`, `@claude review`, `@cursor review`.

PR shape: one PR, ~310 weighted LOC, **amazing** band.
