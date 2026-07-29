**Status**: ready — remaining execution slice after PRs #94 and #95
**Owner**: @mh
**Date**: 2026-07-29
**Model/effort**: opus / extra — load-before-resume, custody ordering, and the hygiene gate are load-bearing.
**Related**: dossier task `restore-single` (id: `tsk_01KYGMJSHR5QGBSJ8B05F5N0ZG`), `docs/features/snapshot-fork-replay/spec.md` D4/D7

# Wire `rooms restore`: fresh Firecracker load, custody-before-resume, and hygiene ACK

## Scope

| Bucket | Files | Est. LOC | Weighted |
|---|---|---|---|
| Production | `src/firecracker.rs`, `src/main.rs`, `src/room.rs`, `src/registry.rs`, `src/runner.rs`, guest resume-agent protocol, existing `src/restore.rs` and `src/slot.rs` integration | ~470 | 470 |
| Tests | live-flow mechanism, custody ordering, lease/GC round-trip, ACK failure matrix | ~280 | 140 |
| **Total** | | | **~610** |

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
- Add `rooms restore <snapshot-dir> [--slot <k>] [--witness] [egress flags] [--json]`.
- Read `snapshot.json`; verify schema, `Neutral` provenance, Firecracker version, rootfs hash, and requested/original slot before creating the VM.
- Start a fresh jailed Firecracker process in an inert state: no tap/network, snapshot load, or guest execution. Atomically persist its PID/starttime and `snapshot_lineage` before acquiring the lease; if that liveness write fails, terminate the inert process and do not lease.
- Lease the snapshot reservation only after the room has a conclusive liveness/recovery breadcrumb. A busy or foreign slot fails closed with no fallback and tears down the inert process.
- Reuse jail/guard/staging mechanisms, but preserve restore's distinct order.
- Bind-mount `snapshot.mem` into the jail so later clones share the inode privately; the small vmstate file may be copied.
- Install witness and egress controls before `Resumed`.
- Execute the landed plan exactly: load snapshot first, resume, then serve the resume nudge and await the guest ACK.
- The guest nudge reseeds randomness, corrects the clock, assigns the new room/run identity, generates a fresh SSH host key in the overlay, starts `sshd`, and ACKs only after all steps succeed.
- No active vsock connection is assumed to survive the snapshot. The agent reconnects with bounded poll/retry and read deadlines.
- Re-probe SSH only after the ACK. Preserve the already-persisted additive `snapshot_lineage` through readiness and teardown.
- Every failure after lease acquisition attempts process/jail teardown. Call `release_lease` only after the reap-clean gate proves the jail and room directory are gone; incomplete cleanup retains the room's lease and recovery breadcrumb so `rooms gc` can finish teardown before returning ownership to the snapshot reservation. Preserve the primary error if cleanup also fails.
- Extend the registry/reap release descriptor to distinguish an ordinary room claim from a snapshot lease. For a leased orphan, reconstruct `snapshot_id` and lessee from `snapshot_lineage`, delete the tap only after clean reap, then call `release_lease`; never route an `@lease` token through `slot::free`.

## Acceptance

- A compatible snapshot restores on the same slot/IP and reaches SSH through a freshly keyed server.
- Custody and snapshot load both precede `Resumed`; no network transmit gap exists.
- `snapshot.mem` is bind-mounted, not copied.
- No hygiene ACK means no SSH/workload readiness.
- Compatibility mismatch loads nothing.
- A crash before Firecracker liveness is durable holds no lease; every crash after lease acquisition is classifiable as live or orphaned-dead and cannot strand the reservation in an `Unknown` room.
- Restore → teardown → restore again succeeds; the reservation persists and the slot is never walk-claimable.
- Clean teardown returns the lease to the reservation; incomplete teardown keeps the lease held until GC proves a clean reap, preventing slot/IP reuse over residue.
- After an incomplete teardown, `rooms gc` uses persisted lineage to return the lease to the reservation after clean reap; it cannot strand an `@lease` token or free the reservation.
- Two restores have distinct RNG output, corrected clocks, fresh host keys, and distinct room identity.

## Test plan

Run `make check`. Unit tests assert ordering, bind-mount behavior, compatibility refusal, inert-process liveness before lease, no lease on liveness-write failure, lease return after clean reap, lease retention after incomplete jail or room cleanup, registry reconstruction and GC-completed return, and the ACK gate. On the rooms-host: snapshot one sealed base, restore it twice, inject a crash immediately before and after lease acquisition, force one compatibility mismatch and one incomplete-cleanup recovery, and prove distinct RNG, clock, key, identity, custody-before-resume, and reservation recovery.

## Non-goals

- N-clone fan-out/netns (`fork-clones`).
- The P2 killer-demo gate.
