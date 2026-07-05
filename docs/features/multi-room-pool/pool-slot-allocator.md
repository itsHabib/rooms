**Status**: draft
**Owner**: @claude-code:michael
**Date**: 2026-07-02
**Related**: dossier task `pool-slot-allocator` (id: `tsk_01KWFVJKNYTEP3AQKAEE6G3F3Z`), [multi-room pool TDD](spec.md) §4 D1/D3, §5, §6

# P1: slot allocator — design spec

## Scope

| Bucket | Files | Est. LOC | Weighted |
|---|---|---|---|
| Production source | `src/slot.rs` (new), `src/room.rs`, `src/main.rs`, `src/error.rs`, `src/lib.rs` | ~250 | 250 |
| Tests | unit + proptest in `src/slot.rs` / `src/room.rs` | ~350 | 175 |
| **Total** | | | **~425** |

Band: **amazing** (< 500) per repo's PR sizing convention. TDD §9 estimates P1 at ~350–500 weighted.

## Goal

One shared tap + hardcoded guest IP means one networked room at a time. This unit adds the race-safe, daemon-free way to hand out per-room network slots: a new `slot` module whose allocation truth is O_EXCL slot files on a local filesystem. Pure Rust, no root, no tap creation, no boot-path wiring — those are P2.

> **Authority note:** the merged TDD ([spec.md](spec.md)) is v2 — it applied an adversarial-pass verdict *after* the dossier task body was written. Where the task body and the TDD disagree (slot-0 reservation, slot-file contents, `free` semantics), **the TDD wins**. The deltas are folded in below.

## Behavior

**Layering:** `slot` sits beside `registry` in the layer map (`config/room → firecracker/rootfs/transport → runner/registry/slot → main`); no downward imports.

**Slot derivation (TDD §4 D1).** Pool slot k (1-based) owns the /30 at `172.16.0.(4k)/30`: tap `tap-fc<k>`, gateway `.4k+1`, guest `.4k+2`, `prefix: 30`. **Slot 0 is reserved** — its /30 derives the legacy shared-tap addresses byte-for-byte (`tap-fc0` / `172.16.0.1` / `.2`) and would collide with the legacy path during the P2 coexistence window. The allocator walks **k = 1..=cap**, giving 63 pool slots max. (The task body's "k=0..cap, 64 max" predates this correction.)

**Public surface (TDD §6):**

```rust
pub struct Slot { pub index: u8, pub tap: String, pub gateway: Ipv4Addr, pub guest: Ipv4Addr, pub prefix: u8 }
pub struct Claimer { pub pid: u32, pub starttime: u64 }   // /proc/<pid>/stat field 22, mirrors room::probe identity

pub fn claim(state: &Path, room_id: &str, me: Claimer, cap: u8) -> Result<Slot, SlotError>
pub fn free(state: &Path, slot_index: u8, expected_room_id: &str) -> Result<Freed, SlotError>
pub fn reconcile(state: &Path, registry: &Registry) -> Vec<Reclaimed>
```

- `claim` mints nothing — the caller passes a pre-minted room id (P2 moves id minting up in `main.rs`; this unit's tests pass ids directly). It also accepts an **optional target index** (reserve-by-index hook for the future snapshots TDD, per §10 Q4 — the arg lands now, the reserve semantics later; an `Option<u8>` parameter or a small builder, agent's choice within the small-sharp-API principle).
- Claim = `create_new` (O_CREAT|O_EXCL) on `<state>/slots/<k>` — the filesystem is the race arbiter; loser advances to k+1; all claimed → `SlotError::PoolFull { cap }` as a distinct error kind (not a stringly boot error).
- **Slot file contents (TDD §5)** — two lines, written in the single create+write:

  ```
  <room_id>
  <claimer_pid> <claimer_starttime>
  ```

  The liveness token lets `reconcile` judge a claimer before any `room.json` exists. An empty/short/unparseable file is the explicit *claim-in-progress* state → skip, never reclaim.
- **`free` is compare-and-delete**, not blind remove: delete `slots/<k>` only if it still names `expected_room_id`; a mismatch returns `Freed::AlreadyReassigned` and touches nothing (closes the ABA teardown, TDD §6/§7.2).
- `reconcile` decides from the slot file's own token (TDD §7.3): token present + claimer confirmed-dead → reclaimable; claimer alive or liveness unknown → skip; claim-in-progress → skip. Returns the reclaimable set; *acting* on it (tap delete, room-dir tombstone) is P2's gc wiring — this unit only classifies and removes the slot file for confirmed-dead claimers with no room dir to consider.

**`room.json` (TDD §5).** `RoomMeta` gains one optional, additive `slot` object:

```json
"slot": { "index": 3, "tap": "tap-fc3", "gateway": "172.16.0.13", "guest": "172.16.0.14", "prefix": 30 }
```

Absent = legacy shared-tap room; no migration. `rooms ls` (and `--json`) display the slot; legacy rooms show blank.

## Acceptance

- Threaded property test: K claimers × S slots, K > S → exactly S winners, no duplicate indices, K−S `PoolFull`. Runs in CI (no KVM).
- Slot 0 is never claimed (walk starts at k=1); derived addresses for k=1 are `tap-fc1` / `172.16.0.5` / `172.16.0.6`.
- `free` with a stale room id returns `AlreadyReassigned` and leaves the file.
- A claim-in-progress slot file (empty/short) is never reclaimed by `reconcile`.
- `room.json` round-trips with and without `slot` (legacy rooms unaffected).
- `PoolFull` is a distinct error kind.

## Test plan

Unit + proptest in CI; `make check` green. No host-e2e needed for this unit. Suggested names: `claim_race_exactly_s_winners` (proptest), `claim_skips_reserved_slot_zero`, `free_mismatched_id_returns_already_reassigned`, `reconcile_skips_claim_in_progress`, `room_json_roundtrip_without_slot`.

## Out of scope

Tap lifecycle, ROOMS_FWD, boot-path wiring (including the id-minting move in `main.rs`), gc/doctor integration of `reconcile`, cap flag + exit-code surfacing (P2/P3). Design: [spec.md](spec.md) §4 D1/D3, §5, §6.
