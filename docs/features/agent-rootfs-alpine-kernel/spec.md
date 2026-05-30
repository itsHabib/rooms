**Status**: implemented (validated on rooms-host 2026-05-29)
**Owner**: @michael (human:mh)
**Date**: 2026-05-29
**Related**: dossier task `agent-rootfs-alpine-kernel` (id: `tsk_01KSS57NYAAW3NBFEFW6BSHJMF`), [v0 spec](../rooms-v0/spec.md), [rootfs-builder](../rootfs-builder/spec.md), [cursor-sdk-runner](../cursor-sdk-runner/spec.md), [follow-ups](../../follow-ups.md)

# Alpine agent rootfs + Firecracker-tuned kernel — design spec

## Scope

| Bucket | Files | Est. LOC | Weighted |
|---|---|---|---|
| Production source (1×) | `scripts/build-rootfs-alpine.sh` (~250), `scripts/setup-rooms-host.sh` kernel-fetch change (~15) | ~265 | 265 |
| Tests (0.5×) | `scripts/test-rootfs-alpine.sh` (boot smoke; ~110) | ~110 | 55 |
| Configs / docs (0×) | `.gitattributes`, `scripts/README.md`, `README.md`, `docs/follow-ups.md`, this spec | ~60 | 0 |
| **Total weighted** | | | **~320** |

Band: **amazing** (< 500). No Rust changes — `make check` is unaffected.

## Goal

Replace the artisanal Ubuntu-noble debootstrap image (`build-rootfs.sh`) with a lean **Alpine 3.21** (musl / busybox / openrc — no systemd) agent rootfs, paired with a **Firecracker-tuned guest kernel** (6.1.155, virtio-rng built in). The image bakes openssh (key-only login for the unprivileged **`rooms`** user, `AcceptEnv ANTHROPIC_API_KEY`), git, ca-certificates, curl, and `claude-code` (native musl binary). DNS works out of the box, boot is ~1.7 s to sshd, and the image is ~276 MB — well under 300 MB.

> Acceptance (from the dossier task) — all met on rooms-host: `rooms run --image agent-alpine.ext4 --keep` boots in **1.7 s**, sshd reachable, `getent hosts github.com` resolves with no manual fix; claude-code runs in-guest; `git clone` over https works; image (276 MB) well under the 300 MB noble image. A `claude -p` dogfood produced a real `README.md` patch.

## Why now (validated)

The first end-to-end dogfood (2026-05-29, PR #34; seams logged in `docs/follow-ups.md`) proved the substrate works and pinned the rootfs as the weak link:

1. noble's **systemd-resolved** clobbered the baked `/etc/resolv.conf` → guest DNS broke. Alpine has no systemd-resolved; a static resolv.conf just persists. **Confirmed fixed** — `getent hosts github.com` resolves on first boot.
2. noble booted **~105 s** on the bionic-quickstart `vmlinux.bin` (Linux 4.14, no `random.trust_cpu`, no virtio-rng). **Confirmed fixed** — Alpine + the 6.1.155 kernel reaches sshd in ~1.7 s.
3. the noble image was **300 MB+**. **Confirmed fixed** — 276 MB.

**De-risk spike + as-built validation (rooms-host, 2026-05-29):**

- `apk add claude-code` from the official Anthropic apk repo (`stable` channel) installs **2.1.148-r1**; `claude --version` → exit 0, no relocation errors. `ldd /usr/bin/claude` links **only** against musl (`ld-musl-x86_64.so.1`) — zero glibc dependency. The recurring `posix_getdents` glibc-symbol regression is **not** present in this stable build.
- claude binary = **220 MB**; full image (50 apk packages, **no Node**) = **276 MB allocated** / 512 MB capacity.
- Anthropic apk signing-key sha256 `395759c1…68b6` verified against the official docs and at build time.
- The FC CI kernel `firecracker-ci/v1.15/x86_64/vmlinux-6.1.155` (44 MB, uncompressed ELF, `CONFIG_HW_RANDOM_VIRTIO=y`) boots Alpine and exposes `/dev/hwrng` — so the python `seed_entropy` path is obsolete.
- **claude-code refuses `--dangerously-skip-permissions` as root** → the agent must run as a non-root user (the noble image was right to use `rooms`). The image runs the agent as `rooms` (uid 1000).
- **busybox `adduser -D` password-locks the account (`!`)**, which Alpine's PAM-less sshd rejects even for pubkey auth; the build switches the shadow field to `*` (disabled, not locked), matching root's working entry.

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

1. Validate prereqs; require `--ssh-key`.
2. Fetch + **sha256-verify** the pinned `alpine-minirootfs-3.21.7-x86_64.tar.gz` (read the `.sha256` sidecar; abort on mismatch).
3. `truncate -s 512M` sparse file → `mkfs.ext4` → loop-mount → `tar -x` the minirootfs.
4. Bind `/dev /proc /sys`; copy host `/etc/resolv.conf` for the apk/key fetch.
5. In chroot: pin `/etc/apk/repositories` to `v3.21` main + community; `apk add` the package set (below); add the Anthropic apk key (**verify sha256**) + repo; `apk add claude-code=2.1.148-r1`.
6. Write guest config: `/etc/inittab`, static `/etc/resolv.conf`, loopback-only `/etc/network/interfaces`, hostname, `rc_parallel`.
7. Harden sshd (host-side `sed`): `PermitRootLogin no`, `PubkeyAuthentication yes`, `PasswordAuthentication no`, `UseDNS no`, `AcceptEnv ANTHROPIC_API_KEY`.
8. In chroot: create the `rooms` user (uid 1000) + NOPASSWD sudo, **unlock its shadow (`!` → `*`)**, pre-create `/workspace` owned by `rooms`, enable openrc services, `ssh-keygen -A`, populate the CA bundle.
9. Host-side: install `/home/rooms/.ssh/authorized_keys` (from `--ssh-key`) and `/home/rooms/.claude/settings.json`, owned by uid 1000.
10. **Smoke gate (hard fail):** `chroot … claude --version` must exit 0 with no `symbol not found` / `Error relocating`.
11. Run `--extend` script if provided; unmount; `mv` into place; print sha256.

**apk package set** (no Node — see ED-4):

```
alpine-base openrc openssh-server sudo \
git ca-certificates bash curl \
libgcc libstdc++ ripgrep \
claude-code=<pinned>
```

`bash` is included because claude-code's Bash tool has refused busybox `ash` historically; `/bin/sh` stays busybox. `curl` is cheap (`libcurl` is already pulled by git) and useful to in-room agents.

### Kernel — FC CI prebuilt 6.1.155

- **Source:** `https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/v1.15/x86_64/vmlinux-6.1.155` (uncompressed ELF vmlinux, 44 MB, `CONFIG_HW_RANDOM_VIRTIO=y`). Pinned URL; S3 objects are not deleted.
- **Lands as** `images/vmlinux.bin` (`main.rs` derives the kernel as `<image dir>/vmlinux.bin`).
- **`setup-rooms-host.sh`** fetches it (replacing the bionic quickstart kernel) and asserts it is an `ELF 64-bit` vmlinux (not bzImage).
- Build-from-source (firecracker `resources/guest_configs/microvm-kernel-ci-x86_64-6.1.config`) stays a documented fallback only.

### Boot config (fast openrc)

`/etc/inittab` — ttyS0 only, no virtual gettys. openrc runlevels: enable `devfs`/`dmesg` (sysinit), `procfs`/`sysfs`/`bootmisc`/`hostname` (boot), `sshd` (default); **never** add `networking` (kernel `ip=` owns eth0), `hwclock`, or `modules`. `rc_parallel="YES"` (OpenRC 0.55.x in Alpine 3.21 is clear of the 0.62.x parallel-boot regression). A loopback-only `/etc/network/interfaces` keeps ifupdown off eth0.

### SSH / user / DNS / certs

`/etc/ssh/sshd_config`: `PermitRootLogin no`, `PubkeyAuthentication yes`, `PasswordAuthentication no`, `UseDNS no` (avoids a ~5 s reverse-DNS stall on connect), `AcceptEnv ANTHROPIC_API_KEY`. Host keys generated at build (`ssh-keygen -A`) so first boot doesn't stall on entropy. The `rooms` user's `authorized_keys` comes from `--ssh-key`; static `/etc/resolv.conf` (`1.1.1.1` / `8.8.8.8`); `ca-certificates` trigger populates `/etc/ssl/certs/ca-certificates.crt` (build asserts it is non-empty); `/home/rooms/.claude/settings.json` sets `USE_BUILTIN_RIPGREP=0` + `DISABLE_AUTOUPDATER=1`.

### Smoke test — `scripts/test-rootfs-alpine.sh`

Boots the image under Firecracker with the real `ip=` boot args, SSHes in **as `rooms`** with `~/.ssh/id_rooms`, and hard-fails on any of: (1) sshd not reachable; (2) wrong user / git or claude missing; (3) `getent hosts github.com` fails; (4) `/dev/hwrng` missing; (5) `claude --version` errors or shows `symbol not found`/`Error relocating`; (6) TLS to `api.anthropic.com` fails cert verification (a non-2xx HTTP status still passes — it proves the handshake); (7) `sshd -T` reports `passwordauthentication` ≠ `no`; (8) allocated size ≥ 300 MB. Warns if boot-to-sshd ≥ 5 s.

## Tradeoffs

- **New script vs `--base alpine` flag.** New script — the Alpine path shares almost nothing with debootstrap (minirootfs+chroot, apk, openrc). Keep each builder single-purpose.
- **apk repo vs npm.** Official apk `stable` channel: it skips releases with major regressions (the built-in mitigation for the musl-symbol pattern), apk-installed binaries don't auto-update, and it avoids the npm "both glibc+musl optional deps installed" binary-selection bug. npm stays a documented fallback.
- **No Node vs bake Node.** No Node (ED-4) — the native binary needs none, and Node would push the image to ~300 MB+.
- **Non-root agent user.** Required, not optional: claude-code refuses to skip permissions as root.

## EDs (engineering decisions)

- **ED-1: Alpine 3.21.7, pinned.** Not `latest-stable` (OpenRC boot regression in 3.22+).
- **ED-2: New `scripts/build-rootfs-alpine.sh`.** Noble `build-rootfs.sh` untouched; retire it later in a separate cleanup PR after the Alpine image soaks.
- **ED-3: `claude-code` via official Anthropic apk repo, pinned `2.1.148-r1`.** Validated clean on musl (ldd pure-musl, `--version` exit 0). Key sha256 `395759c1…68b6` verified at build. Bump procedure: re-run the smoke gate on the new version, then move the pin.
- **ED-4: No Node in the base image.** claude-code is a native musl binary; Node is unused at runtime and would breach the size budget. A `--extend` hook lets `cursor-sdk-runner` add Node + `@cursor/sdk` (mirrors `build-rootfs.sh` ED-6). Decision delegated by the operator 2026-05-29.
- **ED-5: Guest user = non-root `rooms` (uid 1000), key-only** (`PermitRootLogin no`). claude-code refuses `--dangerously-skip-permissions` as root, so the agent runs unprivileged (NOPASSWD sudo, owns `/workspace`). busybox `adduser -D` locks the account (`!`); the build switches it to `*` so Alpine's PAM-less sshd accepts pubkey login. This does **not** resolve follow-up #3 by itself — the runner must SSH as `rooms@` (see Runner seams).
- **ED-6: Kernel = FC CI `vmlinux-6.1.155` (v1.15 path).** virtio-rng built in; obsoletes the python `seed_entropy` path.
- **ED-7: SSH host keys baked at build; static resolv.conf; `USE_BUILTIN_RIPGREP=0` + system ripgrep + bash; `DISABLE_AUTOUPDATER=1`.**
- **ED-8: Networking via kernel `ip=` only.** No openrc `networking` service; loopback-only `/etc/network/interfaces`.
- **ED-9: ext4 512 MiB sparse, NOT shrunk.** The unused capacity is deliberate headroom for the agent workspace (`/workspace`, ~197 MB free after the ~276 MB of content). The image stays sparse, so allocated size tracks content.
- **ED-10: Build-time + runtime smoke gate** on `claude --version` (hard fail on relocation errors).

## Runner seams — follow-ups (NOT in this PR)

The image is fully exercised via `rooms run --keep` + manual `rooms@` SSH (acceptance + dogfood). Driving it through `rooms run --command` / `--repo` needs two `runner.rs` changes, logged in `docs/follow-ups.md` and naturally owned by the `cursor-sdk-runner` task (which already edits `runner.rs`):

- **Runner SSH user (resolves #3).** `runner.rs` SSHes as `root@`; this image accepts key-only login only for `rooms@` (claude can't run as root). The runner must target the agent user (`rooms@`). Until then, `rooms run --command` can't drive the Alpine image (manual `rooms@` SSH works — that's how it was dogfooded).
- **Remove the python `seed_entropy`.** The 6.1.155 kernel's virtio-rng (+ `random.trust_cpu=on`) seeds the guest CRNG directly (`/dev/hwrng` present). `runner.rs::seed_entropy` shells a **python** ioctl over SSH — Alpine has no python, so that step would fail. It is now unnecessary; remove/guard it.

## Validation (as run)

- Build: `build-rootfs-alpine.sh` → 276 MB image; build-time smoke gate passed.
- Boot smoke: `test-rootfs-alpine.sh` → all 8 assertions pass; sshd reachable in ~1.7 s.
- Dogfood: `rooms run --keep` + `ssh rooms@172.16.0.2`, https clone + `claude -p` (skip-permissions, `ANTHROPIC_API_KEY` forwarded via `SendEnv`) → produced a `README.md` diff.
- `make check`: unaffected (no Rust changes).

## Risks

- **R1 — musl regression recurrence.** Mitigated by the `stable` channel + pinned version + build/runtime smoke gates + `DISABLE_AUTOUPDATER=1`. The dogfood (real agent FS walk) is the full proof beyond `--version`.
- **R2 — size near the ceiling.** ~276 MB; the binary grows release-to-release. The 512 MiB capacity is headroom; the smoke guard warns ≥ 270 MB.
- **R3 — Anthropic apk repo / key availability** at build time. Mitigation: npm fallback documented; key sha verified before trust.
- **R4 — kernel S3 dependency.** Pinned URL stays live; the ELF assertion catches a format surprise.
- **R5 — static host keys** identical across VMs from one image. Acceptable for single-tenant ephemeral rooms.

## Out-of-scope

- Nix flake input (separate `nix-flake-input` task).
- Any `runner.rs` change (the two seams above are follow-ups).
- Retiring / deleting the noble `build-rootfs.sh` (separate cleanup PR after soak).
- arm64 / multi-arch (rooms-host is x86_64).
- A package-profile DSL (the `--extend` hook covers cursor-sdk-runner).

## PR shape

One PR, ~320 weighted LOC, **amazing** band. Reviewers: Copilot, `@codex review`, `@claude review`, `@cursor review`.
