**Status**: ready — remaining execution slice after PRs #94 and #95
**Owner**: @mh
**Date**: 2026-07-29
**Model/effort**: opus / extra — load-before-resume, custody ordering, and the hygiene gate are load-bearing.
**Related**: dossier task `restore-single` (id: `tsk_01KYGMJSHR5QGBSJ8B05F5N0ZG`), `docs/features/snapshot-fork-replay/spec.md` D4/D7

# Wire `rooms restore`: fresh Firecracker load, custody-before-resume, and hygiene ACK

## Scope

| Bucket | Files | Est. LOC | Weighted |
|---|---|---|---|
| Production | `src/firecracker.rs`, `src/main.rs`, `src/room.rs`, `src/registry.rs`, `src/rootfs.rs`, `src/runner.rs`, guest resume-agent protocol, existing `src/restore.rs` and `src/slot.rs` integration | ~500 | 500 |
| Tests | live-flow mechanism, custody ordering, lease/GC round-trip, ACK failure matrix | ~280 | 140 |
| **Total** | | | **~640** |

Band: **stretch**.

## Landed foundation

- PR #94: restore compatibility guard and ordered plan: `InstallCustody → LoadSnapshot → ResumeVm → ApplyHygieneNudgeAndAwaitAck`.
- PR #95: persistent snapshot reservation, one-live-lease semantics, and teardown return.
- The preceding driver slice installs the guest resume agent and leaves it snapshot-safe without a held vsock connection.

Do not turn restore into a boot flag or redesign the landed policy/slot primitives.

## Goal

Start a fresh jailed Firecracker process, lease the frozen slot, install custody before any guest execution, load the shared snapshot, resume it, and block readiness until every per-resume hygiene action is acknowledged.

## Behavior

- Add a live `restore(req: RestoreRequest) -> Result<Guard, FirecrackerError>` sibling to `boot()` that consumes `restore::plan_restore`.
- Add `rooms restore <snapshot-dir> --image <path> [--slot <k>] [--witness] [egress flags] [--json]`.
- Read `snapshot.json`; validate the supplied rootfs, compute its hash, and verify schema, `Neutral` provenance, Firecracker version, rootfs hash, and requested/original slot before creating the VM.
- Start a fresh jailed Firecracker process in an inert state: no tap/network, snapshot load, or guest execution. Atomically persist its PID/starttime, `snapshot_lineage`, and a non-owning slot intent with the snapshot's exact derived slot/tap before acquiring the lease; leave `RoomMeta.slot` unset and TAP ownership false. If that recovery-breadcrumb write fails, terminate the inert process and do not lease.
- Lease the snapshot reservation only after the room has a conclusive liveness/recovery breadcrumb. The lease token is authoritative and durable: the shared slot rewrite syncs the token file before rename and the slot directory after rename for both lease acquisition and return. A busy or foreign lease fails closed and pre-lease cleanup must neither delete the shared TAP nor release another room's lease. On success, atomically transition the room to leased ownership and set `RoomMeta.slot` before creating the TAP.
- Reuse jail/guard/staging mechanisms, but preserve restore's distinct order.
- Stage the supplied hash-verified rootfs at the snapshot's saved jail path as a read-only block device with the overlay init contract before `snapshot/load`; restore never mutates the backing image.
- Bind-mount `snapshot.mem` into the jail so later clones share the inode privately; the small vmstate file may be copied.
- Install witness and egress controls before `Resumed`.
- Execute the landed plan exactly: load snapshot first, resume, then serve the resume nudge and await the guest ACK.
- The guest nudge reseeds randomness, corrects the clock, assigns the new room/run identity, generates a fresh SSH host key in the overlay, starts `sshd`, and ACKs only after all steps succeed.
- No active vsock connection is assumed to survive the snapshot. The agent reconnects with bounded poll/retry and read deadlines.
- Re-probe SSH only after the ACK. Preserve the already-persisted additive `snapshot_lineage` through readiness and teardown.
- Every failure after lease acquisition attempts process/jail teardown. Call `release_lease` only after the reap-clean gate proves the jail and room directory are gone; incomplete cleanup retains the room's lease and recovery breadcrumb so `rooms gc` can finish teardown before returning ownership to the snapshot reservation. Preserve the primary error if cleanup also fails.
- Extend the registry/reap release descriptor to distinguish an ordinary room claim, a non-owning restore intent, and a snapshot lease. Compare the persisted intent to the live slot token before any TAP deletion: foreign or still-reserved tokens own nothing; an exact lease to this room may delete its TAP after clean reap and then call `release_lease`. Never route an `@lease` token through `slot::free`.

## Acceptance

- A compatible snapshot restores on the same slot/IP and reaches SSH through a freshly keyed server.
- Restore requires a supplied rootfs whose hash matches metadata, stages it read-only at the saved jail path, and leaves its hash/mtime unchanged.
- Custody and snapshot load both precede `Resumed`; no network transmit gap exists.
- `snapshot.mem` is bind-mounted, not copied.
- No hygiene ACK means no SSH/workload readiness.
- Compatibility mismatch loads nothing.
- A crash before the recovery breadcrumb is durable holds no lease. A crash immediately before or after lease acquisition is classifiable as live or orphaned-dead; cleanup consults the exact lease token before claiming TAP ownership, so a failed competing restore cannot delete the active restore's TAP.
- Restore → teardown → restore again succeeds across process and host crashes; the durably synced reservation persists and the slot is never walk-claimable.
- Clean teardown returns the lease to the reservation; incomplete teardown keeps the lease held until GC proves a clean reap, preventing slot/IP reuse over residue.
- After an incomplete teardown, `rooms gc` uses persisted lineage to return the lease to the reservation after clean reap; it cannot strand an `@lease` token or free the reservation.
- Two restores have distinct RNG output, corrected clocks, fresh host keys, and distinct room identity.

## Test plan

Run `make check`. Unit tests assert ordering, read-only rootfs staging and hash refusal, bind-mount behavior, compatibility refusal, non-owning intent before lease, no lease on breadcrumb-write failure, durable lease/return token rewrites, a competing pre-lease cleanup never deleting the active TAP, idempotent GC immediately before and after leasing, lease return after clean reap, lease retention after incomplete jail or room cleanup, registry reconstruction and GC-completed return, and the ACK gate. On the rooms-host: snapshot one sealed base, restore it twice from the same unchanged image, race a competing restore, inject a crash immediately before and after lease acquisition, force one compatibility mismatch and one incomplete-cleanup recovery, and prove distinct RNG, clock, key, identity, custody-before-resume, backing-image immutability, and reservation recovery.

## Non-goals

- N-clone fan-out/netns (`fork-clones`).
- The P2 killer-demo gate.
