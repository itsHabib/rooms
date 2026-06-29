**Status**: draft
**Owner**: @michael (human:mh)
**Date**: 2026-06-28
**Related**: [rooms-registry/spec.md](../rooms-registry/spec.md) (the registry this builds on — #51), [02-operational-layer/plan.md](../02-operational-layer/plan.md) (part 1, the v0.2 map), `src/firecracker.rs` (`reap_orphan`, `kill_child_gracefully`), `src/room.rs` (`probe` — reused at signal time), `src/registry.rs` (`is_valid_room_id`, `reap_paths`), [docs/follow-ups.md](../../follow-ups.md)

# `rooms kill <id>` — the off-switch for a live room

## Goal

The registry (#51) lets you **see** rooms (`rooms ls`) and reap the **dead** ones (`rooms gc`). The missing verb is a clean off-switch for a *live* one. Today, stopping a `--keep` debug room, a runaway exec, or a stuck ship-on-rooms run means `rooms ls` → copy the pid → `sudo kill` by hand, then a separate `gc` to clean the corpse. `rooms kill <id>` is the one verb that does both: signal the live firecracker, then reap.

The lifecycle verb set is `run / ls / gc / **kill**`; this completes it. It's a small, standalone step on the registry's reap path — a warm-up before the multi-room pool epic, and it exercises the registry against real teardown one more time.

## Background — kill *is* "signal the live fc, then reap"

`gc` already encodes the teardown: for a confirmed-**dead** orphan it builds a `RoomGuard` with `child_pid = None` (nothing to signal — which is *why* gc is safe) and runs `firecracker::reap_orphan` (umount the kernel/rootfs binds, rm the socket, rm the jail + per-room dirs, with an honest post-check). `kill` is that same teardown, preceded by the one thing gc deliberately never does: **send a signal to a live pid.**

So the only genuinely new logic is *"confirm alive → signal until dead → reap."* Everything downstream of "dead" reuses the gc path verbatim. No second teardown.

## Design

### CLI surface

```
rooms kill <id> [--json]
```

`<id>` is **required** (no bare `rooms kill` — killing every live room by default would be dangerous; `--all` is a deliberate future flag, below). `--json` emits the schema'd report on **stdout**, logs on stderr — the same contract as `ls --json` / `doctor --json` / `diff --json`.

### State → action (policy, `src/registry.rs`)

`kill` resolves the room through the registry (same classify the registry already does), then acts on its state:

| Classified state | Action | Disposition | Exit |
| --- | --- | --- | --- |
| `Running` / `Kept` (alive) | terminate the fc (identity-guarded), then `reap_orphan` | `killed` | 0 |
| `OrphanedDead` | **don't signal, don't reap** — "nothing live to kill; run `rooms gc`" | `already-dead` | 0 |
| `Unknown` (no pid / unreadable `/proc`) | **refuse** — indeterminate ≠ alive; never signal a pid we can't identify | `refused` | 2 |
| (valid id, no such room dir) | "no room with id `<id>` (already gone?)" | — (empty) | 0 |
| (invalid id) | reject before any fs/signal work (`InvalidRoomId`) | — (error) | 2 |

**Why kill doesn't reap an already-dead room.** `kill` reaps a room it *transitions* alive→dead (it owns that teardown — a `--keep` room you kill must end up gone, not a corpse). A room that was *already* dead when you asked is `gc`'s job; pointing there keeps each verb's responsibility sharp (kill = remove a live room; gc = reap dead leaks) and keeps kill's blast radius to rooms it actually killed.

### The identity-guarded terminate — the cardinal invariant

Unlike gc, `kill` **signals a recorded pid**, which opens the pid-reuse race: between the registry classifying the room *alive* and `kill` firing a signal, the room's firecracker can exit and the kernel can reuse its pid for an unrelated process. A signal to that reused pid would hit an innocent victim.

`kill_child_gracefully` (the Drop-path teardown) is **not** sufficient on its own here: its grace poll uses `kill -0` (existence), and it escalates to `SIGKILL` on whatever holds the pid after the grace — exactly the reused-pid hazard. The Drop path can afford that (it owns the `Child`, so the pid stays at least a zombie until reaped); `kill` cannot (the room may have been launched by a since-dead process, so init can reap and recycle the pid).

So `kill` uses an **identity-guarded** terminate (new `firecracker::terminate_by_identity`), built on the same `/proc/<pid>/stat` identity check `room::probe` already encodes:

```
terminate_by_identity(pid, grace) -> KillSignalOutcome
  probe(pid):                                  # identity check BEFORE SIGTERM
    Dead    => AlreadyExited                   # gone or pid reused → no signal
    Unknown => Indeterminate                   # can't tell → no signal
    Alive   => send SIGTERM
  loop until grace deadline:
    probe(pid) == Dead => Signaled             # confirmed gone; stop (don't escalate)
    else               => keep polling         # Alive or transient Unknown → wait
  probe(pid):                                  # identity check BEFORE SIGKILL
    Alive   => send SIGKILL; probe again: Dead => Signaled, else => Survived
    Dead    => Signaled
    Unknown => Indeterminate                   # never SIGKILL a pid we can't identify
```

The invariant: **re-probe `/proc/<pid>/stat` identity immediately before every signal; signal only while the pid is still the room's firecracker/jailer.** Liveness is identity-based (`comm ∈ {firecracker, jailer}`, zombie = dead), never `kill -0` existence — so a reused pid reads `Dead` and we stop rather than escalate. Success (`Signaled`/`AlreadyExited`) requires a **definitive `Dead`**; `Unknown` is never read as success (fail-safe, mirroring `probe`'s own contract). The residual TOCTOU floor — a pid reused by *another firecracker/jailer* in the sub-millisecond between the final probe and the syscall — is irreducible without `pidfd` and is far narrower than existence-based signaling; noted in the code.

`terminate_by_identity` lives beside `kill_child_gracefully` in `firecracker` (mechanism) and composes `room::probe`; the registry (policy) decides *which* rooms reach it. The Drop path keeps using `kill_child_gracefully` unchanged — different safety needs, so a little structural duplication of the TERM→grace→KILL skeleton is the right call over coupling the two.

### Outcome → exit-code mapping

`kill` returns a `KillReport { schema_version, outcomes: Vec<KillOutcome> }` (a one-element vec for single-id kill; the `Vec` shape mirrors `GcReport` and makes `--all` a non-breaking add). Each `KillOutcome { id, state, disposition, reason }`; `KillDisposition ∈ {killed, already-dead, refused, failed}` maps to the exit code:

- **0** — `killed` (signaled+reaped, or transitioned-dead-then-reaped) | `already-dead` (pointed to gc) | no such room.
- **1** — `failed`: the kill couldn't complete — fc `Survived` SIGKILL, or `reap_orphan` left a dir/mount behind (surfaced honestly, the way gc already reports a stuck reap).
- **2** — `refused`: `Unknown` liveness | invalid id (via the `Err` path).

### Layering

No new dependency direction. `firecracker::terminate_by_identity` composes `room::probe` (firecracker already depends on `room`); `registry::kill` composes `is_valid_room_id` + `reap_paths` (private, same module) + `firecracker::{terminate_by_identity, reap_orphan}`; `main` adds the `Kill { id, json }` verb → `registry::kill` → render. Identical shape to the `Gc` wiring.

## Cardinal safety invariant (adversarial-gated)

> `rooms kill` must **never** (a) signal a pid that is no longer this room's firecracker, (b) act on a non-room or a path outside the state base, (c) leave a mount or process behind after a successful kill, or (d) kill a room it classified `Unknown`.

Enforcement, defense-in-depth:

- **(a)** Every signal is preceded by a `room::probe` identity check; only a `comm ∈ {firecracker, jailer}` (non-zombie) pid is signaled. A reused pid reads `Dead` → no signal. (The unit suite proves a *live non-firecracker* pid is never signaled.)
- **(b)** The id passes `is_valid_room_id` before any work; the room must exist as a state-base subdir (`find_entry`); every reaped path comes from `reap_paths`, which re-checks each dir is a direct child of its expected parent (`ensure_child`) — the same backstop gc uses. kill adds no new path construction.
- **(c)** Teardown is `reap_orphan` unchanged — it errors (room stays listed) if the jail or room dir survives, so a stuck mount is reported, never silently leaked. kill reaps **only** after a definitive `Dead`; a `Survived` fc is reported `failed` and **not** reaped (never umount/rm a room whose fc is still up).
- **(d)** `Unknown` at classify *or* at the pre-signal re-probe → `refused`, exit 2, no signal. Indeterminate is never coerced to alive *or* dead.

## Acceptance

- `rooms kill <id>` terminates a live room's firecracker (SIGTERM→grace→SIGKILL) then reaps its dirs/mounts; `--json` emits the schema'd report on stdout.
- An `OrphanedDead` id is a safe no-op that points to `gc` (exit 0, not reaped here); an `Unknown` id is refused (exit 2, not signaled); a non-existent id is "already gone" (exit 0); an invalid id is rejected before any signal (exit 2).
- The identity guard never signals a live non-firecracker pid (pid-reuse safety).
- `make check` green (fmt + clippy `--all-targets --all-features -D warnings` + tests, Windows + Linux).
- **Adversarial Workflow pass** scoped to the whole invariant (skeptics told to break it) before merge.
- **Host e2e (rooms-host, gates merge):** boot a real `--keep` room → `ls` shows it `kept` → `rooms kill <id>` → fc process gone, room dir + jail dir gone, no leftover bind-mounts in `/proc/mounts` → `ls` no longer lists it. Plus: `kill` of a dead/unknown id is a safe no-op with the right exit code, and an invalid id is rejected before any signal.

## Test plan

- **Unit (hermetic, `state_base` → tempdir; platform-aware like the registry suite):**
  - `registry::kill` — invalid id rejected (`Err`); no-such-room → empty outcomes; an `Unknown` room (pid `None`) → `refused`, dirs survive, nothing signaled; an `OrphanedDead` room (a pid that cannot exist, Linux) → `already-dead`, **not** reaped (dirs survive), reason points to `gc`.
  - `firecracker::terminate_by_identity` — a pid that cannot exist → `AlreadyExited` (Linux); a **live non-firecracker** child (`sleep`) → `AlreadyExited` *and the child is still alive afterward* (proves the identity guard never signals a non-room pid).
  - `kill_exit_code` (or the disposition→code map) — pure, exhaustive over the four dispositions + empty.
  - CLI parse (`main`) — `rooms kill <id>` and `--json` parse; bare `rooms kill` (no id) is rejected.
  - The alive→signal→reap full path needs a real firecracker; it is the **host-e2e**, not a unit test (no live fc to fake in-process).

## Out of scope (follow-ups)

- **`rooms kill --all`** — kill every live room (batched outcomes, an `id`/`--all` conflict). A clean future add (the `Vec<KillOutcome>` report shape already anticipates it); deferred to keep this v0 sharp.
- **Freeing a network slot / per-room tap on kill** — pool-era. v0 shares `tap-fc0` (every room attaches it; `tap_owned` is always false), so kill, like gc, doesn't touch the tap. When the pool retires `tap-fc0`, kill + gc both free the slot.
- **Snapshots, replay receipts** — separate v0.2 lines (see the plan).
