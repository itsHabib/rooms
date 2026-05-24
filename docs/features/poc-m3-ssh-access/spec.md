**Status**: draft
**Owner**: @michael (human:mh)
**Date**: 2026-05-24
**Related**: dossier task `poc-m3-ssh-access` (id: `tsk_01KSC5RTCNSW5G8MYA97JBFPCG`), [rooms v0 spec](../rooms-v0/spec.md), POC phase [`00-poc-implementation`](../../../README.md)

# POC m3: SSH access into the room (rootfs key bake)

## Scope

| Bucket | Files | Est. LOC | Weighted |
|---|---|---|---|
| Production source (1×) | — | 0 | 0 |
| Scripts (1×) | `scripts/bake-rootfs-ssh.sh` | ~100 | 100 |
| Docs (0×) | `README.md` (bootstrap section) | ~20 | 0 |
| **Total weighted** | | | **~100** |

Band: **amazing**. No Rust changes — entirely a bash helper + docs.

## Goal

Make pubkey SSH to a booted microVM work (`ssh -i ~/.ssh/id_rooms root@172.16.0.2`).

The quickstart bionic rootfs ships with sshd installed and auto-started (we saw `[ OK ] Started OpenBSD Secure Shell server` in m1's boot output), but no `authorized_keys` and root password login disabled — so SSH connections currently get `Permission denied`. This task bakes our pubkey into the rootfs so pubkey auth works. The key lives at the dedicated path `~/.ssh/id_rooms` (not OpenSSH's default `~/.ssh/id_*`), so all SSH invocations must pass `-i ~/.ssh/id_rooms` explicitly until task #6's proper rootfs builder + ssh-config wrapping lands.

After this lands, the m4 milestone (curl Anthropic from inside) is trivial: `ssh root@172.16.0.2 "curl ..."`.

## Functional

**New script: `scripts/bake-rootfs-ssh.sh`** (~120 LOC bash).

Invocation:

```sh
bash scripts/bake-rootfs-ssh.sh [<rootfs-path>]
# Default rootfs-path: ~/rooms/images/rootfs.ext4
# Env override: KEY_PATH (default ~/.ssh/id_rooms), matches setup-tap.sh's pattern
```

**Starting script header** (the committed `scripts/bake-rootfs-ssh.sh` is authoritative; this snippet was the v2 spec's draft and has since drifted as review surfaced refinements — separate INT/TERM traps to preserve signal codes, `sudo` on the losetup probe, EUID-0 refusal at the top, etc.). The hard requirements remain: `set -euo pipefail`, early `MNT=""` / `LOOP=""` declarations, idempotent cleanup, and explicit signal-aware exit codes.

```sh
#!/usr/bin/env bash
set -euo pipefail

ROOTFS="${1:-$HOME/rooms/images/rootfs.ext4}"
KEY_PATH="${KEY_PATH:-$HOME/.ssh/id_rooms}"
PUB_PATH="${KEY_PATH}.pub"

# Declare early so the trap (registered before any losetup/mount) can
# reference them without tripping `set -u`.
MNT=""
LOOP=""

log()   { printf '\033[1;34m[bake-rootfs-ssh]\033[0m %s\n' "$*"; }
fatal() { printf '\033[1;31m[bake-rootfs-ssh]\033[0m %s\n' "$*" >&2; exit 1; }

cleanup() {
    local code=$?
    if [[ -n "$MNT" ]] && mountpoint -q "$MNT"; then
        sudo umount "$MNT" || log "warn: umount $MNT failed (may already be unmounted)"
    fi
    if [[ -n "$LOOP" ]] && losetup "$LOOP" >/dev/null 2>&1; then
        sudo losetup -d "$LOOP" || log "warn: losetup -d $LOOP failed"
    fi
    if [[ -n "$MNT" && -d "$MNT" ]]; then
        rmdir "$MNT" 2>/dev/null || true
    fi
    exit "$code"
}
trap cleanup EXIT INT TERM
```

`set -euo pipefail` is **mandatory** — without it, a failed `sudo mount` followed by chmod-on-host-`/root/.ssh` is a catastrophic silent footgun.

Behavior, step by step:

### 1. Validate prereqs

Check that each of these is on PATH; `fatal` with the apt install command if any is missing:
`sudo mount mountpoint losetup ssh-keygen sed grep tee e2fsck awk`.

(`shellcheck` is a lint-time tool gated by `make check` / CI separately; not a runtime prereq of the bake script. Earlier draft hard-enforced it here and broke fresh-host bootstraps that hadn't installed it.)

### 2. Argument validation

`ROOTFS` must:
- exist (`[[ -f "$ROOTFS" ]]`)
- be writable (`[[ -w "$ROOTFS" ]]`)
- **not already be attached to a loop device** — check `losetup -j "$ROOTFS"`. If output is non-empty, `fatal` with the existing loop name and instructions to `sudo losetup -d <name>` first. This catches stale attachments from a previous crashed bake.

### 3. Preflight safety check

The script does **not** verify the microVM is shut down (no reliable cross-process check; firecracker holds the file open directly, not via `losetup`, so `lsof` and `losetup -j` both miss it). Print an explicit warning to stderr:

```
[bake-rootfs-ssh] WARNING: any microVM using this rootfs MUST be shut down
                  before bake. Mounting a live RW ext4 from another writer
                  corrupts it. Press Ctrl-C now if a VM is running; otherwise
                  the script will continue in 5 seconds...
```

Then `sleep 5` before proceeding. POC-grade safety; productionization (#2 / #6) can add real locking.

### 4. Host-side SSH key

```sh
if [[ -f "$KEY_PATH" && -f "$PUB_PATH" ]]; then
    log "reusing existing keypair at $KEY_PATH (created $(stat -c %y "$KEY_PATH"))"
else
    log "generating ed25519 keypair at $KEY_PATH"
    ssh-keygen -t ed25519 -N "" -f "$KEY_PATH" -C "rooms-microvm" >/dev/null
fi
```

### 5. Loop-mount the rootfs

Order matters — losetup BEFORE the trap actually has something to clean, but the trap is already registered (step 0):

```sh
MNT="$(mktemp -d -t rooms-bake.XXXXXX)"
log "loop-attaching $ROOTFS"
LOOP="$(sudo losetup -f --show "$ROOTFS")"
log "mounting $LOOP -> $MNT"
sudo mount "$LOOP" "$MNT"
```

### 6. Bake the key into the mounted rootfs (NO chroot)

**Do NOT invoke `chroot`.** Direct file writes from the host into `$MNT/...` are simpler, safer, and avoid ld.so / glibc mismatches between the bionic guest and the noble host. The section is named "into the mounted rootfs," not "into the chroot," for exactly this reason.

```sh
log "preparing /root/.ssh in rootfs"
sudo mkdir -p "$MNT/root/.ssh"
sudo chown 0:0 "$MNT/root/.ssh"
sudo chmod 700 "$MNT/root/.ssh"

AK="$MNT/root/.ssh/authorized_keys"
sudo touch "$AK"
sudo chown 0:0 "$AK"
sudo chmod 600 "$AK"

PUBKEY="$(cat "$PUB_PATH")"
if sudo grep -qxF "$PUBKEY" "$AK"; then
    log "pubkey already present in authorized_keys"
else
    log "appending pubkey to authorized_keys"
    echo "$PUBKEY" | sudo tee -a "$AK" >/dev/null
fi
```

### 7. Configure sshd (idempotent, handles bionic's commented defaults)

**Critical:** bionic's `/etc/ssh/sshd_config` ships with directives **commented out** by default (e.g. `#PermitRootLogin prohibit-password`). A naive `sed 's/^PermitRootLogin/.../'` silently misses the comment and falls through to the OpenSSH compiled-in default. Use match-or-append per directive:

```sh
CONFIG="$MNT/etc/ssh/sshd_config"

set_directive() {
    local dir="$1" val="$2"
    if sudo grep -qE "^${dir}[[:space:]]+${val}\$" "$CONFIG"; then
        log "$dir already = $val"
    elif sudo grep -qE "^${dir}[[:space:]]" "$CONFIG"; then
        log "$dir present with wrong value; replacing"
        sudo sed -i.bak.rooms "s|^${dir}[[:space:]].*|${dir} ${val}|" "$CONFIG"
    else
        log "$dir missing or commented; appending"
        echo "${dir} ${val}" | sudo tee -a "$CONFIG" >/dev/null
    fi
}

set_directive PermitRootLogin yes
set_directive PubkeyAuthentication yes
set_directive PasswordAuthentication no
```

Three properties this gives us:
- Idempotent: re-runs see the directive already at the target value, no-op.
- Handles bionic's commented defaults (falls into the "missing" branch, appends a fresh uncommented line — the commented one is ignored by sshd).
- The `.bak.rooms` file from the sed `replace` branch is overwritten on each run. Not a "backup chain"; just a single-file snapshot of the previous state. Fine.

### 8. Sync + unmount + fsck

```sh
sync                                # flush kernel buffers to the loop device
sudo umount "$MNT"
MNT=""                              # mark cleanup-already-handled
sudo e2fsck -fy "$LOOP"             # repair any unclean state before detach
sudo losetup -d "$LOOP"
LOOP=""
```

`MNT=""` / `LOOP=""` after explicit cleanup tells the trap not to double-clean.

### 9. Final logging

```sh
log "done."
log "    pubkey baked into:  $ROOTFS"
log "    private key:        $KEY_PATH"
log "    verify after boot:  ssh -i $KEY_PATH -o StrictHostKeyChecking=accept-new -o UserKnownHostsFile=/dev/null root@172.16.0.2 'uname -a'"
```

The `UserKnownHostsFile=/dev/null` is **mandatory** for verification — bionic regenerates host keys on each boot (no baked-in static keys), so `~/.ssh/known_hosts` would accumulate stale entries. Task #6 (`rootfs-builder`) will bake static host keys; until then, every SSH command must include the flag.

### Idempotency contract

Running the script twice in a row with the same `KEY_PATH` and `ROOTFS` must:
- Not duplicate the pubkey in `authorized_keys` (the `grep -qxF` check guarantees this).
- Not duplicate `sshd_config` directives (the match-or-append in §7 guarantees this).
- Not regenerate the SSH keypair (the file-exists check in §4 guarantees this).
- Produce no errors, exit 0.

## Tradeoffs

- **Modifying the quickstart rootfs (the "dirty path").** We're surgically editing a 50 MB ext4 file that was downloaded by `setup-rooms-host.sh`. Task #6 (`rootfs-builder`) will replace this with a debootstrap-built image that bakes the key at build time, retiring this script. Accepted because m3 needs to land **before** #6 to unblock m4.
- **Bash script, not Rust.** Mount/chroot/sed is bread-and-butter sysadmin work; Rust adds nothing here and would force shelling out to all the same commands anyway. The Rust side stays the boot-control plane; the rootfs preparation lives in `scripts/`.
- **`PermitRootLogin yes`.** Production would use a non-root user with sudo. POC accepts root login because the quickstart rootfs's only meaningful user is root, and creating a user via chroot is fiddly. Task #6 (`rootfs-builder`) creates a non-root `rooms` user from scratch.
- **Hardcoded key path `~/.ssh/id_rooms`.** A dedicated key (not the operator's general-purpose `~/.ssh/id_ed25519`) so the rooms key has its own lifecycle and isn't tied to GitHub or anything else. Could be overridable via `--key <path>` but YAGNI for POC.

## EDs (engineering decisions)

- **ED-1: Host SSH key lives at `~/.ssh/id_rooms`.** Dedicated key, ed25519, no passphrase. Script generates on first run. `KEY_PATH` env var overrides for CI / non-default cases (matches `setup-tap.sh`'s `TAP=` env pattern).
- **ED-2: Bash script, not Rust.** Matches `setup-rooms-host.sh` and `setup-tap.sh` precedent.
- **ED-3: Match-or-append for sshd_config directives (three-branch).** §7 spec — handles commented bionic defaults that a naive sed would miss. Pubkey idempotency via `grep -qxF`.
- **ED-4: Trap-based cleanup with early variable declaration.** `MNT=""` / `LOOP=""` declared before the trap so `set -u` doesn't trip on an early-fail cleanup. Trap registered before any losetup/mount.
- **ED-5: NO chroot.** Direct file writes from host into `$MNT/...`. Avoids bionic-guest-on-noble-host ld.so issues + simpler. The §6 section heading is "into the mounted rootfs" — cursor must NOT invoke `chroot`.
- **ED-6: `PermitRootLogin yes`.** Acknowledged tradeoff; replaced by non-root user in task #6.
- **ED-7: Defense-in-depth `PasswordAuthentication no`.** Even if the rootfs has a default root password (it doesn't — bionic quickstart has no root password), we explicitly disable password auth so only pubkey works.
- **ED-8: `e2fsck -fy` after unmount, before loop-detach.** Repairs any unclean state from a crashed previous run. `-fy` = force + assume-yes (POC; productionization may want `-n` first to report-only).
- **ED-9: 5-second pre-mount warning, no enforcement.** No reliable cross-process "is the VM running" check (firecracker doesn't use `losetup`; `lsof` is TOCTOU). Procedural safety wins for POC.
- **ED-10: `shellcheck` is a hard validation gate.** First validation step is `shellcheck scripts/bake-rootfs-ssh.sh`. PR is not opened until it passes clean. Matches the rigor of `make check` on the Rust side.

## Validation

The agent implementing this MUST run all of these on the rooms-host VM and confirm they pass before opening the PR. Capture the SSH round-trip output (step 4) in the PR description.

1. **Lint.** `shellcheck scripts/bake-rootfs-ssh.sh` exits 0 with no warnings. This catches the unset-var-in-trap / quoting / globbing class of bugs that bit POC m2.

2. **First run, clean state.** No `~/.ssh/id_rooms` present (move out of the way if it is):
   ```sh
   bash scripts/bake-rootfs-ssh.sh
   ```
   Expected: 5-second warning + sleep, key generated, rootfs mounted/edited/unmounted, `e2fsck -fy` reports clean, loop detached, no leftover state, exit 0.
   Verify cleanup: `losetup -a` shows no rooms-related attachments; `mount | grep rooms` empty.

3. **Boot smoke.** Re-boot the microVM with the edited rootfs (m2's networking still up):
   ```sh
   cargo run --quiet -- create --image ~/rooms/images/rootfs.ext4 --repo . --keep
   ```
   Expected: same clean systemd boot output as before m3, sshd starts cleanly. (If sshd fails to start, check `journalctl -u ssh` inside the guest via ttyS0 auto-login.)

4. **SSH round-trip.** In a second shell while the VM is running:
   ```sh
   ssh -i ~/.ssh/id_rooms -o StrictHostKeyChecking=accept-new -o UserKnownHostsFile=/dev/null root@172.16.0.2 "uname -a; cat /etc/os-release | head -2"
   ```
   Expected output includes `Linux ubuntu-fc-uvm 4.14.174` and `NAME="Ubuntu" / VERSION="18.04.5 LTS (Bionic Beaver)"`. SSH exit code 0. **Paste this exact output into the PR description.**

5. **Idempotency.** Shut down VM (Ctrl-C the keep'd `rooms create`). Re-run the bake script:
   ```sh
   bash scripts/bake-rootfs-ssh.sh
   ```
   Expected: every log line is "already present" or "already = yes/no"; no append branch fires. Exit 0.
   Verify: mount the rootfs separately into a fresh tempdir, count occurrences:
   ```sh
   TMP=$(mktemp -d); LP=$(sudo losetup -f --show ~/rooms/images/rootfs.ext4); sudo mount "$LP" "$TMP"
   sudo grep -c "rooms-microvm" "$TMP/root/.ssh/authorized_keys"   # must be exactly 1
   sudo grep -cE "^PermitRootLogin yes\$" "$TMP/etc/ssh/sshd_config"  # must be exactly 1
   sudo umount "$TMP"; sudo losetup -d "$LP"; rmdir "$TMP"
   ```

6. **Idempotency boot.** Re-boot the VM after the second bake; repeat step 4. Must succeed identically.

7. **`make check`.** From repo root: `source ~/.cargo/env && make check`. Bash-only change shouldn't touch Rust, but confirm.

If any step fails, do NOT open the PR; fix and re-validate from step 1.

## Risks

- **Mounting an in-use rootfs corrupts it.** No clean cross-process check (firecracker holds the file directly, bypassing `losetup`; `lsof` would catch but adds a TOCTOU window). Mitigation is procedural: the 5-second pre-mount warning in §3 gives the operator a window to abort. `losetup -j` check catches stale loop attachments from previous crashed bakes. Productionization (#2) can add real locking via a sidecar file or fcntl.
- **`sshd_config` sed misfires on bionic's commented defaults.** Caught by the spec — the §7 match-or-append pattern explicitly handles "uncommented & correct" / "uncommented & wrong" / "commented or missing" as three distinct branches. A naive `sed 's/^Directive .*/Directive yes/'` would silently miss bionic's `#Directive ...` defaults; the spec's three-branch logic avoids this.
- **Loop devices leak on crash.** Mitigated by the §0 trap (registered before any losetup) + the §2 `losetup -j` startup check that refuses to run if a stale attachment exists.
- **`set -u` trips in trap on early failure.** Mitigated by declaring `MNT=""` and `LOOP=""` at the top of the script so the trap can safely reference them when cleanup fires before they're populated.
- **SSH host key changes between boots.** Bionic regenerates host keys on each boot (no static keys baked in). Every operator SSH must use `-o UserKnownHostsFile=/dev/null` until task #6 (`rootfs-builder`) bakes static keys. The script's final log line shows the full SSH command with this flag so the operator copies it correctly.
- **`e2fsck -fy` reports unrepairable corruption.** Mitigated by the §3 pre-mount warning (most likely cause is in-use mount). If `e2fsck` fails, the operator needs to re-download the quickstart rootfs (`rm ~/rooms/images/rootfs.ext4 && bash scripts/setup-rooms-host.sh`).
- **Catastrophic silent footgun from missing `set -e`.** Mitigated by the §0 mandatory script header — `set -euo pipefail` is non-negotiable. Cursor must NOT remove or weaken it.

## Out-of-scope (deferred to future tasks)

- **Per-room dynamic SSH keys.** POC: one shared key for all rooms. Task #2 (`harden-firecracker-control`) can add per-room keys when needed.
- **Rust-driven SSH invocation from the `rooms` binary.** For m3, SSH is something the operator (or m4) does manually. A future `rooms exec <room_id> -- <cmd>` command will own this end-to-end.
- **Replacing the quickstart rootfs with a debootstrap build.** That's task #6 (`rootfs-builder`); it retires this script.
- **Non-root user inside the guest.** Task #6.
- **Disabling `PermitRootLogin yes`.** Task #6.

## Implementation plan

1. Write `scripts/bake-rootfs-ssh.sh` per the Functional section. The script header in §0 is **mandatory and copy-paste verbatim** (no edits to `set -euo pipefail`, the trap, or the early MNT/LOOP declarations). The rest of the body follows §§1–9.
2. Make the script executable (`chmod +x scripts/bake-rootfs-ssh.sh`).
3. Run `shellcheck scripts/bake-rootfs-ssh.sh` until it exits clean with no warnings.
4. Run the 7 validation steps on the rooms-host VM. Capture the SSH round-trip output (step 4) for the PR description.
5. Add a brief subsection to `README.md` → "Prereqs" / "Inside the Ubuntu VM" that mentions running `bash scripts/bake-rootfs-ssh.sh` once during host setup (between `setup-rooms-host.sh` and `setup-tap.sh`). One paragraph, no deep details.
6. Commit on branch `m3-ssh-access`.
7. Push, open PR. Request reviewers: Copilot, `@codex review`, `@claude review`. Iterate per the standard review cycle (~3 rounds before reaching out). PR body must include the captured SSH round-trip output from step 4 as proof of end-to-end success.

PR shape: one PR, ~120 weighted LOC. "amazing" band.

**Branch:** `m3-ssh-access`.

**Spec path for `ship.ship`:** `docs/features/poc-m3-ssh-access/spec.md`, relative to the worktree root.
