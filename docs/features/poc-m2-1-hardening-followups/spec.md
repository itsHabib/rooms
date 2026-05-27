**Status**: draft (v2, descoped read-only-rootfs after reviewer caught sshd-DOA risk)
**Owner**: @michael (human:mh)
**Date**: 2026-05-24
**Related**: dossier task `poc-m2-1-hardening-followups` (id: `tsk_01KSDN9BAG8RJS32PHVP2PQ6SM`), POC phase `00-poc-implementation`, retroactive Plan + general-purpose reviewer reports (session 2026-05-24), descoped sibling `readonly-rootfs-with-overlay` (id: `tsk_01KSDNM7D0RQH6J823RFZ1S9EJ`)

# POC m2.1: hot-fixes from review (socket cluster + CLI right-sizing)

Two fixes in one PR, both touching `src/firecracker.rs` + `src/main.rs`.

## What changed from v1

v1 included a third item (`is_read_only: true` for the rootfs). An adversarial Plan-agent review caught that this would break sshd inside the guest (host keys, `/var/log/auth.log`, `/var/run/sshd.pid` all require writes to `/`; bionic's quickstart rootfs has no overlay init logic). That would land m3 DOA. The proper fix is a kernel-cmdline overlay setup that needs to live with `rootfs-builder` (#6) — descoped to new task `readonly-rootfs-with-overlay`.

This PR now ships **socket cluster fix** + **CLI right-sizing**.

## Scope

| Bucket | Files | Est. LOC | Weighted |
|---|---|---|---|
| Production source (1×) | `src/firecracker.rs`, `src/main.rs` | ~110 | 110 |
| Tests (0.5×) | unit test for `RoomDirGuard`, unit test for `wait_for_socket` race | ~50 | 25 |
| Docs (0×) | `README.md` CLI examples update | ~5 | 0 |
| **Total weighted** | | | **~135** |

Band: **amazing**.

## Functional

### 1. Socket cluster (kills 3 reviewer-flagged criticals)

**Current state** (`src/firecracker.rs:69-74`):

```rust
let socket = PathBuf::from(format!("/tmp/fc-{}.sock", std::process::id()));
if socket.exists() {
    let _ = tokio::fs::remove_file(&socket).await;
}
```

Three independent reviewer-flagged criticals:
- `/tmp` is shared; an attacker process running as the same user can pre-create a symlink at the predicted path → our blind `remove_file()` deletes the target → our `curl --unix-socket` then talks to an attacker socket.
- PID-reuse: a second `rooms` invocation that gets a recycled PID stomps the first's live socket.
- On any error-path return from `boot()`, the socket file is never cleaned (only `BootedVm::shutdown` removes it).

**New state — per-room state directory:**

- Add a `RoomId` newtype wrapping `ulid::Ulid` in `src/firecracker.rs` (above `boot()`).
- `boot()` generates `RoomId::new()` immediately.
- Compute per-room dir as `std::env::var("HOME")? + "/.local/state/rooms/" + room_id.to_string().to_lowercase()`. **Use `std::env::var("HOME")?`, NOT the `dirs` crate** — keeps the dep surface small for this PR.
- `tokio::fs::create_dir_all(&per_room_dir).await?` to create it.
- **Immediately after create, set mode 0700:** `tokio::fs::set_permissions(&per_room_dir, std::fs::Permissions::from_mode(0o700)).await?`. Required: `use std::os::unix::fs::PermissionsExt;`. `create_dir_all` inherits umask (typically 0755); explicit chmod is mandatory.
- API socket path: `per_room_dir.join("api.sock")`.

**`RoomDirGuard` for cleanup-on-Drop:**

```rust
struct RoomDirGuard {
    path: PathBuf,
    dismiss: bool,
}

impl Drop for RoomDirGuard {
    fn drop(&mut self) {
        if !self.dismiss {
            // Synchronous remove — Drop is sync, async drop isn't stable.
            // We're cleaning up a failed boot; blocking briefly is fine.
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}
```

- **The guard is stack-local in `boot()`. It is NOT stored in `BootedVm`.** Constructed early with `dismiss: false`; right before returning `Ok(BootedVm { ... })`, set `guard.dismiss = true` (so successful boot doesn't trigger cleanup).
- Uses `std::fs::remove_dir_all` (blocking) intentionally — `Drop` is sync; async-drop isn't stable. Add a one-line justification comment.
- `BootedVm::shutdown` does its own explicit cleanup of the per-room dir (separately, with proper async).

**`BootedVm` shape:**

```rust
pub struct BootedVm {
    child: Child,           // FIELD ORDER MATTERS — child first
    socket: PathBuf,        // unchanged (still inside per_room_dir)
    room_dir: PathBuf,      // NEW — added alongside, not a rename of `socket`
}
```

Field declaration order matters: Rust drops fields in declaration order. `child` must be first so `kill_on_drop` fires (and firecracker is reaped) BEFORE `room_dir` gets used by any cleanup. (Today no field has a Drop side-effect on the room_dir, but explicit ordering is cheap insurance for future changes.)

**`BootedVm::shutdown` updates:**
- Existing logic: kill child + remove socket file. Add: `tokio::fs::remove_dir_all(&self.room_dir).await.ok();` at the end. Idempotent — if already gone, no-op.

**`wait_for_socket` connect-vs-exists fix:**

Current (`src/firecracker.rs:163-173`):

```rust
async fn wait_for_socket(socket: &Path, timeout: Duration) -> Result<()> {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if socket.exists() {
            return Ok(());
        }
        sleep(Duration::from_millis(50)).await;
    }
    anyhow::bail!(...)
}
```

**New:**

```rust
async fn wait_for_socket(socket: &Path, timeout: Duration) -> Result<()> {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if tokio::net::UnixStream::connect(socket).await.is_ok() {
            // Connection succeeded → Firecracker has listen()ed.
            // Drop the stream immediately; next API call opens a fresh one.
            return Ok(());
        }
        sleep(Duration::from_millis(50)).await;
    }
    anyhow::bail!(
        "firecracker api socket at {} did not accept connections within {:?}",
        socket.display(),
        timeout,
    )
}
```

`tokio::net::UnixStream::connect` is already enabled via `tokio = { features = ["net", ...] }` in `Cargo.toml:15` (verified). No dep changes needed.

**Caveat:** `connect()` proves Firecracker has `listen()`-ed, but there's still a microsecond window before the API server's HTTP handler is mounted. In practice this hasn't shown up in tests; the existing `--fail-with-body` `--silent` curl invocations will surface a real error if it does. Note for future hardening (`harden-firecracker-control` task): consider a `GET /` health probe after connect as belt-and-suspenders.

### 2. CLI right-sizing

**Current** (`src/main.rs:23-79`): 6 subcommands; 5 are `anyhow::bail!("not yet implemented (POC in flight)")` stubs; one (`Create`) does the real work.

**New:**

```rust
#[derive(Subcommand, Debug)]
enum Command {
    /// Run a task end-to-end in a fresh microVM.
    Run {
        /// Path to the rootfs image (ext4).
        #[arg(long)]
        image: PathBuf,
        /// Keep the room alive until Ctrl-C instead of the default 3s auto-shutdown.
        #[arg(long)]
        keep: bool,
        // Intentionally absent in this PR: --command, --task, --repo. Land in m3/m4
        // and repo-transport milestones. DO NOT add them speculatively here.
    },
    /// Check the host environment (KVM, Firecracker, image, etc.).
    Doctor,
}
```

**Changes:**
- Remove `Command::Create`, `Command::Exec`, `Command::Collect`, `Command::Destroy`. Move the body of the old `Command::Create` into `Command::Run`, dropping the `--repo` arg (currently required but unused; it'll return when repo transport lands in a later milestone).
- Keep `Command::Doctor` as a `bail!` stub — that's `harden-firecracker-control` task #2's territory.
- Remove or downgrade the `#[allow(clippy::unused_async)]` on `dispatch` if clippy stops needing it after the collapse. Read the lint output after the change; only allow what's still needed.
- Keep the `cli_definition_is_valid` test in `src/main.rs::tests` (it's still relevant; `Cli::command().debug_assert()` validates the new 2-verb shape).

## Tradeoffs

- **`std::env::var("HOME")?` instead of the `dirs` crate.** Linux-only POC; `$HOME` is reliable. `dirs::state_dir()` would handle `XDG_STATE_HOME` correctly, but that's a new dep + larger build surface for one path lookup. Add `dirs` later if multi-platform or XDG correctness matters.
- **`RoomDirGuard` is stack-local, not stored in `BootedVm`.** Lifecycle is clear: "the dir exists from boot start; if boot fails, clean it; if boot succeeds, the BootedVm owns the dir going forward via the `room_dir` field." Storing the guard in BootedVm would conflate "boot-time cleanup" with "shutdown-time cleanup" — they're different concerns.
- **`std::fs::remove_dir_all` (blocking) in `Drop`.** `Drop` is sync; tokio async drop isn't stable. Blocking briefly during error cleanup is acceptable. Suppress clippy with one-line justification comment.
- **No overlay / RO rootfs work in this PR.** Per v2 descope above. Sibling task `readonly-rootfs-with-overlay` covers it.

## EDs (engineering decisions)

- **ED-1: Per-room dir under `~/.local/state/rooms/<room_id>/`.** XDG state convention. Mode 0700.
- **ED-2: `RoomId` is a ULID, lowercased in path components.** 26-char Crockford base32; `to_string()` is uppercase, `.to_lowercase()` for visual cleanliness in `ls`.
- **ED-3: `RoomDirGuard` is stack-local in `boot()`, dismissed on success.** NOT stored in `BootedVm`.
- **ED-4: `BootedVm` fields ordered `child, socket, room_dir`** so Rust's declaration-order drop runs `kill_on_drop` before any room_dir reference would be needed. Add comment.
- **ED-5: `tokio::net::UnixStream::connect` replaces `Path::exists()` in `wait_for_socket`.**
- **ED-6: Failed-run dirs are cleaned, not preserved for postmortem.** Symmetric: success cleans via `shutdown`, failure cleans via `Drop`. (Earlier draft proposed leaving failed runs for forensics; reverted because it contradicts the guard logic and leads to disk creep.) If postmortem becomes a real need, add a `--keep-on-failure` flag later.
- **ED-7: `--repo` is dropped from the CLI for now.** Reintroduce when repo transport actually exists (m4 / cursor-sdk-runner / ship-rooms-backend timeframe).
- **ED-8: `Command::Doctor` stays a `bail!` stub.** Real implementation in `harden-firecracker-control` task #2.

## Validation

Implementing agent MUST run these on the rooms-host VM and confirm pass before opening the PR. Capture the parallel-boot output (step 3) in the PR description.

1. **`make check` passes** (fmt + clippy strict + test).
2. **`rooms --help` shows only `run` + `doctor`** (no `create`, `exec`, `collect`, `destroy`, no `--repo`).
3. **Parallel boot isolation test:** in two shells on the rooms-host, each runs `cargo run --quiet -- run --image ~/rooms/images/rootfs.ext4 --keep`. The first should boot successfully. The second will fail at `PUT /network-interfaces/eth0` (the shared TAP from m2 is already in use) — **that failure is expected and acceptable for this milestone.** What MUST be verified: each invocation gets its own `~/.local/state/rooms/<room_id>/` dir; sockets are at different paths; the first invocation's socket is NOT deleted by the second's "clean stale" logic (which has been removed entirely). Verify by `ls ~/.local/state/rooms/` while the first is booted.
4. **RoomDirGuard unit test:** in `firecracker.rs::tests`, construct a `RoomDirGuard` pointed at a `tempfile::tempdir()` path; create some files inside; drop the guard with `dismiss=false`; assert the dir is gone. Repeat with `dismiss=true`; assert the dir still exists. (This replaces the v1 "comment out a step to test cleanup" hand-wave that cursor couldn't faithfully execute.)
5. **`wait_for_socket` regression test:** unit test in `firecracker.rs::tests` (gate behind `#[cfg(test)]`, no `e2e` feature needed): create a `tempfile::tempdir`, `tokio::fs::File::create(dir.path().join("fake.sock"))` (creates the file but nothing is listening), `wait_for_socket(path, Duration::from_millis(300))` — assert it times out. Then `tokio::net::UnixListener::bind(path)` (start listening), assert a fresh `wait_for_socket(path, Duration::from_millis(300))` returns `Ok(())` within the window.
6. **Original m2 ping still works:** `cargo run -- run --image ~/rooms/images/rootfs.ext4 --keep` in one shell; `ping -c 3 172.16.0.2` from another shell → 3/3 replies. Networking unchanged.
7. **Permissions check:** `stat -c '%a' ~/.local/state/rooms/<some-room-id>` after a boot → `700`.

If any step fails, do NOT open the PR; fix and re-validate.

## Risks

- **`std::fs::remove_dir_all` in `Drop` blocking the tokio runtime.** Brief, only during cleanup, acceptable. If it ever shows up as a real perf issue (won't), move to a `tokio::task::spawn_blocking` in a non-Drop cleanup path.
- **`RoomDirGuard` doesn't fire on `std::process::abort` or `SIGKILL`.** Known. Failure modes that bypass Rust unwinding leave the dir; the next run sees the stale dir but doesn't collide (ULID-named). `harden-firecracker-control` task #2's "doctor sweep" handles long-term cleanup.
- **`wait_for_socket` `connect()` might still race the HTTP handler mount inside Firecracker.** Documented in Functional §1 caveat. Belt-and-suspenders fix (a `GET /` probe) goes into `harden-firecracker-control`.
- **CLI rename breaks any scripts that called `rooms create`.** No such scripts exist; rooms is a week old. N/A.

## Out-of-scope

- Structured `FirecrackerError` enum — `harden-firecracker-control` task #2.
- Read-only rootfs / overlay — new task `readonly-rootfs-with-overlay`.
- Per-room dynamic TAP / IP allocation — `harden-tap-rules`.
- `--keep` PID persistence for `rooms destroy <room_id>` recovery — `harden-firecracker-control`.
- Cleanup-after-reboot of leftover dirs from previous-boot crashes — `harden-firecracker-control` doctor sweep.
- Jailer integration — `firecracker-under-jailer`.
- Re-introducing `create / exec / collect / destroy` — task #5 when ship's `RoomCursorRunner` needs them.

## Implementation plan

1. In `src/firecracker.rs`, add the `RoomId` newtype (or just `type RoomId = ulid::Ulid;` — your call, lighter is fine).
2. Add `RoomDirGuard` struct + Drop impl above `boot()`.
3. Refactor `boot()`:
   - Generate `room_id = Ulid::new()`.
   - Compute `per_room_dir = PathBuf::from(env::var("HOME")?).join(".local/state/rooms").join(room_id.to_string().to_lowercase())`.
   - `tokio::fs::create_dir_all(&per_room_dir).await?`.
   - `tokio::fs::set_permissions(&per_room_dir, std::fs::Permissions::from_mode(0o700)).await?` (with `use std::os::unix::fs::PermissionsExt;`).
   - Construct `let mut guard = RoomDirGuard { path: per_room_dir.clone(), dismiss: false };` immediately after.
   - `let socket = per_room_dir.join("api.sock");`.
   - Existing boot sequence proceeds. Each `?` between here and the final return triggers the guard's Drop → dir cleaned.
   - **Important:** drop the existing "clean stale socket" block (`if socket.exists() { remove_file ... }`) — no longer needed with per-room dirs.
   - At successful return point: `guard.dismiss = true;` then return `Ok(BootedVm { child, socket, room_dir: per_room_dir })`.
4. Update `BootedVm` struct: add `room_dir: PathBuf` as the third field (after `child`, `socket`).
5. Update `BootedVm::shutdown` to call `tokio::fs::remove_dir_all(&self.room_dir).await.ok();` at the end (idempotent).
6. Update `wait_for_socket` to use `tokio::net::UnixStream::connect`.
7. In `src/main.rs`, replace the 6-variant `Command` enum with the 2-variant version. Move the existing `Command::Create` body into `Command::Run`. Drop the `--repo` field.
8. Update or delete the `#[allow(clippy::unused_async)]` attribute on `dispatch` if no longer needed; check with `cargo clippy --all-targets --all-features -- -D warnings`.
9. Update `README.md` CLI examples (`rooms create` → `rooms run`).
10. Add the two unit tests (RoomDirGuard, wait_for_socket).
11. Run validation steps 1-7. Capture step 3 output for PR description.
12. Commit on `m2-1-hardening`, push, open PR with Copilot + `@codex review` + `@claude review`.

PR shape: one PR, ~135 weighted LOC. "amazing" band.

**Branch:** `m2-1-hardening` (already created).
**Workdir for `ship.ship`:** `<repo-root>/.claude/worktrees/m2-1-hardening/`.
**Spec path for `ship.ship`:** `docs/features/poc-m2-1-hardening-followups/spec.md` (this file, relative to workdir).
