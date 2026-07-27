**Status**: draft
**Owner**: @mh
**Date**: 2026-07-27
**Model/effort**: opus / extra — the disjoint restore API flow + load-before-resume ordering + jail staging are the load-bearing correctness of the whole feature.
**Related**: dossier task `restore-single` (id: `tsk_01KYGMJSHR5QGBSJ8B05F5N0ZG`), design doc `docs/features/snapshot-fork-replay/spec.md` §4 D4, §6, §7B, §7E

# restore() sibling + `rooms restore` — design spec

## Scope

| Bucket | Files | Est. LOC | Weighted |
|---|---|---|---|
| Production source | `src/firecracker.rs` (new `restore()` sibling to `boot()`, ~:1232), `src/slot.rs` (target-claim hook ~:80), `src/runner.rs` (re-probe ~:104), `src/room.rs` (`snapshot_lineage` additive ~:17), `src/main.rs` (CLI) | ~240 | 240 |
| Tests | restore flow, compat-guard branches, slot target reclaim | ~160 | 80 |
| **Total** | | | **~320** |

Band: **ideal** (upper) per repo's PR sizing convention.

## Goal

Nothing restores a snapshot. Restore is a **different** Firecracker flow from boot: `/snapshot/load` runs **before any config** (no boot-source, no drive PUTs), then the VM goes to `Resumed`. Doing the restore on the **same slot** the snapshot froze avoids every network hard problem (the frozen guest IP is reclaimed intact), so it's the right first proof — a single clone on the origin slot.

## Behavior / fix

- **New `restore(req: RestoreRequest) -> Result<Guard, FirecrackerError>`** — a **sibling to `boot()`** (design D4 — *not* a flag on `boot()`/`configure_vm`, `firecracker.rs:1232`), sharing the jail / guard / staging plumbing.
- **`rooms restore <snap> [--slot <k>]`**:
  - `claim(target: Some(k))` **consumes the snapshot-owned reservation token** left by task `snapshot-create` (design D8 v4) — returns `TargetTaken` if the slot is busy, **never** a silent fallback to another slot. (The token is why the frozen IP's slot is still available: the base was torn down but the slot was transferred, not freed, so no concurrent room could steal it.)
  - **`snapshot.mem` is bind-mounted (not copied) into the jail** (design D6 v4, boot-path precedent `firecracker.rs:29-31/951-952`) so N clones later `MAP_PRIVATE` the same inode and CoW-share; `snapshot.vmstate` (small, read once) may be copied.
  - **install the witness pcap + egress chain BEFORE resume** (design FR7 v4, same fail-closed posture as boot `firecracker.rs:478-498`) — a warm restored guest has network up instantly with no boot delay, so custody must exist before `Resumed` or the guest transmits in the gap. `restore` accepts the same `--witness` / egress flags as `boot`.
  - fresh FC process; `PUT /snapshot/load {mem_backend:{backend_type:"File"}}` **BEFORE** any other config → `PATCH /vm {state:"Resumed"}`.
- **FR4 compat guard:** before load, compare `snapshot.json` `fc_version` + `rootfs_hash` against the host; on mismatch → `FirecrackerError::SnapshotIncompatible{field}` (**fail closed** — never a best-effort load).
- **Resume-apply agent + hygiene nudge + ack gate (design D7).** The resume-apply agent captured in the snapshot is a **poll-retry** consumer (connect → short-timeout → `nanosleep` → retry, each attempt with a read deadline — it holds *no* connection across snapshot, since FC severs active vsock connections on resume). On resume it applies the nudge — **reseed kernel RNG, step clock, generate a fresh `sshd` host key into the overlay and start `sshd`, deliver identity/`run_id`** — then ACKs. Host gates `workload_started` on the ack: **no ack ⇒ no workload**. Even a *single* restore reseeds — a warm base reused twice must not repeat RNG/clock/host-key.
- **Re-probe SSH:** the pre-snapshot TCP connection is dead; re-run `wait_for_ssh` (`runner.rs:104`) against the **freshly-keyed `sshd`** after resume.
- `room.json` gains **`snapshot_lineage`** (additive field, `room.rs:17` — v-bump the persisted schema).

## Acceptance

- `rooms restore` on a fresh snapshot reaches SSH-ready on the **same slot/IP** against a **freshly-keyed `sshd`**; `room::probe` liveness works against the restored guest.
- A snapshot with a mismatched `fc_version` **or** `rootfs_hash` → `SnapshotIncompatible{field}`, **nothing loaded**.
- Same-slot restore has **zero IP collision** (the reservation token is consumed, not double-allocated).
- Witness + egress are active **before** the guest resumes (no transmit gap); `restore` honors `--witness` / egress flags.
- The hygiene nudge is **acked before `workload_started`**; a withheld ack ⇒ no workload (fail closed).

## Test plan

Rust unit tests: restore flow (assert witness/egress install **and** `snapshot/load` both precede `Resumed`, and `snapshot/load` precedes any other config), both compat-guard branches (fc_version mismatch, rootfs_hash mismatch), reservation-token consume (`TargetTaken` on busy, no silent fallback), ack-gate (no ack ⇒ no workload). **Phase intermediate gate** (rooms-host): snapshot a room, restore it **twice**, confirm both reach workload-ready, compat guard fires on a forced mismatch, and the two restores show **distinct kernel RNG draws, a resynced clock, and distinct `sshd` host keys** (hygiene fires on every restore).

## Non-goals

- N clones / netns / vsock nudge (phase P2 `fork-clones`).
- the killer demo (phase P2 validation gate).
