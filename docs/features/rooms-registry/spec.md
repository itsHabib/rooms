**Status**: draft
**Owner**: @michael (human:mh)
**Date**: 2026-06-28
**Related**: `src/firecracker.rs` (the `RoomGuard` teardown this reuses), `src/config.rs` (state paths), `src/main.rs` (the verb wiring), [docs/vision.md](../../vision.md) (v0.2 operational layer), [docs/follow-ups.md](../../follow-ups.md)

# `rooms ls` + `rooms gc` — the room registry + orphan reaper

## Goal

rooms can run one agent room unattended without hurting you (jailer + read-only rootfs + `--max-wall` cap + the `rooms diff` lane gate). What it **can't** do is tell you what rooms exist or clean up the ones that leak. A crash, a `--keep`, or a `kill` of the launching process strands a firecracker process and a jailer chroot under `~/.local/state/rooms/` with no way to see it and no way to reap it (stale-room cruft hit twice during the safety arc).

This is the "`docker ps` + `docker system prune`" of rooms, and it's the substrate the rest of v0.2 (pool, scheduler, snapshots) hangs off: everything in that layer needs a room registry first.

Two verbs + the metadata that makes them possible:

1. **`rooms ls [--json]`** — what rooms exist, and is each one alive, kept, or a leaked corpse.
2. **`rooms gc [--dry-run] [<id>]`** — reap the corpses; never touch anything alive.

## Background (the on-disk layout, verified against `firecracker.rs`)

A room owns **two** directories under the state base (default `$HOME/.local/state/rooms`, overridable via `RoomsConfig::state_base`):

```
$STATE_BASE/
├── <id>/                              # per-room dir, mode 0700  (room_state_dir)
│   ├── firecracker.log
│   └── room.json                      # NEW — the metadata this feature adds
└── jailer/                            # the jailer chroot base (NOT a room)
    └── firecracker/<id>/              # jail instance dir
        └── root/
            ├── kernel                 # bind-mount  ← must be unmounted before rm
            ├── rootfs                 # bind-mount  ← must be unmounted before rm
            └── api.sock               # firecracker API socket
```

`<id>` is a lowercase ULID. The filesystem is the **source of truth** for "a room exists" — `room.json` only *enriches* it (pid, label, age). The scan tolerates a room with no `room.json` (a crash between dir-create and the meta write).

Normal teardown is `RoomGuard::cleanup_sync` (`firecracker.rs`): kill the fc child → (release tap — *skipped in v0*, shared `tap-fc0`) → rm the API socket → `teardown_jail_sync` (umount `kernel`+`rootfs`, rm the jail instance dir) → rm the per-room dir. **gc is exactly this teardown for a room whose live `RoomGuard` is gone**, so it reuses that path rather than forking a parallel one.

## Design

### 1. Per-room metadata — `room.json` (new module `src/room.rs`, leaf/domain layer)

Written **atomically** (temp + `rename` in the same dir; honors the repo's atomic-write convention) by `boot()` right after the jailer child pid is known — earliest possible, so a crash strands as little as possible.

```rust
pub struct RoomMeta {
    pub schema_version: u32,        // own file schema; forward-compat like result.json
    pub id: String,                // lowercase ULID (matches the dir name)
    pub label: Option<String>,     // command/task: "id", "cursor:<repo>", "(keep)", "(idle)"
    pub started_at: DateTime<Utc>,
    pub pid: Option<u32>,          // jailer→firecracker child pid; None ⇒ liveness Unknown
    pub keep: bool,                // started with --keep (a deliberately held room)
}
```

`room.rs` is plain data + its own I/O (atomic write, read) + the liveness probe. No upward import; depends only on std/serde/chrono. The `label` is the only field `main` (policy) supplies — `boot` takes it as `Option<&str>` and stays ignorant of CLI semantics.

### 2. Liveness — `/proc/<pid>/comm`, not `kill -0`

The reliable, **uid-independent, pid-reuse-resistant** liveness signal is `/proc/<pid>/comm`:

- `kill -0 <pid>` is wrong here: cross-uid it returns **EPERM** (process exists but you can't signal it) — which a naive `status.success()` reads as *dead*. `ls` runs as the unprivileged operator against a firecracker-uid process, so `kill -0` would misreport every live room.
- `/proc/<pid>/comm` is **world-readable** (mode 0644), needs no ptrace, and fits the 15-char `comm` cap (`firecracker` = 11). It distinguishes "process gone" from "pid reused by something else."

```
probe(pid) -> Liveness
  None                              => Unknown      // no pid recorded
  Some(p):
    /proc/p/comm missing            => Dead         // process gone
    comm ∈ {firecracker, jailer}    => Alive
    comm = anything else            => Dead         // pid reused by another process
    read error (not NotFound)       => Unknown      // fail-safe: can't tell ⇒ don't claim dead
  (non-unix)                        => Unknown
```

The recorded pid is the jailer child; the jailer `exec`s into firecracker (no `--daemonize`), so `comm` is `firecracker` in steady state (`jailer` accepted to cover the pre-exec window).

### 3. State classification (policy — `src/registry.rs`)

```
classify(meta, liveness):
  Alive + keep      => Kept            // a deliberately-held --keep room, still up
  Alive + !keep     => Running         // an in-flight `rooms run` exec
  Dead              => OrphanedDead    // fc gone, dirs/mounts leaked  ← the only reapable state
  Unknown           => Unknown         // no pid / unreadable /proc — never reaped
```

### 4. `rooms ls [--json]`

Scan `$STATE_BASE` for `<id>` subdirs (skip `jailer/` and any name that isn't a 26-char ULID), load `room.json` (tolerate absent), probe liveness, classify. Sort by `started_at` (newest last).

- **human**: one row per room — `ID  STATE  COMMAND  AGE  PID` (label truncated, age as `2h3m`/`45s`).
- **`--json`**: a `Serialize` struct `{ schema_version, rooms: [...] }` to **stdout**, logs on stderr, behind a justified `#[allow(clippy::print_stdout)]` — identical contract to `doctor --json` / `diff --json`.

### 5. `rooms gc [--dry-run] [<id>]`

Reap **only** `OrphanedDead` rooms. The reap reuses `RoomGuard` via a new `firecracker::reap_orphan(config, id)`: it builds a guard for the orphan (`child_pid = None` — the process is *confirmed dead*, so there is nothing to kill, which also sidesteps the pid-reuse-kill hazard; `tap` untouched) and calls `.cleanup()` → the exact `teardown_jail_sync` + socket-rm + dir-rm path the live drop uses.

- bare `rooms gc` → reap every `OrphanedDead` room; print a per-room summary (`reaped <id>` / `skipped <id> (running)`).
- `rooms gc <id>` → reap just that one (still only if `OrphanedDead`; otherwise a clear skip line, not a forced delete).
- `rooms gc --dry-run` → print what *would* be reaped; **touch nothing** (never constructs a guard, never unmounts, never rms).

Destructive-by-default is safe **by construction**: the reap predicate is `state == OrphanedDead`, so gc can never reap a live, kept, or unknown-liveness room. Safety comes from the predicate, not from a `--force` flag (matches the e2e contract: bare `gc` reaps, `--dry-run` previews).

### Cardinal safety invariant (adversarial-gated)

> gc must **never** (a) reap a live or `--keep` room, (b) delete anything outside `$STATE_BASE`, or (c) leave a mount or process behind.

Enforcement, defense-in-depth:

- **(a)** Reap predicate is `OrphanedDead` only. Liveness is **fail-safe — indeterminate ≠ dead** (mirrors `rooms diff`'s "couldn't verify ≠ clean" → exit 2): a missing pid or an unreadable `/proc` is `Unknown`, never reaped. A `--keep` room shows `Kept` while alive and is never reaped; once its fc is confirmed dead the keep flag no longer protects a corpse (nothing left to inspect) and it becomes reapable — that *is* the leak gc exists to clean.
- **(b)** Every reap target is derived from a **validated** id (`^[0-9a-z]{26}$`; the `<id>` arg and every scanned dir name pass through the same validator, so `..`, `jailer`, absolute paths, and separators are rejected) and re-checked: `room_dir.parent() == $STATE_BASE` and `jail_instance_dir.parent() == $STATE_BASE/jailer/firecracker` before any umount/rm.
- **(c)** Reap reuses `teardown_jail_sync` — unmount the binds, then `remove_dir_all`; it reports success only when the jail instance dir is actually gone (an active mount fails the removal and leaves it). When a bind is stuck (EBUSY at shutdown), the **live** teardown (`cleanup_sync`) now *preserves the per-room dir* instead of deleting gc's only handle — so the room re-classifies `orphaned-dead` and a later `gc` retries the unmount. `reap_orphan` errors (the room stays listed) if a dir survives. No silent, un-reapable mount leak. *(Hardening from the adversarial pass — the one real finding.)*

### Errors (`src/error.rs`)

A small `RegistryError` (thiserror) — `InvalidRoomId { id }`, `PathEscape { path }`, `Io(#[from])` — wired into `RoomsError::Registry`. Matches the structured-error direction (`FirecrackerError`); errors-not-capitalized.

### Layering

- `room` — new **leaf** (domain): `RoomMeta` + atomic I/O + `Liveness::probe`. Depends on nothing in-crate.
- `config` — gains `state_base` + path resolvers (`room_dir(id)`, `jail_instance_dir(id)`, `jail_root_dir(id)`, `jail_socket(id)`); centralizes path policy that was scattered as private fns in `firecracker.rs`. Leaf — everyone depends on it.
- `firecracker` — `boot` writes meta via `room`; `reap_orphan` reuses `RoomGuard`. Depends on `config`, `room`.
- `registry` — scan + classify + ls/gc policy (beside `runner`, composes `firecracker`). Depends on `config`, `room`, `firecracker`.
- `main` — `Ls`/`Gc` verbs → `registry`; renders human + json.

No cycle (`room`/`config` are leaves; `firecracker → {config, room}`; `registry → {config, room, firecracker}`). Dependencies flow down only.

## Acceptance

- `room.json` is written atomically at boot (temp + rename); round-trips through serde; `ls` degrades gracefully when it's absent.
- Liveness: `firecracker`/`jailer` comm ⇒ Alive; missing `/proc/<pid>` ⇒ Dead; foreign comm ⇒ Dead; no pid / unreadable ⇒ Unknown.
- `rooms ls` lists every room with correct state/age/pid; `--json` emits the schema'd struct on stdout (logs on stderr).
- `rooms gc` reaps only `OrphanedDead`; **never** selects Running/Kept/Unknown. `--dry-run` deletes nothing. An invalid/`..`/non-ULID id is rejected before any path is touched.
- `make check` green (fmt + clippy `-D warnings` + tests, Windows + Linux).
- **Adversarial Workflow pass** scoped to the whole invariant (skeptics told to break it, not just review a function) before merge.
- **Host e2e (rooms-host, gates merge):** leak a live `--keep` room → `ls` shows it alive → `kill` its fc pid → `ls` shows `orphaned-dead` → `gc` reaps it (dir gone, no leftover mounts in `/proc/mounts`, no fc process) → `ls` empty. Plus: `--dry-run` previews without deleting, and a negative test that a **live** room is never reaped.

## Test plan

- **Unit (hermetic, `state_base` → tempdir):** meta round-trip + atomic-write leaves no temp; `probe` over a fabricated `/proc`-style input (alive/dead/unknown — pure on the pid→Liveness mapping); `classify` truth table; gc reap-selection over a fixture of mixed-state rooms (asserts Running/Kept/Unknown are never selected); the id validator (rejects `..`, `jailer`, `<26`, separators); `--dry-run` leaves the fixture intact; the age/label formatters.
- **E2e (host-only, the gate):** the leak→kill→ls→gc→empty cycle above on a real jailer chroot.

## Out of scope (follow-ups)

- **Multi-room pool** (per-room tap + IP allocation + scheduler) — the L-sized epic, its own `/tdd`. v0 has one shared `tap-fc0`, so gc never frees a tap (noted, not built).
- **`rooms kill <id>`** (terminate a *live* room) — an orphaned-but-*alive* fc (launcher gone, fc still up) shows as `running` and is not gc's job. Stretch / follow-on.
- **Snapshots, replay receipts** — separate v0.2 lines.
- **`gc --json`** — trivial parity add; ship `ls --json` first, add if a consumer needs it.
- **Reaping meta-less / pre-feature leaks.** A room with no `room.json` (created before this lands, or a crash before the write) classifies `Unknown` and isn't auto-reaped (fail-safe). A **socket-probe liveness fallback** (the kickoff sanctions "the API socket": a refused/absent `api.sock` ⇒ definitively dead) would let `gc` clean those too — the natural fast-follow. Its own liveness path → its own adversarial pass.
- **Jail-subtree sweep** — defense-in-depth: have the registry also enumerate `<chroot_base>/firecracker/<id>` for jail dirs whose per-room dir is gone, as a second orphan source (the hardening above prevents *new* such leaks; this would recover historical ones).
- **Lazy-unmount self-heal** — `umount -l` as a last resort in the reap path so a transient EBUSY clears without waiting for a retry.
