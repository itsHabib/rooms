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
- **No snapshots/fork** — sequenced after the pool (own TDD). But this design must not *foreclose* it, and one correction from review matters: a Firecracker snapshot freezes the guest's static `ip=`/gateway into the restored kernel state, so a restored room canNOT simply take a fresh arbitrary slot (wrong IP → black-holed guest). The forward-compat hook, cheap now and expensive to retrofit: `claim` accepts an **optional target index** so restore can request *the same slot's IP* (reserve-by-index), rather than first-free-only. P1 adds the optional arg; the reserve semantics are settled in the snapshots TDD. (The earlier "re-attaches to a new slot / must not assume the guest negotiated its IP at boot" framing was backwards — noted and corrected here.)
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

### D1 — IP layout: **/30 per slot carved from `172.16.0.0/24`, slot 0 reserved for legacy** (not flat /24)

Pool slot k (1-based; **k=0 reserved**) owns the /30 at `172.16.0.(4k)/30`: gateway (host/tap side) `.4k+1`, guest `.4k+2`. So slot 1 = `tap-fc1` / gw `.5` / guest `.6`; 63 pool slots (k=1..63).

**Why slot 0 is reserved:** the k=0 /30 would derive `tap-fc0` / gw `172.16.0.1` / guest `172.16.0.2` — **byte-for-byte the legacy shared-tap addresses** (`scripts/setup-tap.sh`: `tap-fc0`, host `172.16.0.1/24`, guest `172.16.0.2`), differing only in mask. During the P2 coexistence window (legacy `tap-fc0` path still alive alongside slotted rooms), a pool boot on slot 0 would collide on `ip tuntap add tap-fc0` (EEXIST) or duplicate-IP a live legacy guest. Reserving k=0 makes the two address spaces disjoint by construction; the allocator starts at k=1. Once the legacy path is removed (end of P2), k=0 *may* be reclaimed by a follow-up, but the default stays k≥1 to keep the invariant simple.

- *Why not flat /24 (all guests `.2/.3/.4…` behind per-room taps):* per-room TAPs are separate L2 segments; one /24 spanning N host interfaces forces per-guest /32 host routes and proxy-ARP-ish hacks. A /30 per tap is the boring, well-trodden per-VM layout: the kernel routes each /30 to its tap with zero extra configuration.
- *Cost:* guest netmask changes from `/24` to `/30` — `build_boot_args` currently bakes `255.255.255.0`; the netmask becomes a `NetworkConfig` field sourced from the slot (`prefix`; see §5 for the prefix→dotted-quad conversion `build_boot_args` needs).
- *Supernet stays `172.16.0.0/24`* for NAT + firewall matching — one rule set for all slots (see D4).

### D2 — TAP lifecycle: **created by `rooms run` at boot, deleted at teardown** (not a pre-provisioned tap pool)

`rooms run` already runs under `sudo` (jailer requirement), so it can `ip tuntap add tap-fc<k> mode tap user firecracker`, `ip addr add <gw>/30`, `ip link set up`, and the per-tap forwarding sysctl — the same four operations `setup-tap.sh` does today, moved into the boot path with the slot's values.

- *Why not pre-provisioned:* idle root-owned network devices for rooms that may never boot; a second thing for doctor to reconcile; no latency win that matters (tap create is milliseconds).
- `setup-tap.sh` splits: the **host-once** parts (ROOMS_FWD chain install, supernet NAT, outbound-iface forwarding) become `setup-tap.sh --host` (idempotent, doctor-checked); the **per-room** parts move into the binary. The script's env-tunable single-tap mode is deleted with P2, not kept as a parallel path.

### D3 — Allocator persistence: **O_EXCL slot files under the state base** (not registry-scan, not a lock daemon)

Claim = `create_new` (O_CREAT|O_EXCL) on `<state>/slots/<k>` containing the room id; the filesystem is the arbiter, so two concurrent `rooms run` invocations racing slot k get exactly one winner — loser advances to k+1. Free = remove the file (teardown/gc/kill). `room.json` *also* records the slot — for `ls` display and for gc's reconciliation — but the slot **file** is the allocation truth.

- *Why not registry-scan-and-pick:* scan-then-claim is a TOCTOU window under concurrent boots; fixing it needs a lock anyway. The slot file *is* the lock, with no daemon and no flock lifetime subtleties across sudo/jailer process trees.
- *Why not flock on a shared file:* advisory locks held across the room's multi-hour lifetime pin an fd in a process tree that jailer re-parents; a crash's lock release semantics get subtle. O_EXCL files have exactly the same crash-orphan story as rooms themselves — and gc already solves that story (see §7.3).
- *Assumption (stated, doctor-checked):* `create_new`/O_EXCL is atomic **on a local filesystem** (the state base is `$HOME`-derived, currently local ext4). It is *not* reliably atomic over classic NFS/overlay; if the state base ever moves to a network share this guarantee breaks. Invariant: the `slots/` dir must be local — `doctor` adds a mount-type check (§6) so a misconfigured host fails loud rather than silently double-allocating.

### D4 — Firewall: **`ROOMS_FWD` chain + `tap-fc+` wildcard, O(1) rules**

Installed once per host: **`-I FORWARD 1 -j ROOMS_FWD`** (inserted at position 1, not appended — iptables is first-match, so appending lets a pre-existing broad `FORWARD ACCEPT` from Docker/tailscale/a permissive default policy preempt the isolation; the jump must run before anything else can accept a guest packet). Inside `ROOMS_FWD`, the policy expressed once and **source/dest-qualified by the `172.16.0.0/24` supernet** (not by interface name alone — see below): RFC1918 DROPs (guest→LAN), the guest→guest isolation DROP (`-s 172.16.0.0/24 -d 172.16.0.0/24`), guest→out ACCEPT, and the established-return ACCEPT **keeping its `-i <out_iface>` ingress scope** (an interface-agnostic ESTABLISHED,RELATED accept would let a conntrack-helper expectation carry a sibling→sibling packet past the isolation DROP). The chain ends with a **self-terminating `-s 172.16.0.0/24 -j DROP` tail** so rooms traffic can never fall through to whatever FORWARD policy lives below the jump. NAT: one `POSTROUTING -s 172.16.0.0/24 -o <out> -j MASQUERADE` (the `-s` supernet bound is also what keeps the `tap-fc+` wildcard from ever forwarding a non-rooms interface's traffic).

- *Why:* today's script appends 5 rules per tap into the global FORWARD; at N taps that's 5N rules interleaved with whatever else lives there, and teardown has to string-match them back out. A named chain owned by rooms is inspectable (`iptables -L ROOMS_FWD`), survives N without growing, and `--host` teardown is "flush + delete one chain."
- *Guest→guest isolation for free:* add one `tap-fc+ → tap-fc+`-shaped DROP (src+dst both in supernet) so parallel agent runs can't reach each other — this falls out of the chain design and closes a hole the single-room world never had. **This line is why the pool needs the adversarial review pass** (parallel rooms = new lateral-movement surface).

### D5 — Cap + full-pool semantics: **`--max-pool` (default 8), fail-fast, no queue**

Allocator tries slots 1..=min(effective_cap, 63); all claimed → exit **4** + `error_kind: "pool_full"` (§6). Ship's driver treats it like any dispatch failure and retries on its own schedule.

- **Cap is a host fact, not a per-caller flag.** `slots/` is host-global, so if each `rooms run` read its ceiling only from its own `--max-pool`, two concurrent drivers would disagree on "full" — one idles capacity while the other blows past the ceiling the cap exists to protect. So the source of truth is a **host cap** (a host config value; default 8), and `--max-pool <n>` / `ROOMS_MAX_POOL` can only *lower* it for a given invocation: `effective_cap = min(flag_or_env ?? host_cap, host_cap, 63)`. The resource ceiling is a *mechanism* concern (how many microVMs this box will admit) — it is not scheduling/queue/backpressure policy, which stays in the driver.
- **Two axes, both surfaced.** The `63` is the addressing ceiling (the /24 carve minus reserved slot 0); the host cap is the resource ceiling. `--max-pool > 63` is rejected at parse time with a message naming the /24 origin, so raising the default (§10 Q1) can't silently clamp.
- *Why 8:* the Hyper-V rooms-host has modest cores/RAM; 8 × (1 vCPU + 512 MB-ish) rooms is already generous. A knob, not a constant.
- *Why no queue:* a queue in rooms duplicates scheduling state the driver already owns, and a CLI that blocks indefinitely holding a sudo is operationally worse than one that says "full, come back."

## 5. Data model

**Room id — minted before the claim.** Today `RoomId::new`/`as_str` are private to `firecracker` and the id is generated *inside* `boot`. The pool moves id minting up into `main.rs` **before** `slot::claim`, so the same id value is the slot-file contents, the room dir name, and `room.json.id` — one identity threaded through all three. Without this, §7.1 would "write the room id" at a point where no id exists yet, and reconcile would have nothing stable to key on. (`RoomId::new` + `as_str` become `pub(crate)`.)

**Slot file** `<state>/slots/<k>` carries its **own liveness token**, not just the room id — because at claim time the room dir may not exist yet, so the registry can't yet vouch for the claimer. Contents (two lines, written in the single O_EXCL create+write):

```
<room_id>
<claimer_pid> <claimer_starttime>     # /proc/<pid>/stat field 22, the same identity tuple room::probe uses
```

The token lets reconcile decide liveness from the slot file *alone*, before any room.json exists. An empty/short/unparseable slot file (crash between the O_EXCL create and the write) is the explicit **claim-in-progress** state → skip, never reclaim (see §7.3).

**`room.json`** (registry) gains one optional object — additive, schema-compatible; absent = legacy shared-tap room:

```json
"slot": { "index": 3, "tap": "tap-fc3", "gateway": "172.16.0.13", "guest": "172.16.0.14", "prefix": 30 }
```

**`NetworkConfig`** gains `prefix: u8` (today implied /24). `build_boot_args` interpolates a **dotted-quad** netmask into the kernel `ip=` cmdline, so the pool adds a `prefix → dotted-quad` conversion (`30 → 255.255.255.252`) at the `NetworkConfig`→boot-args boundary — name it so P1/P2 don't rediscover it.

No migrations: old rooms without `slot` are display-blank in `ls` and reap exactly as today (`tap_owned=false` → shared tap untouched).

## 6. API contract

```
rooms run …existing flags… [--max-pool <n>]     # per-invocation ceiling; effective cap = min(n, host cap, 63)
rooms ls [--json]                               # + slot column / field
rooms doctor [--json]                           # + checks: ROOMS_FWD installed AND matches the allocator supernet
                                                #   (version/supernet marker rule, not existence-only);
                                                #   slots/ present + local-fs + writable;
                                                #   orphaned taps (tap-fc<k> with no live room)
setup-tap.sh --host                             # idempotent host-once install (chain, NAT, sysctls, marker rule)
setup-tap.sh --host --teardown                  # flush + remove chain, restore recorded sysctls
```

**Slot module (internal):**

```rust
pub struct Slot { pub index: u8, pub tap: String, pub gateway: Ipv4Addr, pub guest: Ipv4Addr, pub prefix: u8 }
pub struct Claimer { pub pid: u32, pub starttime: u64 }   // /proc/<pid>/stat identity, mirrors room::probe
// claim mints nothing — the caller passes the pre-minted room id (§5) and its own identity.
pub fn claim(state: &Path, room_id: &str, me: Claimer, cap: u8) -> Result<Slot, SlotError>  // O_EXCL walk 1..=cap
pub fn free(state: &Path, slot_index: u8, expected_room_id: &str) -> Result<Freed, SlotError>  // compare-and-delete
pub fn reconcile(state: &Path, registry: &Registry) -> Vec<Reclaimed>   // gc/doctor hook; liveness from slot token
```

**`free` is compare-and-delete**, not blind remove: it deletes the tap + slot file **only if** the slot file still names `expected_room_id`. A mismatch means index k was already reclaimed and reassigned to another room → `free` leaves it untouched and returns `Freed::AlreadyReassigned` (mirrors the existing `terminate_by_identity` pid guard — never act on a reused identity). This closes the ABA teardown where a stale `room.json` drives `ip link del tap-fc<k>` against a *live* room that reused the index.

**Errors / exit codes.** `SlotError::PoolFull { cap }`, `SlotError::Io`. Today every `RoomsError` maps to **exit 2** and guest command codes pass through `0..255` (lane-escape already owns **3**), so PoolFull needs its own reserved code the guest can't emit as a normal status: **reserve exit 4 for pool-full**, and `main.rs` must branch `PoolFull` *ahead* of the generic `Err → 2` arm. Because guest-code overlap can't be fully eliminated by an exit code alone, `rooms run --json` **also** emits a terminal record with `error_kind: "pool_full"` — ship's runner keys on that field, not on the code or a string. (Satisfies the §2 NFR "distinguishable without string-matching," which the first draft asserted but never delivered.)

## 7. Key flows

### 7.1 Boot (the changed path)

0. `main.rs` mints the room id (§5) — before anything else, so id is available to the claim.
1. `slot::claim(id, me, cap)` walks k = 1..=cap: for each, `create_new(slots/<k>)` then write `<id>\n<my_pid> <my_starttime>` — first success wins; all `AlreadyExists` → `PoolFull`. (k=0 is never walked — reserved for legacy, §4 D1.)
2. Build `NetworkConfig` from the slot; `ip tuntap add <tap> … user firecracker`, `ip addr add <gw>/30`, `ip link set <tap> up`, per-tap forwarding sysctl.
3. `RoomGuard::set_tap_owned(true)`; write `slot` into `room.json` before the firecracker spawn.
4. Boot proceeds with the slot's cmdline IP args (netmask = `prefix`→dotted-quad).

The slot file's own liveness token (step 1) is what makes every pre-`room.json` window reconcilable: even if the process dies before step 3 writes `room.json`, reconcile can probe the recorded claimer identity and reclaim once it's confirmed dead — it does not depend on the room dir existing. Failure at any step ≥ 2 unwinds in reverse (tap delete if created, then `slot::free(k, id)`) via the existing guard — the guard grows two steps.

### 7.2 Teardown / kill / gc (the freeing path)

Guard cleanup (normal exit), `kill` (signal + reap), and `gc` (orphan reap) all converge on `reap_orphan`. It grows: **gate the slot release on `reap_orphan` returning `Ok(())`** — if reap preserved the room dir (e.g. the stranded-mount case `cleanup_sync` already handles), the slot stays claimed so a later retry, not a premature free, handles it. On clean reap: delete the tap (`ip link del`, tolerate already-gone), then `slot::free(k, this_room_id)` — **compare-and-delete** (§6): free only fires if `slots/<k>` still names *this* room. If it names another id, index k was already reclaimed and reassigned → leave it (returns `AlreadyReassigned`). Order is tap-then-slot-file so a crash mid-reap leaves the slot file as the breadcrumb; the compare-and-delete guard makes the retry safe even if the index churned in between.

### 7.3 Leak reclamation (the flow that keeps the pool honest)

A slot leaks when its claimer died before `room.json` was written, or a reap crashed mid-way. `gc` (acting) and `doctor` (read-only) run `slot::reconcile`, which decides **from the slot file's own token**, not from the room dir's presence:

- **Empty / short / unparseable slot file** → *claim-in-progress* (crashed between O_EXCL create and the token write). **Skip** — never reclaim; a later `gc` re-checks. (A truly stuck one is caught by a generous age threshold + the claimer probe below, not by racing it.)
- **Token present** → probe the recorded `(pid, starttime)` with the same identity check `room::probe`/`terminate_by_identity` use:
  - claimer **confirmed dead** → reclaim: `ip link del tap-fc<k>` (tolerate gone), remove the slot file, **and remove/tombstone the room dir if one exists** so a single corpse can't drive two frees (the ABA source). 
  - claimer **alive**, or liveness **Unknown/indeterminate** → **skip** (never reclaim a live or unprovable room — symmetry with gc's confirmed-dead-only invariant).

This replaces the first draft's "room absent, or present-and-dead → reclaim", which was wrong two ways: a room *dir without `room.json`* classified Unknown and was reclaimed by neither reconcile nor gc (slot wedged forever), and reclaiming on mere `room.json` *absence* raced a still-booting room into double-allocation (the slot file legitimately exists before the room dir during jail staging). Keying on the slot's own claimer token closes both.

### 7.4 Concurrent boot (what P3 proves)

### 7.4 Concurrent boot (what P3 proves)

N `rooms run` invocations race: claims serialize on the filesystem, taps are per-slot so no device contention, jailer dirs are already per-room. Host-e2e: boot 3 rooms concurrently, each runs a network task (distinct HTTP fetch), assert 3 distinct guest IPs, zero cross-talk (guest k cannot reach guest j — D4's isolation DROP), all slots freed after.

## 8. Concurrency / consistency / failure model

- **Claim race:** filesystem O_EXCL is the serialization point; no retry loop needed beyond the k-walk. Two racers on the last slot: one `PoolFull`, one winner — property-tested with threads in P1.
- **Crash windows:** enumerated against the slot file's own liveness token (§5, §7.3), so none needs a human. (i) between O_EXCL create and token write → *claim-in-progress* file → reconcile skips, retries later. (ii) after token write, before `room.json` → reconcile probes the claimer identity, reclaims once confirmed dead (does not need the room dir). (iii) mid-reap → gate on `reap_orphan Ok(())` keeps the slot claimed if the room dir was preserved; the compare-and-delete `free` makes the retry safe even if index k churned. (iv) orphaned tap with no slot file → doctor flags, gc's tap sweep (a `tap-fc<k>` with no live room) deletes. The one thing that must hold: reclaim removes/tombstones the room dir, so a single corpse can never drive two frees against a reused index.
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
4. **Snapshot re-attach contract** — the guest's IP is boot-frozen into the restored kernel (§2), so restore needs the slot's IP *reserved by index*, not a fresh one. P1 lands the optional-target-index hook on `claim`; the full reserve/versioning/re-point semantics (and whether a receipt records the slot index) are settled in the snapshots TDD. Open only in *degree*, not direction.

## Changelog

- **2026-07-01 (v2, pre-merge):** applied the adversarial-pass verdict (MERGE-WITH-EDITS). Six load-bearing corrections: reserved slot 0 to end the legacy-alias collision (D1); slot file carries its own claimer liveness token so pre-`room.json` windows are reconcilable and the wedge/double-alloc pair is closed (§5, §7.3, §8); `free`/reap is compare-and-delete gated on `reap_orphan Ok(())`, killing the ABA teardown of a reused index (§6, §7.2); room id minted before claim (§5, §7.1); PoolFull reserves exit 4 + a `--json` `error_kind` field (§6); `-I FORWARD 1` + self-terminating supernet DROP + kept ingress scope so isolation can't be preempted or bypassed (D4). Folded to tasks: host-fact cap coherence (D5), the two-axis ceiling, prefix→dotted-quad conversion point, doctor supernet-match check. Original decisions D1–D5 unchanged.

## 11. Validation plan

The P3 gate is the v0.2 "hard parallelism" claim made falsifiable:

- **Binary signal:** on rooms-host, dispatch 3 concurrent `rooms run` (and separately, 2 parallel ship-on-rooms streams through `RoomCursorRunner`); PASS = every room boots on a distinct slot, completes its task, pushes its branch (ship case), guests cannot reach each other, and post-run `ip link` + `slots/` + `iptables -L ROOMS_FWD` are byte-identical to pre-run. FAIL on any leak, cross-talk, or double-allocation.
- **Race property (P1, pre-host):** threaded property test — K claimers × S slots, K > S → exactly S winners, K−S `PoolFull`, no duplicate indices. Runs in CI (no KVM needed).
- **No-regression:** existing single-room host-e2e suite green on every unit.
