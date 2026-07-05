**Status**: draft
**Owner**: @claude-code:michael
**Date**: 2026-07-02
**Related**: dossier task `pool-tap-lifecycle-fwd-chain` (id: `tsk_01KWFVK8A552FGJCDYP193S5QE`), [multi-room pool TDD](spec.md) §4 D2/D4, §7.1–7.3, §8

# P2: per-room tap lifecycle + ROOMS_FWD chain + setup-tap.sh --host split — design spec

## Scope

| Bucket | Files | Est. LOC | Weighted |
|---|---|---|---|
| Production source | `src/main.rs`, `src/firecracker.rs`, `src/registry.rs`, `src/slot.rs`, `src/doctor.rs`, `src/config.rs`, `scripts/setup-tap.sh` | ~400 | 400 |
| Tests | guard-unwind + reap-order units (fake command layer), e2e (feature-gated) | ~400 | 200 |
| **Total** | | | **~600** |

Band: **ideal** (< 700). TDD §9 estimates P2 at ~450–650 weighted.

## Goal

Networking is host-once-per-tap today: `setup-tap.sh` creates the single shared `tap-fc0` and appends 5 FORWARD rules per tap into the global chain. This unit moves tap creation into the boot path (per-slot values), grows every teardown path to delete the tap and free the slot, and replaces per-tap firewall appends with the O(1) `ROOMS_FWD` chain. After this unit a single room boots on a pool slot end-to-end.

## Behavior

### Boot path (TDD §7.1)

0. `main.rs` mints the room id **before** the claim (`RoomId::new`/`as_str` become `pub(crate)`, minting moves up out of `boot`) — the same id value is the slot-file contents, the room dir name, and `room.json.id`.
1. `slot::claim(state, id, me, cap)` — `me` is this process's `(pid, starttime)` identity.
2. Create the slot's tap: `ip tuntap add tap-fc<k> mode tap user firecracker`, `ip addr add <gw>/30`, link up, per-tap forwarding sysctl (the four operations `setup-tap.sh` does today, moved into the binary with slot values).
3. Build `NetworkConfig` from the slot. `NetworkConfig` gains `prefix: u8`; `build_boot_args` stops hardcoding `255.255.255.0` and interpolates a dotted-quad netmask via a named `prefix → dotted-quad` conversion at the `NetworkConfig`→boot-args boundary (`30 → 255.255.255.252`).
4. `RoomGuard::set_tap_owned(true)`; write `slot` into `room.json` **before** the firecracker spawn.
5. Failure at any step ≥ 2 unwinds in reverse via the existing guard (tap delete if created, then `slot::free(k, id)`) — the guard grows two steps.

### Teardown / kill / gc (TDD §7.2–7.3)

All three converge on `reap_orphan`. It grows: **gate the slot release on `reap_orphan` returning `Ok(())`** — if reap preserved the room dir (stranded-mount case), the slot stays claimed for a later retry. On clean reap: delete the tap (`ip link del`, tolerate already-gone), then `slot::free(k, this_room_id)` — compare-and-delete, so a reclaimed-and-reassigned index is left untouched (`AlreadyReassigned`). Order is tap-then-slot-file so a crash mid-reap leaves the slot file as the breadcrumb.

gc wires `slot::reconcile`: liveness comes from the slot file's own claimer token, probed with the same `(pid, starttime)` identity check `room::probe`/`terminate_by_identity` use. Confirmed-dead → reclaim (tap delete tolerate-gone, slot file removed, **room dir removed/tombstoned if present** so one corpse can't drive two frees). Alive or unknown → never reclaim. Claim-in-progress files (empty/short) → skip. Live rooms are never touched; the pid-identity probe is the authority.

Legacy no-slot rooms reap exactly as today (`tap_owned=false` → shared tap untouched).

### setup-tap.sh split (TDD §4 D2/D4)

- `--host` installs the once-per-host substrate, idempotently: `-I FORWARD 1 -j ROOMS_FWD` (**inserted at position 1**, not appended — a pre-existing broad FORWARD ACCEPT must not preempt isolation); inside `ROOMS_FWD`, source/dest-qualified by the `172.16.0.0/24` supernet: RFC1918 DROPs (guest→LAN), the **guest→guest isolation DROP** (`-s 172.16.0.0/24 -d 172.16.0.0/24`), guest→out ACCEPT, established-return ACCEPT **keeping its `-i <out_iface>` ingress scope**, and a self-terminating `-s 172.16.0.0/24 -j DROP` tail. NAT: one `POSTROUTING -s 172.16.0.0/24 -o <out> -j MASQUERADE`. Recorded sysctls. Include the version/supernet **marker rule** doctor keys on.
- `--host --teardown` flushes + removes the chain and restores recorded sysctls.
- The per-tap single-tap mode (env-tunable) is **deleted**, not kept as a parallel path.

### Doctor (TDD §6)

Read-only checks: `ROOMS_FWD` installed **and matches the allocator supernet** (marker rule, not existence-only); `slots/` present + local-filesystem + writable (the O_EXCL atomicity assumption, §4 D3); orphaned-tap flag (`tap-fc<k>` with no live room). Doctor flags; gc acts. Degraded mode: chain missing → boot fails at the doctor-precheck with the exact `setup-tap.sh --host` remediation, before a slot is claimed.

## Acceptance

- Host-e2e: single room boots on slot 1, task runs with egress, reap leaves `ip link` / `slots/` / `iptables -L ROOMS_FWD` byte-identical to pre-run.
- Kill and gc paths both free tap + slot; a fabricated orphan (slot file + dead claimer) is reclaimed by gc and flagged by doctor.
- Legacy no-slot rooms reap exactly as today.
- Guard unwind order verified: boot failure after tap create deletes the tap then frees the slot.

## Test plan

Unit tests for the guard unwind ordering + reap steps against a fake command layer (CI-runnable, `make check` green — cloud agent VMs have no KVM, per repo gotchas). Host-e2e (`cargo test --features e2e`) written by this unit but **executed on rooms-host as the batch gate**, not in the agent's sandbox.

## Review posture

Root-touching firewall + teardown-safety surface; crash windows (TDD §8) must be reconcilable without a human. Per the gate-change convention this PR gets an **adversarial Workflow pass before merge** (skeptics briefed to escape the /30, reach a sibling guest, leak a slot) in addition to the bot panel.

## Out of scope

Concurrency cap + `PoolFull` exit-code/`--json` surfacing at the CLI (P3); snapshots re-attach (own TDD). Design: [spec.md](spec.md) §4 D2/D4, §7.1–7.3, §8.
