**Status**: draft (planning)
**Owner**: @michael (human:mh)
**Date**: 2026-06-28
**Related**: [docs/vision.md](../../vision.md) (v0.2 = this), [rooms-registry/spec.md](../rooms-registry/spec.md) (the substrate, #51), [docs/follow-ups.md](../../follow-ups.md)

# rooms v0.2 — the operational layer (decomposition)

## Where we are

The **room registry** (`rooms ls` + `rooms gc`, [#51](https://github.com/itsHabib/rooms/pull/51)) shipped: per-room `room.json` metadata, `/proc/<pid>/stat` liveness, and a fail-safe orphan reaper. That was deliberately the *first* v0.2 step — almost everything else in the operational layer needs a way to **see** what rooms exist and **clean up** what leaks.

v0.2 per the vision: **snapshots + fork, replay receipts, hard parallelism for `/work-driver` fan-out.** This doc decomposes that into shippable parts, sizes and orders them, sketches each design, and frames the open ship-provider decision. It is a *map*, not a per-feature TDD — each part gets its own kickoff (small) or `/tdd` (large) when picked.

## The parts

### 1. `rooms kill <id>` — complete the lifecycle CLI · **S**

**Problem.** You can now *see* rooms (`ls`) and reap *dead* ones (`gc`), but there's no clean off-switch for a *live* one. A `--keep` debug room or a runaway exec has to be killed by hand (`ls` → copy the pid → `sudo kill`). The lifecycle verb set is `run / ls / gc / **kill**`; kill is the missing one.

**Approach.** `rooms kill <id> [--json]`: resolve the room via the registry, confirm it's **alive** (Running or Kept), `SIGTERM`→grace→`SIGKILL` the firecracker pid (reuse `kill_child_gracefully`), then reap its dirs/mounts via the existing `reap_orphan` path (it's dead now). A `Dead`/`Unknown` room → clear "nothing live to kill; use `gc`" rather than a forced action.

**Key decisions.** Validate the id (reuse `is_valid_room_id`); kill only confirmed-alive (symmetry with gc's confirmed-dead); `--all` to kill every live room (stretch); does it free the network slot (ties into the pool, part 2).

**Deps.** Registry (done). **Reuses** `kill_child_gracefully` + `reap_orphan` — no new teardown path.
**Ship relevance.** A driver/operator can stop a stuck ship-on-rooms run by id.
**Sizing.** S (~150–250 LOC). A clean standalone `/goal` — good warm-up; lands independent of the pool.

### 2. Multi-room pool + scheduler — hard parallelism · **L (keystone)**

**Problem.** There is **one** shared `tap-fc0` at `172.16.0.1/24` and a hardcoded guest IP `172.16.0.2` (`main.rs` `NetworkConfig`). So in practice **one networked room at a time**. `/work-driver` fan-out (N parallel ship runs) cannot run N rooms concurrently. This is the v0.2 "hard parallelism" goal, and the registry was built as its bookkeeping substrate.

**Approach.** A per-room **network slot**:
- **Slot allocator** hands out `(tap_name, guest_ip, gateway_ip)` tuples; the registry's `room.json` records the slot per room so `ls`/`gc`/`kill` know it and free it. The registry is the source of truth for what's allocated.
- **Per-room tap** (`tap-fc0..N`) created at boot, `ip tuntap del`'d at reap — this wires the already-stubbed `RoomGuard::set_tap_owned` and closes the registry's deferred "gc doesn't free a TAP" follow-up.
- **IP layout** — a `/30` per room, or `.2/.3/.4…` flat in `172.16.0.0/24`, or a wider subnet. (Decision below.)
- **iptables** — rework `setup-tap.sh`'s per-tap FORWARD/NAT rules into a rooms-owned **`ROOMS_FWD` chain** matching the guest subnet, so N taps don't each append a growing rule set (the existing `setup-tap` follow-up).
- **Concurrent boot + cap** — boot up to `max_pool` rooms at once; allocate-on-create, free-on-teardown.

**Key decisions (the `/tdd` will settle these).** Subnet layout (`/30`-per-room vs flat `/24`, and the pool-size ceiling it implies); tap naming + lifecycle (created at boot vs pre-provisioned pool); allocator persistence (registry-backed scan vs a small lock file — race-safe slot claim under concurrent boots); the `ROOMS_FWD` chain shape; leak handling (a slot whose room orphaned → reclaimed by gc).

**Deps.** Registry (done). Touches `gc`/`kill` (free the slot + tap on teardown).
**Ship relevance.** **THE enabler** for parallel ship-on-rooms — `/work-driver` firing N streams on `backend: "rooms"`. Highest leverage for the dogfood.
**Sizing.** L. Decomposes into (a) slot allocator + registry slot field, (b) per-room tap lifecycle + `ROOMS_FWD` chain, (c) concurrent boot + the cap. **Own `/tdd`.**

### 3. Snapshots + fork — fast warm boot · **M/L**

**Problem.** Every room cold-boots (~2 s to sshd). For fan-out (boot N) and fast iteration, forking a *warm* snapshot is far cheaper — Firecracker `/snapshot/create` + `/snapshot/load` restore in well under a second. No snapshot scaffolding exists yet (clean slate).

**Approach.** Snapshot a booted **base** room (post-boot, sshd up, pre-repo) to disk; `rooms run --from-snapshot <base>` restores a fresh room from it into a new network slot with a CoW rootfs overlay. Fork = restore-then-diverge.

**Key decisions.** What the base captures (booted kernel + sshd, no repo/keys-yet); snapshot storage + versioning (invalidate when kernel/rootfs changes); **network re-attach** on restore (a snapshot freezes the NIC/IP — restoring into a *different* slot needs the tap re-pointed, which is the sharp part and couples to part 2); CoW interplay with the existing read-only-rootfs + overlay; staleness.

**Deps.** Best on top of the pool (per-room slots on restore); can prototype single-room first.
**Ship relevance.** Faster ship-on-rooms spin-up, especially in parallel. An *optimization*, not a blocker.
**Sizing.** M/L. **Own `/tdd`.**

### 4. Replay receipts — record + replay a run · **M**

**Problem.** No durable record of *what a room did* — command, deps, repo sha, artifacts, outcome — for audit, repro, or comparing two runs. The registry holds *live* state; a receipt is the post-mortem record (the vision's "replay" line).

**Approach.** On teardown, write a **receipt** (the `room.json` meta + `result.json` + the `changeset` from `rooms diff` + the inputs: command / deps / repo+base-sha) to a receipts store. `rooms replay <receipt>` re-runs the same inputs against the same image/deps and compares outcomes.

**Key decisions.** Receipt schema + store location (under the state base? a sibling `receipts/`?); exactly what's captured for reproducibility; replay determinism (pin image + deps); whether receipts link back to dossier.

**Deps.** Registry + artifacts + diff (all done).
**Ship relevance.** Audit/repro for ship-on-rooms runs ("why did this agent run pass/fail"). Later.
**Sizing.** M. Kickoff or small `/tdd`.

## Dependency graph + recommended sequence

```
registry (#51, done)
   ├── kill ............... S   standalone, builds on the reap path
   ├── pool ............... L   keystone — parallel ship-on-rooms        ◀ the big /tdd
   │      └── snapshots ... M/L optimizes the pool (warm fork into a slot)
   └── receipts ........... M   audit/replay; after the lifecycle is parallel
```

**Recommended:** **kill → pool → snapshots → receipts.**
- `kill` is a clean immediate `/goal` (small, completes the verb set, exercises the registry further).
- `pool` is the keystone — the `/tdd` to run next; it's what makes rooms a real *parallel* ship backend.
- `snapshots` optimizes the pool; sequence after (or prototype single-room alongside).
- `receipts` is the audit capstone once the parallel lifecycle is solid.

## The ship-provider decision (for you to call)

`ship` runs an agent via a runtime/backend. Three things we could test, and how each relates to the parts above:

| Provider | What it is | Status today | Needs |
| --- | --- | --- | --- |
| **local cursor** | agent on the host / a worktree | works | nothing — the baseline |
| **cloud cursor** | agent in cursor's cloud | works | nothing — the cloud-driver baseline |
| **`backend: "rooms"`** | agent **inside a rooms microVM** on rooms-host (`RoomCursorRunner`) | single run works (cursor-sdk-runner); registry now cleans up after | **N=1: nothing.** Parallel: the **pool** (part 2). |

**Framing.**
- To **validate the rooms backend end-to-end now**, fire `backend: "rooms"` at **N=1** — one ship-on-rooms run. No new code; the registry makes post-run cleanup safe. This de-risks the dogfood path *before* investing in the pool, and exercises `ls`/`gc` against real ship-driven rooms.
- To run **parallel** ship-on-rooms (the `/work-driver`-on-rooms vision), the **pool** is the prerequisite — that's the case the keystone part unblocks.
- **local / cloud cursor** are the non-rooms baselines to measure the rooms backend against (speed, reliability, cost).

**Recommendation.** Test `backend: "rooms"` at **N=1** first (cheap, validates the dogfood + the new registry), and land `rooms kill` as a quick parallel win, then commit to the **pool `/tdd`** to scale ship-on-rooms to fan-out. Pick the provider once we see the N=1 rooms run behave.

## Out of scope (still, per the vision's non-goals)

Web preview / port-forwarding, persistent dev-workspace UX, multi-host control, containers. Snapshots/fork/parallel are the v0.2 lines; the rest stays where the vision drew it.
