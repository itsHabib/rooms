**Status**: draft
**Owner**: @claude-code:michael
**Date**: 2026-07-02
**Related**: dossier task `pool-concurrent-boot-cap` (id: `tsk_01KWFVKSDVTQXWTGZK0HNGXVYC`), [multi-room pool TDD](spec.md) §4 D5, §6, §7.4, §11

# P3: concurrent boot + --max-pool cap + fan-out validation gate — design spec

## Scope

| Bucket | Files | Est. LOC | Weighted |
|---|---|---|---|
| Production source | `src/main.rs`, `src/config.rs`, `src/error.rs`, `src/slot.rs` | ~150 | 150 |
| Tests | concurrent-boot + pool-full e2e (feature-gated), exit-code units | ~450 | 225 |
| **Total** | | | **~375** |

Band: **amazing** (< 500). TDD §9 estimates P3 at ~300–450 weighted.

## Goal

With slots (P1) and per-room taps (P2) in place, nothing yet proves N rooms run concurrently under the real contract, and the pool ceiling has no operator surface. This unit adds the cap flag, makes `PoolFull` machine-readable, and lands the host-e2e that IS the v0.2 "hard parallelism" validation gate. Mostly test harness + a flag; the invariants are pinned by P1/P2.

## Behavior

### Cap semantics (TDD §4 D5)

- **The cap is a host fact, not a per-caller flag.** `slots/` is host-global, so the source of truth is a host cap (host config value, default 8); `--max-pool <n>` / `ROOMS_MAX_POOL` can only *lower* it for an invocation: `effective_cap = min(flag_or_env ?? host_cap, host_cap, 63)`.
- **Two axes, both surfaced:** 63 is the addressing ceiling (the /24 carve minus reserved slot 0); the host cap is the resource ceiling. `--max-pool > 63` is rejected at parse time with a message naming the /24 origin.
- No queue, fail fast — scheduling/backpressure stays in ship's driver.

### PoolFull surfacing (TDD §6)

- Reserve **exit 4** for pool-full (2 = generic error, 3 = lane-escape, guest codes pass through). `main.rs` branches `PoolFull` *ahead* of the generic `Err → 2` arm.
- Because guest-code overlap can't be fully eliminated by an exit code alone, `rooms run --json` **also** emits a terminal record with `error_kind: "pool_full"` — ship's runner keys on that field, not on the code or a string.

### Host-e2e (TDD §7.4, §11)

- **Concurrent boot:** 3 simultaneous `rooms run`, distinct slots/IPs, each completes a network task (distinct HTTP fetch), guest k **cannot** reach guest j (D4 isolation DROP), post-run `ip link` + `slots/` + `iptables -L ROOMS_FWD` byte-identical to pre-run.
- **Pool-full:** N=cap live rooms → next `rooms run` exits 4 with a clear message; a freed slot is immediately claimable.
- **Ship fan-out smoke:** 2 parallel ship-on-rooms streams through `RoomCursorRunner`, both branches pushed. (Runs from the ship side; this repo's e2e only needs to leave the substrate ready.)

## Acceptance

- **VALIDATION GATE (v0.2 "hard parallelism"):** 3 concurrent networked rooms, zero cross-talk, zero double-allocation, all slots + taps freed after; 2 parallel ship-on-rooms streams push distinct branches.
- Pool-full: exit code 4 + `--json` `error_kind: "pool_full"`; freed slot immediately claimable.
- `--max-pool 64` rejected at parse time; `--max-pool` above the host cap clamps down to it.
- Measure real memory/CPU at N=3 on rooms-host and **record it on the dossier task** (informs the default cap — TDD §10 Q1).
- CI keeps P1's threaded claim property green.

## Test plan

Unit tests for cap resolution + exit-code branching (CI, `make check` green). Host-e2e (`cargo test --features e2e`) written by this unit but **executed on rooms-host as the phase's validation gate** — the agent's sandbox has no KVM. The N=3 resource measurement happens during that gate run.

## Review posture

This unit IS the v0.2 validation gate — per the gate-change convention, an **adversarial Workflow pass on the e2e's isolation asserts** before merge (can the asserts pass while cross-talk is actually possible?), in addition to the bot panel.

## Out of scope

Queueing/backpressure (driver-side), snapshots warm-boot, `kill --all`/drain (TDD §10 Q2 — leaning leave out; `ls --json | xargs kill` composes). Design: [spec.md](spec.md) §4 D5, §7.4, §11.
