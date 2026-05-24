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
| Docs (0×) | `README.md` (bootstrap section), brief `scripts/README.md` mention | ~20 | 0 |
| **Total weighted** | | | **~100** |

Band: **amazing**. No Rust changes — entirely a bash helper + docs.

## Goal

Make `ssh root@172.16.0.2` work against a booted microVM.

The quickstart bionic rootfs ships with sshd installed and auto-started (we saw `[ OK ] Started OpenBSD Secure Shell server` in m1's boot output), but no `authorized_keys` and root password login disabled — so SSH connections currently get `Permission denied`. This task bakes our pubkey into the rootfs so pubkey auth works.

After this lands, the m4 milestone (curl Anthropic from inside) is trivial: `ssh root@172.16.0.2 "curl ..."`.

## Functional

**New script: `scripts/bake-rootfs-ssh.sh`** (~100 LOC bash).

Invocation:

```sh
bash scripts/bake-rootfs-ssh.sh [<rootfs-path>]
# Default rootfs-path: ~/rooms/images/rootfs.ext4
```

Behavior:

1. **Validate prereqs.** Check for `sudo`, `mount`, `chroot`, `losetup`. Exit non-zero with actionable message if any missing.
2. **Argument parsing.** Accept optional positional `<rootfs-path>`. Default to `$HOME/rooms/images/rootfs.ext4`. Error out clearly if the file doesn't exist or isn't writable.
3. **Generate / reuse host-side SSH key.** Check for `~/.ssh/id_rooms` + `~/.ssh/id_rooms.pub`. If absent, generate a new ed25519 keypair with empty passphrase: `ssh-keygen -t ed25519 -N "" -f ~/.ssh/id_rooms -C "rooms-microvm"`. If present, reuse (idempotency).
4. **Loop-mount the rootfs.**
   - Make a temp mount dir: `MNT=$(mktemp -d)`
   - Allocate a free loop device: `LOOP=$(sudo losetup -f --show "$ROOTFS")`
   - Mount: `sudo mount "$LOOP" "$MNT"`
   - Register a trap that runs `cleanup_mount` on EXIT / INT / TERM to guarantee unmount + loop-detach even on failure.
5. **Bake the key inside the chroot.**
   - `sudo mkdir -p "$MNT/root/.ssh"`
   - `sudo chmod 700 "$MNT/root/.ssh"`
   - `sudo touch "$MNT/root/.ssh/authorized_keys"; sudo chmod 600 "$MNT/root/.ssh/authorized_keys"`
   - Append the pubkey **only if not already present** (idempotency): `grep -qxF "$(cat ~/.ssh/id_rooms.pub)" "$MNT/root/.ssh/authorized_keys" || sudo tee -a "$MNT/root/.ssh/authorized_keys" < ~/.ssh/id_rooms.pub`
6. **Configure sshd.** Edit `$MNT/etc/ssh/sshd_config` in place (with backup `*.bak.rooms` for safety):
   - Ensure `PermitRootLogin yes` (replace any `PermitRootLogin <other>` line)
   - Ensure `PubkeyAuthentication yes`
   - Ensure `PasswordAuthentication no` (defense in depth — we don't want password auth even with the password set)
   - Use `sed -i.bak.rooms` to make the changes idempotent (re-runs leave the file in the same state).
7. **Clean up.** Trap fires: `sudo umount "$MNT"; sudo losetup -d "$LOOP"; rmdir "$MNT"`. Script exits 0 on success.

**Logging.** Use the same `log()` / `fatal()` helpers as `scripts/setup-tap.sh`. Print:
- Key path being used (so the operator can copy the `ssh -i` flag for verification)
- Each major step
- Final success line with the suggested next-step `ssh` command

**Idempotency.** Running the script twice in a row with the same key must:
- Not duplicate the key in `authorized_keys`
- Not duplicate `sshd_config` edits (no `PermitRootLogin yes\nPermitRootLogin yes`)
- Not fail with "key already exists"

## Tradeoffs

- **Modifying the quickstart rootfs (the "dirty path").** We're surgically editing a 50 MB ext4 file that was downloaded by `setup-rooms-host.sh`. Task #6 (`rootfs-builder`) will replace this with a debootstrap-built image that bakes the key at build time, retiring this script. Accepted because m3 needs to land **before** #6 to unblock m4.
- **Bash script, not Rust.** Mount/chroot/sed is bread-and-butter sysadmin work; Rust adds nothing here and would force shelling out to all the same commands anyway. The Rust side stays the boot-control plane; the rootfs preparation lives in `scripts/`.
- **`PermitRootLogin yes`.** Production would use a non-root user with sudo. POC accepts root login because the quickstart rootfs's only meaningful user is root, and creating a user via chroot is fiddly. Task #6 (`rootfs-builder`) creates a non-root `rooms` user from scratch.
- **Hardcoded key path `~/.ssh/id_rooms`.** A dedicated key (not the operator's general-purpose `~/.ssh/id_ed25519`) so the rooms key has its own lifecycle and isn't tied to GitHub or anything else. Could be overridable via `--key <path>` but YAGNI for POC.

## EDs (engineering decisions)

- **ED-1: Host SSH key lives at `~/.ssh/id_rooms`.** Dedicated key, ed25519, no passphrase. Script generates on first run.
- **ED-2: Bash script, not Rust.** Matches `setup-rooms-host.sh` and `setup-tap.sh` precedent.
- **ED-3: Idempotency via grep-then-append + sed-with-backup.** Re-runs are safe and produce identical rootfs state.
- **ED-4: Trap-based cleanup.** Unmount + loop-detach in `trap ... EXIT INT TERM` so stale mounts don't persist on failure.
- **ED-5: No new dossier task for the verifier.** Verification is in the spec's "Acceptance" section; the agent runs it before the PR is opened.
- **ED-6: `PermitRootLogin yes`.** Acknowledged tradeoff; replaced by non-root user in task #6.
- **ED-7: Defense-in-depth `PasswordAuthentication no`.** Even if the rootfs has a default root password, we explicitly disable password auth so only pubkey works.

## Validation

The agent implementing this MUST run all of these on the rooms-host VM and confirm they pass before opening the PR:

1. **First run.** Fresh state (no `~/.ssh/id_rooms` yet):
   ```sh
   bash scripts/bake-rootfs-ssh.sh
   ```
   Expected: key generated at `~/.ssh/id_rooms` + `.pub`, rootfs mounted/edited/unmounted, no leftover mount, exit 0.
2. **Boot smoke.** Re-boot the microVM with the edited rootfs:
   ```sh
   cargo run --quiet -- create --image ~/rooms/images/rootfs.ext4 --repo . --keep
   ```
   Expected: same clean systemd boot output as before m3, sshd starts cleanly.
3. **SSH round-trip.** While the VM is running (in a second shell):
   ```sh
   ssh -i ~/.ssh/id_rooms -o StrictHostKeyChecking=accept-new -o UserKnownHostsFile=/dev/null root@172.16.0.2 "uname -a; cat /etc/os-release | head -2"
   ```
   Expected output includes `Linux ubuntu-fc-uvm 4.14.174` and `NAME="Ubuntu" / VERSION="18.04.5 LTS (Bionic Beaver)"`. SSH exit code 0.
4. **Idempotency.** Shut down VM. Re-run the bake script:
   ```sh
   bash scripts/bake-rootfs-ssh.sh
   ```
   Expected: exit 0, no errors, no duplicate `authorized_keys` lines (`grep -c rooms-microvm ~/rooms/images/...` — verify by mounting separately if needed; or trust grep-then-append logic).
5. **Idempotency boot.** Re-boot the VM after the second bake; SSH again as in step 3. Should still work.
6. **Stale-mount safety.** Force a failure mid-script (e.g. comment out a step temporarily to make it `false`), confirm the trap runs and `mount | grep rooms` shows no leftover mounts.

If any of the 6 fail, do NOT open the PR; fix and re-validate.

## Risks

- **Mounting an in-use rootfs corrupts it.** If a microVM is currently running using the rootfs, mounting it read-write on the host risks corruption. Mitigation: script checks `lsof "$ROOTFS"` (or similar — `fuser`) before mounting and refuses if anything has the file open. If a stale firecracker is using it, the operator gets a clear error.
- **sed regex misfires on `sshd_config`.** If bionic's sshd_config has unusual comment formatting, the sed could miss or duplicate. Mitigation: the `.bak.rooms` backup lets the operator diff and recover. Use anchored, conservative sed patterns (match full lines with the directive at column 0).
- **Loop devices leak.** If the script crashes before the trap registers, a loop device could be left attached. Mitigation: register the trap BEFORE doing any losetup/mount.
- **SSH host key changes between boots.** The microVM regenerates host keys on each boot (or doesn't have any baked in?). The `UserKnownHostsFile=/dev/null` option in the verification step skips host key verification entirely — fine for POC, the operator just needs to be aware.

## Out-of-scope (deferred to future tasks)

- **Per-room dynamic SSH keys.** POC: one shared key for all rooms. Task #2 (`harden-firecracker-control`) can add per-room keys when needed.
- **Rust-driven SSH invocation from the `rooms` binary.** For m3, SSH is something the operator (or m4) does manually. A future `rooms exec <room_id> -- <cmd>` command will own this end-to-end.
- **Replacing the quickstart rootfs with a debootstrap build.** That's task #6 (`rootfs-builder`); it retires this script.
- **Non-root user inside the guest.** Task #6.
- **Disabling `PermitRootLogin yes`.** Task #6.

## Implementation plan

1. Write `scripts/bake-rootfs-ssh.sh` per the Functional section. Mirror the `log()`/`fatal()` helpers from `scripts/setup-tap.sh` for consistency.
2. Make the script executable (`chmod +x`).
3. Run the 6 validation steps on the rooms-host VM. Capture the SSH round-trip output in the PR description so reviewers can see proof.
4. Add a brief subsection to `README.md` → "Prereqs" that mentions running `bash scripts/bake-rootfs-ssh.sh` once during host setup (between `setup-rooms-host.sh` and `setup-tap.sh`). One paragraph, no deep details.
5. Optional: add a one-line mention to `scripts/README.md` (if it exists; create if not) listing all three bootstrap scripts.
6. Commit + push on branch `m3-ssh-access`.
7. Open PR. Request reviewers: Copilot, `@codex review`, `@claude review`. Iterate per the standard review cycle (~3 rounds before reaching out).

PR shape: one PR, ~100 weighted LOC. "amazing" band.

**Branch:** `m3-ssh-access` (already created via `git worktree add -b m3-ssh-access .claude/worktrees/m3-ssh-access`).

**Workdir for `ship.ship`:** `C:\Users\MichaelHabib\pers\rooms\.claude\worktrees\m3-ssh-access`.

**Spec path for `ship.ship`:** `docs/features/poc-m3-ssh-access/spec.md` (this file, relative to workdir).
