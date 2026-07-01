# Multi-room pool — Technical Design Document

**Status:** draft / proposal — NOT a build commitment. The artifact we decide from.
**Owner:** @itsHabib (human:mh)
**Date:** 2026-07-01
**Related:** [`docs/vision.md`](../../vision.md) (v0.2 "hard parallelism"), [`02-operational-layer/plan.md`](../02-operational-layer/plan.md) (part 2 — this doc settles its open decisions), [`rooms-registry/spec.md`](../rooms-registry/spec.md) (the bookkeeping substrate), dossier project `rooms` phase `02-operational-layer`

> **Reviewers — focus areas:** §4 D1 (subnet layout — /30-per-slot vs flat /24), §4 D3 (race-safe slot claim without a daemon), §7.3 (the leak/reclaim flow — a slot must never outlive its room silently), and §8 (what happens when two `rooms run` invocations race the last slot).

## 1. Problem & hypothesis

rooms boots exactly **one networked room at a time**. The host side is a single shared TAP (`tap-fc0` at `172.16.0.1/24`) and a hardcoded guest IP (`172.16.0.2`) wired in `main.rs` when it builds `NetworkConfig`. A second concurrent room would collide on both the IP and the TAP. That makes rooms a single-lane backend: ship's `RoomCursorRunner` works at N=1 (ship PR #143), but `/work-driver` fan-out — N parallel ship-on-rooms streams — cannot exist.

**The bet:** a per-room *network slot* — `(tap_name, guest_ip, gateway_ip, netmask)` allocated at boot, recorded in the registry, freed at teardown — is the only missing piece between "works at N=1" and "hard parallelism." Everything else already scales: the registry gives per-room visibility (`ls`/`gc`, PR #51), `kill` completes the lifecycle (PR #53), jailer isolation is per-room already, and Firecracker doesn't care how many siblings it has.

**Non-goals (this TDD):**
- **No in-rooms scheduler or queue.** rooms is mechanism; scheduling is the consumer's policy. When the pool is full, `rooms run` fails fast with a distinct exit code — ship's driver already owns dispatch/backpressure and is the right home for "wait and retry."
- **No snapshots/fork** — sequenced after the pool (own TDD); this design only avoids painting it into a corner (a restored room re-attaches to a *new* slot; the slot abstraction must not assume the guest negotiated its IP at boot).
- **No multi-host pooling, no port-forwarding/ingress, no persistent rooms** — vision non-goals unchanged.

## 2. Functional & non-functional requirements

**FR**
1. N rooms boot and run concurrently, each with isolated egress-only networking, N up to a configured cap.
2. `rooms ls` shows each room's slot; `gc`/`kill`/normal teardown free the slot *and* delete the TAP.
3. A crashed/orphaned room's slot is reclaimable by `gc` — no manual iptables/ip surgery, ever.
4. Existing single-room flows keep working unchanged (`rooms run` with no siblings behaves exactly as today).
5. `rooms doctor` validates the pool substrate (chain installed, slot dir writable, no orphaned taps).

**NFR**

| Dimension | Target |
| --- | --- |
| Slot claim under race | 0 double-allocations across concurrent `rooms run` invocations (property: two racers, one slot → exactly one winner) |
| iptables growth | O(1) rules regardless of N (chain + wildcard match, not per-room appends) |
| Boot overhead | Slot claim + TAP create adds < 250 ms to the ~2 s boot path |
| Cap semantics | Pool-full is a *distinct, machine-readable* failure (exit code + `--json` error kind), not a generic boot error |
| Cleanup | After teardown/gc/kill, `ip link` shows no room TAP and the slot file is gone — verified in host-e2e |

## 3. Architecture overview

```
                    host (rooms-host, root via sudo)
┌──────────────────────────────────────────────────────────────┐
│  once per host (setup-tap.sh --host / doctor-verified):      │
│    ROOMS_FWD chain + FORWARD jump; supernet NAT;             │
│    per-iface forwarding sysctls                              │
│                                                              │
│  per room (inside `rooms run`, at boot):                     │
│    slots/ claim ──▶ NetworkConfig from slot ──▶ tap create   │
│        │                                        (tap-fcN)    │
│        ▼                                                     │
│    room.json records slot ──▶ RoomGuard::set_tap_owned(true) │
│                                                              │
│  teardown / gc / kill:                                       │
│    tap delete ──▶ slot file removed ──▶ registry reap        │
└──────────────────────────────────────────────────────────────┘
```

**Reused, not rebuilt:** `NetworkConfig` already carries `(tap_name, guest_ip, gateway_ip)` — the pool stops `main.rs` hardcoding it and derives it from a slot. `RoomGuard::set_tap_owned` is a shipped stub waiting for exactly this (guard teardown already branches on `tap_owned`). The registry's `room.json` + liveness probe is the source of truth for *which rooms exist*; slots add one field to it. `reap_orphan` grows one step (tap + slot release).

**The one new module:** `slot` (allocator) — sits beside `registry` in the layer map (`config/room → firecracker/rootfs/transport → runner/registry/slot → main`), no downward imports.

## 4. Key decisions & trade-offs

### D1 — IP layout: **/30 per slot carved from `172.16.0.0/24`** (not flat /24)

Each slot k (0-based) owns the /30 at `172.16.0.(4k)/30`: gateway (host/tap side) `.4k+1`, guest `.4k+2`. 64 slots max — far above any realistic cap on one host.

- *Why not flat /24 (all guests `.2/.3/.4…` behind per-room taps):* per-room TAPs are separate L2 segments; one /24 spanning N host interfaces forces per-guest /32 host routes and proxy-ARP-ish hacks. A /30 per tap is the boring, well-trodden per-VM layout: the kernel routes each /30 to its tap with zero extra configuration.
- *Cost:* guest netmask changes from `/24` to `/30` — `build_boot_args` currently bakes `255.255.255.0`; the netmask becomes a `NetworkConfig` field sourced from the slot. Legacy shared-tap boots keep working (slot 0's /30 ≠ today's addresses, so the migration is: pool rooms use slots; the old `tap-fc0`+`.2` path survives only until P2 lands, then `main.rs` always allocates).
- *Supernet stays `172.16.0.0/24`* for NAT + firewall matching — one rule set for all slots (see D4).

### D2 — TAP lifecycle: **created by `rooms run` at boot, deleted at teardown** (not a pre-provisioned tap pool)

`rooms run` already runs under `sudo` (jailer requirement), so it can `ip tuntap add tap-fc<k> mode tap user firecracker`, `ip addr add <gw>/30`, `ip link set up`, and the per-tap forwarding sysctl — the same four operations `setup-tap.sh` does today, moved into the boot path with the slot's values.

- *Why not pre-provisioned:* idle root-owned network devices for rooms that may never boot; a second thing for doctor to reconcile; no latency win that matters (tap create is milliseconds).
- `setup-tap.sh` splits: the **host-once** parts (ROOMS_FWD chain install, supernet NAT, outbound-iface forwarding) become `setup-tap.sh --host` (idempotent, doctor-checked); the **per-room** parts move into the binary. The script's env-tunable single-tap mode is deleted with P2, not kept as a parallel path.

### D3 — Allocator persistence: **O_EXCL slot files under the state base** (not registry-scan, not a lock daemon)

Claim = `create_new` (O_CREAT|O_EXCL) on `<state>/slots/<k>` containing the room id; the filesystem is the arbiter, so two concurrent `rooms run` invocations racing slot k get exactly one winner — loser advances to k+1. Free = remove the file (teardown/gc/kill). `room.json` *also* records the slot — for `ls` display and for gc's reconciliation — but the slot **file** is the allocation truth.

- *Why not registry-scan-and-pick:* scan-then-claim is a TOCTOU window under concurrent boots; fixing it needs a lock anyway. The slot file *is* the lock, with no daemon and no flock lifetime subtleties across sudo/jailer process trees.
- *Why not flock on a shared file:* advisory locks held across the room's multi-hour lifetime pin an fd in a process tree that jailer re-parents; a crash's lock release semantics get subtle. O_EXCL files have exactly the same crash-orphan story as rooms themselves — and gc already solves that story (see §7.3).

### D4 — Firewall: **`ROOMS_FWD` chain + `tap-fc+` wildcard, O(1) rules**

Installed once per host: `FORWARD -j ROOMS_FWD` (idempotent), and inside `ROOMS_FWD` the exact policy `setup-tap.sh` appends per-tap today, expressed once against the wildcard interface `tap-fc+` and the `172.16.0.0/24` supernet: RFC1918 DROPs (guest→LAN), guest→out ACCEPT, established return ACCEPT. NAT: one `POSTROUTING -s 172.16.0.0/24 -o <out> -j MASQUERADE`.

- *Why:* today's script appends 5 rules per tap into the global FORWARD; at N taps that's 5N rules interleaved with whatever else lives there, and teardown has to string-match them back out. A named chain owned by rooms is inspectable (`iptables -L ROOMS_FWD`), survives N without growing, and `--host` teardown is "flush + delete one chain."
- *Guest→guest isolation for free:* add one `tap-fc+ → tap-fc+`-shaped DROP (src+dst both in supernet) so parallel agent runs can't reach each other — this falls out of the chain design and closes a hole the single-room world never had. **This line is why the pool needs the adversarial review pass** (parallel rooms = new lateral-movement surface).

### D5 — Cap + full-pool semantics: **`--max-pool` (default 8), fail-fast, no queue**

Allocator tries slots 0..min(cap, 64); all claimed → exit with a distinct code + `pool full: <cap> rooms live` (and a `--json` error kind). Ship's driver treats it like any dispatch failure and retries on its own schedule.

- *Why 8:* the Hyper-V rooms-host has modest cores/RAM; 8 × (1 vCPU + 512 MB-ish) rooms is already generous. It's a knob, not a constant.
- *Why no queue:* a queue in rooms duplicates scheduling state the driver already owns, and a CLI that blocks indefinitely holding a sudo is operationally worse than one that says "full, come back."

## 5. Data model

**`room.json`** (registry) gains one optional object — additive, schema-compatible; absent = legacy shared-tap room:

```json
"slot": { "index": 3, "tap": "tap-fc3", "gateway": "172.16.0.13", "guest": "172.16.0.14", "prefix": 30 }
```

**Slot file** `<state>/slots/<k>`: single line, the room id. Created O_EXCL at claim; removed at free. Orphan story in §7.3.

**`NetworkConfig`** gains `netmask_prefix` (today implied /24); constructed from the slot instead of literals in `main.rs`.

No migrations: old rooms without `slot` are display-blank in `ls` and reap exactly as today (`tap_owned=false` → shared tap untouched).

## 6. API contract

```
rooms run …existing flags… [--max-pool <n>]     # n defaults 8; env ROOMS_MAX_POOL
rooms ls [--json]                               # + slot column / field
rooms doctor [--json]                           # + checks: ROOMS_FWD installed, slots/ writable,
                                                #   orphaned taps (tap-fc<k> with no live room)
setup-tap.sh --host                             # idempotent host-once install (chain, NAT, sysctls)
setup-tap.sh --host --teardown                  # flush + remove chain, restore recorded sysctls
```

**Slot module (internal):**

```rust
pub struct Slot { pub index: u8, pub tap: String, pub gateway: Ipv4Addr, pub guest: Ipv4Addr, pub prefix: u8 }
pub fn claim(state: &Path, room_id: &str, cap: u8) -> Result<Slot, SlotError>   // O_EXCL walk 0..cap
pub fn free(state: &Path, slot_index: u8) -> Result<(), SlotError>              // idempotent
pub fn reconcile(state: &Path, live: &[RoomRow]) -> Vec<Reclaimed>              // gc hook: slot files ∖ live rooms
```

**Errors:** `SlotError::PoolFull { cap }` (distinct exit code), `SlotError::Io`. Pool-full must be distinguishable from boot failure by ship's runner without string-matching.

## 7. Key flows

### 7.1 Boot (the changed path)

1. `rooms run` → `slot::claim` walks k = 0..cap: `create_new(slots/<k>)` → first success wins; all `AlreadyExists` → `PoolFull`.
2. Build `NetworkConfig` from the slot; `ip tuntap add <tap> … user firecracker`, `ip addr add <gw>/30`, `ip link set <tap> up`, per-tap forwarding sysctl.
3. `RoomGuard::set_tap_owned(true)`; write `slot` into `room.json` **before** the firecracker spawn (so a crash between claim and boot is reconcilable — the slot file names a room id whose room.json exists and whose liveness probe says dead → gc reclaims).
4. Boot proceeds exactly as today with the slot's cmdline IP args (netmask from `prefix`).

Failure at any step ≤ 3 unwinds in reverse (tap delete if created, slot free) via the existing guard — no new cleanup mechanism, the guard grows two steps.

### 7.2 Teardown / kill / gc (the freeing path)

Guard cleanup (normal exit), `kill` (signal + reap), and `gc` (orphan reap) all converge on `reap_orphan`; it grows: if `room.json` has a slot → delete the tap (`ip link del`, tolerate already-gone), then `slot::free`. Order matters — tap first, slot file last, so a crash mid-reap leaves the slot file as the breadcrumb and the next `gc` retries (freeing is idempotent).

### 7.3 Leak reclamation (the flow that keeps the pool honest)

A slot leaks when its claimer died between claim and room.json write, or a reap crashed mid-way. `gc` (and `doctor` read-only) runs `slot::reconcile`: for each `slots/<k>`, resolve the recorded room id against the registry — room absent, or present-and-dead → reclaim (delete any matching `tap-fc<k>`, remove the slot file). A slot whose room is **live** is never touched (symmetry with gc's confirmed-dead invariant). Rooms' pid-identity probe (from the kill work) is the liveness authority, so a reused pid can't make a dead room look alive.

### 7.4 Concurrent boot (what P3 proves)

N `rooms run` invocations race: claims serialize on the filesystem, taps are per-slot so no device contention, jailer dirs are already per-room. Host-e2e: boot 3 rooms concurrently, each runs a network task (distinct HTTP fetch), assert 3 distinct guest IPs, zero cross-talk (guest k cannot reach guest j — D4's isolation DROP), all slots freed after.

## 8. Concurrency / consistency / failure model

- **Claim race:** filesystem O_EXCL is the serialization point; no retry loop needed beyond the k-walk. Two racers on the last slot: one `PoolFull`, one winner — property-tested with threads in P1.
- **Crash windows:** every window leaves either (a) a slot file naming a dead/absent room → `reconcile` reclaims, or (b) an orphaned tap with no slot file → doctor flags, gc's tap sweep (taps matching `tap-fc<k>` with no live room) deletes. There is no window that requires a human.
- **Sudo/process-tree:** claim/free run in the pre-jailer parent (same privilege as today's boot path); nothing inside the guest or jailed child touches slot state.
- **Degraded mode:** ROOMS_FWD missing (host not set up) → boot fails at the doctor-precheck with the exact `setup-tap.sh --host` remediation, before a slot is claimed.

## 9. Rollout / implementation plan

Sequenced as one dossier phase (`multi-room-pool`), three PR-sized units, dependency-ordered. The **N=1 ship-on-rooms validation** (dossier `ship-on-rooms-n1-validation`, phase `02-operational-layer`) is the entry gate — it must pass (substrate holds under ship's real contract) before P1 dispatches.

| Unit | Goal | Depends | Gate | Scope (weighted LOC) |
| --- | --- | --- | --- | --- |
| **P1 slot allocator** | `slot` module: claim/free/reconcile, O_EXCL semantics, `room.json` slot field, `ls` display. Pure Rust, no root, property-tested race. | N=1 validation | unit+prop tests green | ~350–500 |
| **P2 per-room tap + chain** | Tap create/delete in boot/reap, `set_tap_owned` wired live, netmask-from-slot in boot args, `setup-tap.sh --host` split, ROOMS_FWD + isolation DROP, doctor checks. | P1 | host-e2e: single room on a slot, clean reap, doctor green | ~450–650 |
| **P3 concurrent boot + cap** | `--max-pool`/env, `PoolFull` error kind + exit code, concurrent-boot host-e2e (N=3, cross-talk assert), ship fan-out smoke (2 parallel ship-on-rooms streams). | P2 | **VALIDATION GATE:** 3 concurrent networked rooms, distinct branches pushed, zero cross-talk, all slots freed | ~300–450 |

Post-gate (NOT this TDD, tracked in `02-operational-layer`): snapshots+fork (own TDD — restore-into-slot is designed *against* this slot abstraction), replay receipts.

**Review posture:** P2 and P3 touch the firewall and parallel isolation — per the operator's convention, each gets an **adversarial Workflow pass** (skeptics briefed to break the gate: escape the /30, reach a sibling guest, leak a slot) in addition to the bot panel.

## 10. Open questions

1. **Default cap value** — 8 is a guess at the Hyper-V host's comfort; the P3 e2e should measure real memory/CPU at N=3 and inform the default. (Not blocking: it's a knob.)
2. **`kill --all`** — plan part 1 stretch; with the pool it becomes "drain." Worth folding into P3 or leaving for receipts-era? Leaning: leave out; `ls --json | xargs kill` composes.
3. **Doctor's orphan-tap sweep vs gc's** — read-only flag in doctor + acting sweep in gc, or both in gc only? Leaning doctor-flags/gc-acts (doctor never mutates).
4. **Snapshot re-attach contract** — restored room gets a *fresh* slot; does the guest's frozen IP config need in-guest re-negotiation, or does Firecracker's restored NIC simply sit on the new tap with the old IP (requiring the slot's IP to be *reserved* for restore)? Flagged now so the pool doesn't foreclose either answer; settled in the snapshots TDD.

## 11. Validation plan

The P3 gate is the v0.2 "hard parallelism" claim made falsifiable:

- **Binary signal:** on rooms-host, dispatch 3 concurrent `rooms run` (and separately, 2 parallel ship-on-rooms streams through `RoomCursorRunner`); PASS = every room boots on a distinct slot, completes its task, pushes its branch (ship case), guests cannot reach each other, and post-run `ip link` + `slots/` + `iptables -L ROOMS_FWD` are byte-identical to pre-run. FAIL on any leak, cross-talk, or double-allocation.
- **Race property (P1, pre-host):** threaded property test — K claimers × S slots, K > S → exactly S winners, K−S `PoolFull`, no duplicate indices. Runs in CI (no KVM needed).
- **No-regression:** existing single-room host-e2e suite green on every unit.
