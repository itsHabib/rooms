**Status**: ready — remaining execution slice after PRs #94 and #95
**Owner**: @mh
**Date**: 2026-07-29
**Model/effort**: opus / extra — load-before-resume, custody ordering, and the hygiene gate are load-bearing.
**Related**: dossier task `restore-single` (id: `tsk_01KYGMJSHR5QGBSJ8B05F5N0ZG`), `docs/features/snapshot-fork-replay/spec.md` D4/D7

# Wire `rooms restore`: fresh Firecracker load, custody-before-resume, and hygiene ACK

## Scope

| Bucket | Files | Est. LOC | Weighted |
|---|---|---|---|
| Production | `src/firecracker.rs`, `src/main.rs`, `src/room.rs`, `src/registry.rs`, `src/rootfs.rs`, `src/runner.rs`, guest resume-agent protocol, existing `src/restore.rs` and `src/slot.rs` integration | ~560 | 560 |
| Tests | live-flow/lifecycle mechanism, custody ordering, resource-cleanup + lease/GC round-trip, ACK failure matrix | ~320 | 160 |
| **Total** | | | **~720** |

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
- Add `rooms restore <snapshot-dir> --image <path> (--keep | --command <cmd>) [--slot <k>] [--secret <ENV>]... [--out <dir>] [--witness] [egress flags] [--json]`; require exactly one lifecycle mode before any effect. `--out` and `--witness` conflict with `--keep`, and `--witness` requires `--out`, matching the existing capture lifecycle.
- Canonicalize the snapshot artifact directory and any command `--out` before process creation or lease acquisition. Require the trees to be disjoint: reject an output equal to, inside, or containing the snapshot directory so collection can never delete or overwrite this snapshot or sibling snapshots.
- Extend the existing pre-runtime `--secret` admission/harvesting to restore: validate every named host variable, remove it from the process environment before worker threads start, and carry the values only in the per-restore resume nudge. Never fall back to SSH `SendEnv` or ambient guest credentials.
- Read `snapshot.json`; validate the supplied rootfs, compute its hash, and verify schema, `Neutral` provenance, Firecracker version, rootfs hash, and requested/original slot before creating the VM.
- Start a fresh jailed Firecracker process in an inert state: no tap/network, snapshot load, or guest execution. Atomically persist its PID/starttime, `snapshot_lineage`, and a non-owning slot intent with the snapshot's exact derived slot/tap before acquiring the lease; leave `RoomMeta.slot` unset and TAP ownership false. Also durably index that recovery tombstone at `<state-base>/restore-intents/<room-id>.json`, outside the room and jail trees, so it survives room deletion. If either recovery-breadcrumb write fails, terminate the inert process and do not lease.
- Lease the snapshot reservation only after the room has a conclusive liveness/recovery breadcrumb. The lease token is authoritative and durable: the shared slot rewrite syncs the token file before rename and the slot directory after rename for both lease acquisition and return. A busy or foreign lease fails closed and pre-lease cleanup must neither delete the shared TAP nor release another room's lease. On success, atomically transition the room to leased ownership and set `RoomMeta.slot` before creating the TAP.
- Reuse jail/guard/staging mechanisms, but preserve restore's distinct order.
- Stage the supplied hash-verified rootfs at the snapshot's saved jail path as a read-only block device with the overlay init contract before `snapshot/load`; restore never mutates the backing image.
- Bind-mount `snapshot.mem` into the jail so later clones share the inode privately; the small vmstate file may be copied.
- Install witness and egress controls before `Resumed`.
- Execute the landed plan exactly: load snapshot first, resume, then serve the resume nudge and await the guest ACK.
- The guest nudge reseeds randomness, corrects the clock, assigns the new room/run identity, stages the admitted secrets in guest tmpfs with owner-only permissions, generates a fresh SSH host key in the overlay, starts `sshd`, and ACKs only after every hygiene and secret-delivery step succeeds.
- No active vsock connection is assumed to survive the snapshot. The agent reconnects with bounded poll/retry and read deadlines.
- Re-probe SSH only after the ACK. Preserve the already-persisted additive `snapshot_lineage` through readiness and teardown.
- After readiness, `--command` executes through the existing runner and drops the guard only after the workload result is captured; `--keep` explicitly suppresses guard cleanup, emits the live room id, and hands ownership to the persisted room. A bare restore is refused rather than immediately dropping a successful guard.
- Every failure after lease acquisition attempts process, jail, egress, TAP, and room teardown. Call `release_lease` only after the reap-clean gate proves the process dead, jail and room directory gone, egress chain removed, and TAP deletion succeeded or the TAP was already absent. Make TAP/egress removal fallible and idempotent instead of discarding command status. Keep the indexed restore tombstone until both resource cleanup and lease return are durably complete, then remove and directory-sync the tombstone; incomplete cleanup or a failed return retains it so `rooms gc` can retry. Preserve the primary error if cleanup also fails.
- Extend the registry/reap release descriptor to distinguish an ordinary room claim, a non-owning restore intent, and a snapshot lease. Scan indexed restore tombstones as well as room directories. Compare the persisted intent to the live slot token before any TAP deletion: foreign or still-reserved tokens own nothing; an exact lease to this room may delete its TAP after clean reap and then call `release_lease`. If the room directory is already absent, require the tombstone's recorded PID/starttime to be dead and both room and jail paths to be absent before returning that exact lease. Never route an `@lease` token through `slot::free`.

## Acceptance

- A compatible snapshot restores on the same slot/IP and reaches SSH through a freshly keyed server.
- `--keep` returns a live durable room id; `--command` runs one workload and then tears down; omitting both modes fails before process creation or lease acquisition.
- Restore secrets use the existing host-env admission rules, are removed from the launcher environment, arrive only through the acknowledged nudge, and never traverse SSH environment forwarding.
- A witnessed command restore requires `--out` and persists `witness.json` plus `witness.pcap`; capture/output modes are refused with `--keep`.
- Restore refuses any canonical snapshot/output tree overlap before creating a process or lease; output cleanup cannot erase snapshot artifacts.
- Restore requires a supplied rootfs whose hash matches metadata, stages it read-only at the saved jail path, and leaves its hash/mtime unchanged.
- Custody and snapshot load both precede `Resumed`; no network transmit gap exists.
- `snapshot.mem` is bind-mounted, not copied.
- No hygiene ACK means no SSH/workload readiness.
- Compatibility mismatch loads nothing.
- A crash before the recovery breadcrumb is durable holds no lease. A crash immediately before or after lease acquisition is classifiable as live or orphaned-dead; cleanup consults the exact lease token before claiming TAP ownership, so a failed competing restore cannot delete the active restore's TAP.
- Restore → teardown → restore again succeeds across process and host crashes; the durably synced reservation persists and the slot is never walk-claimable.
- Clean teardown returns the lease to the reservation; incomplete process, jail, room, egress, or TAP teardown keeps the lease held until GC proves a clean reap, preventing slot/IP reuse over residue.
- Room deletion never destroys the last recovery record for a live `@lease`; the indexed tombstone is cleared only after the exact return is durable.
- After an incomplete teardown or a crash between room deletion and durable lease return, `rooms gc` uses the indexed tombstone to return the exact lease after clean reap; it cannot strand an `@lease` token or free the reservation.
- Two restores have distinct RNG output, corrected clocks, fresh host keys, and distinct room identity.

## Test plan

Run `make check`. Unit tests assert lifecycle/output/witness parse admission, canonical snapshot/output ancestor and descendant rejection, restore secret harvesting and nudge delivery, no SSH-env fallback, keep handoff, command teardown, ordering, read-only rootfs staging and hash refusal, bind-mount behavior, compatibility refusal, non-owning room and indexed tombstone intent before lease, no lease on breadcrumb-write failure, durable lease/return token rewrites, a competing pre-lease cleanup never deleting the active TAP, idempotent GC immediately before and after leasing, checked egress/TAP teardown, lease return after clean reap, a crash between room deletion and lease return, lease retention after any incomplete resource cleanup, indexed reconstruction and GC-completed return, and the ACK gate. On the rooms-host: snapshot one sealed base, restore it twice with `--command --out` from the same unchanged image, exercise one `--keep` handoff, deliver one secret per restore, race a competing restore, inject a crash immediately before and after lease acquisition, force one compatibility mismatch and one incomplete-cleanup recovery, and prove distinct RNG, clock, key, identity, custody-before-resume, backing-image immutability, witness persistence, and reservation recovery.

## Non-goals

- N-clone fan-out/netns (`fork-clones`).
- The P2 killer-demo gate.
