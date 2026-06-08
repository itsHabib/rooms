**Status**: draft
**Owner**: @michael (human:mh)
**Date**: 2026-05-31
**Related**: dossier task `supply-chain-checksums` (id: `tsk_01KSDN6VY01DKSPX9NPN3ZQVHH`), [docs/follow-ups.md](../../follow-ups.md), retroactive security review 2026-05-24 (finding #2), cursor-sdk-runner / PR #37 follow-up

# Supply-chain checksums — pin + verify every build input — design spec

## Scope

| Bucket | Files | Est. LOC | Weighted |
|---|---|---|---|
| Production source (1×) | `scripts/setup-rooms-host.sh`, `scripts/build-rootfs-alpine.sh`, `scripts/rootfs/install-cursor.sh`, `src/doctor.rs` | ~170 | 170 |
| Configs (0×) | `scripts/checksums.txt`, vendored `package-lock.json` ×2 | ~0 | 0 |
| Tests (0.5×) | doctor sha-drift unit test + a checksum-mismatch bail test | ~70 | 35 |
| Docs (0×) | `scripts/README.md` note on bumping pins | ~20 | 0 |

Band: **amazing** (~205 weighted). Split if it grows: PR-A = `setup-rooms-host.sh` + `checksums.txt` + doctor; PR-B = the two npm lockfiles + the `--extend` asset delivery.

## Goal

`scripts/setup-rooms-host.sh` and the rootfs builders currently fetch-and-execute build inputs (firecracker, kernel, rootfs, rustup, NodeSource, npm packages) with **TLS-only trust and zero integrity verification**. A MITM, or a compromise of any upstream (S3, GitHub Releases, NodeSource, npm), gets **root on `rooms-host`** — firecracker is installed `0755` to `/usr/local/bin`, NodeSource runs as `sudo bash -`, npm-global runs as root. This matters more on the workbench-cloud-driver trajectory, where the script runs on machines the operator doesn't sit at.

Pin every build input by sha256 and verify before use, so a tampered or drifted upstream fails the build with a clear error instead of silently landing on the host.

## Behavior / fix

### `scripts/checksums.txt` (new)
Central, reviewable list of `sha256␠␠path-or-artifact-name` for every pinned artifact at its known-good version. The single place a reviewer audits when a pin is bumped.

### `scripts/setup-rooms-host.sh`
Add `sha256sum -c` (or an explicit `echo "<sha>  <file>" | sha256sum -c`) on every downloaded file **before** `install` / `tar` / `bash`; bail with a clear, actionable error on mismatch. Per site:
- **Firecracker tgz** (GitHub Releases) — firecracker publishes a `.sha256.txt` sidecar; pin the expected digest, don't trust the sidecar blindly.
- **Kernel image** (`spec.ccfc.min` S3) — no published digest; record + pin a sha256 in `checksums.txt`.
- **Rootfs ext4** (`spec.ccfc.min` S3) — same; pin.
- **rustup install script** (`sh.rustup.rs`) — fetch, `sha256sum -c`, then `sh` (no `curl | sh`). Toolchain already `--default-toolchain stable`-pinned.
- **NodeSource setup script** — fetch the script, sha256-check, **then** execute. No inline pipe-to-`sudo bash`.
- **npm `@anthropic-ai/claude-code`** — vendor a committed `package-lock.json` for the claude-code dependency tree; install via `npm ci --strict-peer-deps` from the lockfile, not `npm install -g` against the live registry.

### `scripts/build-rootfs-alpine.sh`
Hardcode the expected sha256 for the Alpine minirootfs of the pinned `ALPINE_VERSION` (today it fetches the `.sha256` sidecar from the same CDN as the tarball — TLS-only, no added integrity), the way `CLAUDE_KEY_SHA256` already pins the Anthropic apk key. (deferred here from PR #36.)

### `scripts/rootfs/install-cursor.sh`
Vendor a committed `package-lock.json` for `@cursor/sdk` and `npm ci` from it, instead of `npm install @cursor/sdk@<ver>` (which resolves transitive deps fresh each build, so two builds can drift). **Delivery constraint:** `build-rootfs-alpine.sh --extend` copies only the single hook script into the chroot, so the lockfile needs either (a) embedding in the hook heredoc, or (b) a small `--extend`-asset mechanism (the builder also stages a sibling file/dir into the chroot). Prefer (b) — it generalizes the `--extend` seam and avoids a giant embedded lockfile; it's the smaller long-term tax. (cursor-sdk-runner / PR #37 follow-up.)

### `src/doctor.rs`
Add a check that re-verifies installed binaries (firecracker, kernel, …) against the expected sha256s from `checksums.txt` on startup. **Warn, don't fail** on drift — catches tampered/updated installs over time without blocking a legitimately-bumped host.

## Acceptance

- `scripts/checksums.txt` exists with a sha256 for each pinned artifact.
- Each download in `setup-rooms-host.sh` is sha-verified before use; a forced mismatch (e.g. edit a pin) makes the script bail non-zero with a message naming the artifact.
- NodeSource + rustup are fetch → verify → execute, never `curl | bash`.
- Both npm sites install via `npm ci` from a committed lockfile, not a live-registry `npm install`.
- `build-rootfs-alpine.sh` verifies the Alpine minirootfs against a hardcoded sha256.
- `rooms doctor` reports a sha-drift check (warn-level); a deliberately-altered binary surfaces as a warning, not a hard fail.
- `make check` green.

## Test plan

- `doctor.rs`: unit test for the sha-drift check — matching digest → ok; mismatched → warn (not fail). Mirror the existing `parse_firecracker_version` test style.
- A checksum-mismatch bail: a small harness (or a documented manual step) confirming `sha256sum -c` failure aborts the script. Host-only / `e2e`-gated if it needs real downloads.

## Non-goals

- A full SLSA/provenance attestation pipeline — sha-pinning is the v0 mitigation, not signed provenance.
- Auto-bumping pins on upstream releases (operator bumps `checksums.txt` deliberately, like `CLAUDE_VERSION`).
- `nix`-based reproducible builds (`nix-flake-input` is parked; this task is the cheaper, Alpine-native reproducibility win).
- Sandboxing/jailing the install itself (`firecracker-under-jailer` is its own task).

## Validation / drive notes

Local, hands-on stream (like cursor-sdk-runner): the changes touch host setup scripts + the rootfs builders, so verifying a forced-mismatch bail and the doctor drift check is done on `rooms-host`, not a cloud VM. Reconcile any drift between this spec and the current scripts before implementing (the builders moved to Alpine since the task was written).
