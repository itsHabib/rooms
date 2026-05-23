**Status**: draft
**Owner**: @michael (human:mh)
**Date**: 2026-05-23
**Related**: dossier task `rootfs-builder` (id: `tsk_01KSBE4PGS5THA41HF4EMWE8HW`), [v0 spec](../rooms-v0/spec.md)

# Rootfs builder script (debootstrap) — design spec

## Scope

| Bucket | Files | Est. LOC | Weighted |
|---|---|---|---|
| Production source (1×) | `scripts/build-rootfs.sh`, `scripts/lib/rootfs-helpers.sh` | ~180 | 180 |
| Tests (0.5×) | `scripts/test-rootfs.sh` (smoke; boots the image briefly) | ~60 | 30 |
| Configs / docs (0×) | `scripts/README.md` + main README mention + `.gitignore` for `images/` | ~30 | 0 |
| **Total weighted** | | | **~210** |

Band: **amazing**.

## Goal

A repeatable, documented script that produces the v0 Ubuntu rootfs.ext4 from scratch. POC uses a hand-built image; this task retires that with a deterministic builder so other contributors (and future-you on a different VM) can reproduce it.

No Nix yet — `debootstrap` only. Nix as a deps spec input is the follow-on task (#7); this task gives #7 a reference target.

## Functional

**`scripts/build-rootfs.sh`** (bash, requires root or sudo):

```sh
./scripts/build-rootfs.sh \
  --suite noble \                        # Ubuntu 24.04
  --size 4G \
  --out images/node-dev.ext4
```

Steps:
1. Validate prereqs: `debootstrap`, `mkfs.ext4`, `mount`, `chroot`, `losetup` all present.
2. Allocate `${out}.tmp` (sparse file, `${size}` cap).
3. `mkfs.ext4` on the file.
4. Loop-mount it at a temp dir.
5. `debootstrap --variant=minbase ${suite} ${mount} http://archive.ubuntu.com/ubuntu/`.
6. `chroot ${mount}` and:
   - Set up minimal `/etc/apt/sources.list` (main + universe).
   - `apt update && apt install -y`: `git`, `openssh-server`, `curl`, `ca-certificates`, `gnupg`, `iproute2`, `iputils-ping`.
   - Install Node 20 via NodeSource: `curl -fsSL https://deb.nodesource.com/setup_20.x | bash -; apt install -y nodejs`.
   - `npm install -g @anthropic-ai/claude-code`.
   - (Hook for #4 to extend: `@cursor/sdk` install + `/opt/rooms/cursor-runner/` setup.)
   - Create non-root user `rooms` (uid 1000); `sudo` granted via `/etc/sudoers.d/rooms`.
   - Configure SSH: `/etc/ssh/sshd_config` allows pubkey auth, root login off; install authorized_keys from `--ssh-key <pubkey-path>` arg.
   - Set hostname placeholder (overridden by Firecracker kernel cmdline at boot).
   - Apt cache clean, log truncate.
7. Unmount, detach loop, `mv ${out}.tmp ${out}`.
8. Print sha256 of the output for the operator to record.

**`scripts/lib/rootfs-helpers.sh`** (sourced helpers):
- `assert_root` — exit 1 if not running as root.
- `cleanup_mount` — unmount + detach loop, idempotent, safe in trap.
- `pin_chroot_apt` — write `/etc/apt/apt.conf.d/99rooms` with `Acquire::Retries "3"; APT::Get::Assume-Yes "true";`.

**Idempotency:**
- Running twice with the same `--out` produces an output whose sha256 is identical modulo `apt` timestamp metadata.
- Strategy: `find / -name "*.log" -delete`, truncate `/var/log/apt/`, `apt-get clean`, remove `/var/cache/apt/archives/*.deb`, `find /var/cache -delete` after install.
- "Byte-for-byte identical" is aspirational; "no `apt` lock files, no `/var/log/dpkg.log`, no random tmp" is achievable. Document the residual sources of non-determinism (timestamps in some configs, /etc/machine-id if not handled).

**README docs:**
- `scripts/README.md` covers: prereqs, invocation, output location, sha256 verification.
- Main README "Develop" section: brief pointer to `scripts/build-rootfs.sh` + a default download URL fallback.

**`images/.gitignore`:**
- Ignore `*.ext4` and `*.tmp` so the built image doesn't get accidentally committed.
- Or: use Git LFS; defer the decision per ED-2 below.

## Tradeoffs

- **debootstrap vs nix.** debootstrap is well-trodden, fast, no learning curve; Nix would be more reproducible but is its own task (#7). This task accepts "Ubuntu-shaped, not bit-identical across machines."
- **Minbase variant vs minimal.** Minbase is smaller (~150MB vs ~250MB) but missing more (`sudo`, `iputils-ping`). We add explicitly what we need; smaller blast radius.
- **Idempotent vs reproducible.** True bit-reproducibility is a tar pit; aim for "two runs produce equivalent images modulo timestamps," document the gaps.
- **Store the built image where?** Git LFS adds CI complexity; "build locally" puts friction on new contributors. Pick "build locally + fallback download URL" — see ED-2.

## EDs (engineering decisions)

- **ED-1: Ubuntu 24.04 (noble), variant=minbase.** Modern, supported, smallest reasonable base.
- **ED-2: Built image NOT committed to the repo.** `images/` is gitignored. The script is the source of truth; the artifact is local. Future: if download UX hurts new contributors, publish a tarball to GitHub Releases or S3 and document the fallback URL.
- **ED-3: SSH key is required at build time.** No default `rooms:rooms` password — pubkey only. Operator passes their own pubkey via `--ssh-key`.
- **ED-4: Document idempotency gaps, don't fight them.** Two builds may differ in nanosecond timestamps and `/etc/machine-id`. Acceptable.
- **ED-5: `claude-code` is installed at build time, not at first boot.** First-boot speed matters; trade build-time fatness for boot-time lean.
- **ED-6: Hooks pattern for #4 to extend.** `scripts/build-rootfs.sh` accepts `--extend <script>` that runs inside the chroot after baseline installs. #4 uses this to add `@cursor/sdk` without forking the builder.

## Validation

- **Idempotency**: run the script twice, diff the two outputs (`cmp` or `sha256sum`). Expect non-identical (timestamps), but `tar -tf` of the rootfs should be identical, and `apt list --installed` inside should match.
- **Boot smoke** (`scripts/test-rootfs.sh`): boot the built image in Firecracker for ~30 seconds, SSH in, run `which git`, `which node`, `which claude`, `id rooms`. Assert all exit 0.
- **SSH lockdown**: try password auth → fail; pubkey auth (matching `--ssh-key`) → succeed.
- **Size**: built image under 1.5GB (`du -h`).

## Risks

- **NodeSource setup script.** Pipes curl-to-bash; if NodeSource changes the script, builds break unpredictably. Mitigation: pin to the URL with a comment dated 2026-05; add an `--node-source` override flag for future-proofing.
- **Ubuntu archive flakiness.** `apt update` can fail intermittently. Mitigation: `Acquire::Retries "3"` via `99rooms` apt conf.
- **chroot escape during build.** Bind-mounting `/dev`, `/proc`, `/sys` for the chroot is required; leaving them mounted on the host on script abort is a footgun. Mitigation: `trap` for cleanup.
- **`sudo` required.** debootstrap and loop mounts need root. Document; consider a `--unprivileged` future via `proot` if it becomes an issue.

## Out-of-scope

- Nix flake input (that's #7).
- Multi-arch (arm64) builds — `rooms-host` is x86_64 today; revisit when Apple Silicon Parallels comes back into scope.
- Container image build (Docker / OCI) — `rooms` is microVM-first, not container-first.
- Stripping the kernel — kernel image is a separate concern, handled by the host-setup script.
- A package whitelist DSL for the rootfs — current set is hardcoded; YAGNI until there are multiple profiles.

## Implementation-plan

1. Draft `scripts/build-rootfs.sh` with the steps above. Run it locally; debug until a built image exists.
2. Extract helpers into `scripts/lib/rootfs-helpers.sh` once duplication shows up.
3. Add `images/.gitignore`.
4. Write `scripts/test-rootfs.sh` (boots the image, SSHs in, runs the three `which` checks).
5. Write `scripts/README.md` covering prereqs + invocation + verification.
6. Add a "Building the rootfs" subsection to the main README under "Develop."
7. Retire the POC's hand-built image: delete it from `images/`, update CLAUDE.md if it referenced the manual path.
8. `make check` + manual `scripts/build-rootfs.sh` + `scripts/test-rootfs.sh`.

PR shape: one PR, ~210 weighted LOC. "amazing" band. Reviewers: Copilot, `@codex review`, `@claude review`.

**Sequencing note:** This task is upstream of #4 (cursor-sdk-runner) and #7 (nix-flake-input). The `--extend <script>` hook (ED-6) is what makes #4 non-blocking — #4 amends rootfs via its own extension script rather than modifying this one.
