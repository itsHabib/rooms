**Status**: ready — remaining execution slice after PRs #93 and #95
**Owner**: @mh
**Date**: 2026-07-29
**Model/effort**: opus / extra — the pause/create order and slot-ownership transfer are correctness-critical.
**Related**: dossier task `snapshot-create` (id: `tsk_01KYGMJCQW6GT0GKTS3NNGJY2X`), `docs/features/snapshot-fork-replay/spec.md` D2/D8

# Wire `rooms snapshot`: execute the landed plan and transfer slot ownership

## Scope

| Bucket | Files | Est. LOC | Weighted |
|---|---|---|---|
| Production | `src/config.rs`, `src/egress.rs`, `src/firecracker.rs`, `src/registry.rs`, `src/main.rs`, existing `src/snapshot.rs` and `src/slot.rs` integration | ~430 | 430 |
| Tests | mocked API execution, target staging/collection failures, GC fencing/abort, transaction recovery, reservation transfer | ~300 | 150 |
| **Total** | | | **~580** |

Band: **ideal** (upper).

## Landed foundation

- PR #93: `snapshot::plan`, metadata schema, neutral/vsock/slot refusal, jail-visible paths, and atomic metadata writer.
- PR #95: snapshot-owned reservation tokens and lease/return semantics.

Do not redesign those APIs. This slice supplies the live Firecracker and CLI mechanism.

## Goal

Execute a Full Firecracker snapshot of a sealed neutral base, durably commit its protected artifact set and frozen network slot as one recoverable transaction, and then destroy the template without ever exposing the slot to the walk allocator.

## Behavior

- Add `rooms snapshot <room-id> [--out <dir>] [--json]`.
- Add `rooms snapshot-recover [<snapshot-id>] [--json]`. Without an id it lists indexed pending transactions and their next safe action; with an id it resumes that transaction before attempting any live-room lookup.
- Resolve the live room and require `RoomMeta::is_snapshottable`; there is no force bypass.
- Gather `firecracker --version`, the exact rootfs SHA-256, repo SHA when available, and live-vsock state. Build the existing `SnapshotRequest` and call `snapshot::plan`.
- Default `--out` to `<state-base>/snapshots/<snapshot-id>`, outside every managed cleanup and transaction-index tree. Canonicalize an explicit output and require it to be disjoint in both directions from `<state-base>/snapshot-intents` and `<state-base>/restore-intents`; also scan all room-state directories plus every default or custom jail-instance root and refuse the output at or below any tree that `RoomGuard` or GC may delete, not only the base room's trees.
- Create the host artifact directory at `0700`; keep memory, vmstate, metadata, and transaction state owner-only. Refuse an unrelated non-empty directory, but resume an exact matching pending transaction idempotently.
- Before snapshot creation, durably write a pending intent at `<state-base>/snapshot-intents/<snapshot-id>.json` that embeds the complete planned `SnapshotMeta` verbatim, artifact paths, and completed transaction boundaries. Capture `created_at`, Firecracker version, rootfs hash, repo SHA, provenance, base identity, slot, and guest IP before the first effect so base-absent recovery publishes the original descriptor without recomputing mutable or unavailable facts. This stable index is outside the room and output trees and remains discoverable after either is absent.
- Before the API call, create the plan's jail-visible snapshot and memory targets inside the jail, owner-only and writable by the jailer's Firecracker uid/gid. Refuse symlinks or pre-existing unrelated targets. Record this staging boundary in the intent so recovery can distinguish no-create, partial-create, and complete-create states.
- Execute the returned operations verbatim against the room API socket:
  1. `PATCH /vm {"state":"Paused"}`
  2. `PUT /snapshot/create` with `snapshot_type:"Full"` and the plan's jail-visible paths.
- Firecracker's success response is not a durable transaction boundary. Before reservation or reap, explicitly collect the jail-created `/snapshot.vmstate` and `/snapshot.mem` into the plan's distinct host output paths. Copy each to a private temporary file in `--out`; preserve or set the final memory inode's owner to the configured jailer Firecracker uid/gid with no group/other access, so a later unprivileged `/snapshot/load` can read the bind-mounted inode while the root-owned `0700` artifact directory keeps it host-private. Apply ownership/mode before `sync_all`, rename into place, and sync the output directory.
- Verify both collected host files exist, are non-empty, and match the successful create operation, then durably record a create-and-collect-success boundary in the indexed intent. Recovery may reserve or publish only a pair covered by that synced marker; if the process or host died without it, treat even two non-empty files as an unrecoverable partial and take the terminal abort path. Strengthen `snapshot::write_meta_atomic` to sync its temporary file and the parent directory around the rename. A completed `snapshot.json` must never advertise partial artifacts after a host crash.
- After both binary artifacts are durable, call the landed `slot::reserve` while the live base still owns the claim, then cleanly terminate and reap the base. Strengthen the shared slot-token rewrite so it syncs the temporary file before rename and the slot directory after rename; `reserve`, `lease`, and `release_lease` must not report their transitions complete before that durable boundary. This protected handoff must leave no dead-claim interval in which `rooms gc` can free the frozen slot; cleanup observes the snapshot reservation and cannot return it to the walk allocator.
- Treat base reap as complete only after the process is dead, jail and room directory are gone, egress-chain removal succeeds, and TAP deletion succeeds or proves already absent. Make the shared TAP/egress teardown fallible and idempotent; on any residual or cleanup error, retain the snapshot intent and reservation so GC can retry before publication.
- Extend `rooms gc` to consult `snapshot-intents` before ordinary dead-room reap. An exact pending intent fences its matching base claim and jail tree from `slot::free` and deletion until the shared recovery state machine durably collects the artifacts and completes the reservation, or reports the transaction as protected pending. GC must never expose that slot to the walk allocator merely because the pre-reservation base process is dead.
- Give that recovery state machine a terminal abort for an exact, conclusively dead pre-reservation base whose jailed/host artifact pair is absent, partial, or invalid and therefore cannot be completed. The abort removes partial outputs, proves no reservation or metadata was published, cleanly reaps the jail, frees only the exact ordinary claim still owned by the base room, then removes and directory-syncs the intent. A reserved token is never abortable through this path.
- Publish `snapshot.json` only after the reservation is durable and the full process/jail/room/egress/TAP reap-clean gate succeeds; metadata is the public completion marker. Remove and directory-sync the indexed intent after publication. `snapshot-recover` must idempotently validate and finish any indexed transaction left before or after publication, including the reservation-complete/base-absent case, while the non-empty-directory guard continues to reject unrelated contents.
- Failure before snapshot creation attempts to resume a paused base. Failure after creation never silently frees the frozen slot. A failed reservation leaves the live base claim intact; a crash or teardown failure after reservation preserves the snapshot reservation and pending intent for recovery. Preserve the primary error.
- Emit human/JSON success with snapshot id, directory, base room, slot, and provenance.

## Acceptance

- A sealed neutral room produces non-empty `snapshot.vmstate`, `snapshot.mem`, and `snapshot.json`.
- A provisioning, tainted, slotless, or active-vsock room is refused before any pause.
- API requests use the exact jail-visible paths and `Paused → CreateFullSnapshot` order.
- Firecracker receives pre-staged owner-only jail targets writable by its jail uid/gid; it is never asked to create into an absent or unrelated path.
- The default artifact directory survives cleanup; an output overlapping either transaction-index tree or beneath any managed room/jail-instance cleanup tree is refused after canonicalization.
- Both jailed Firecracker outputs are durably collected into the advertised host artifact paths before reservation or reap; cleanup cannot delete the only copy.
- The collected memory inode remains private but is readable by the configured unprivileged Firecracker uid/gid used for restore; collection never silently replaces it with a root-only inode.
- Metadata is written only after both binary artifacts and their directory are synced, the slot reservation file and slot directory are synced, and the base is cleanly reaped; the atomic metadata publish is itself synced.
- Residual egress or TAP state blocks metadata publication and intent removal; GC retains the reservation and retries cleanup without exposing the slot.
- Successful completion leaves a never-reclaim snapshot reservation, not a free slot or dead base claim, and concurrent GC cannot observe an unowned handoff gap.
- A dead base with an exact pre-reservation intent is protected from ordinary GC free/reap until recovery completes or explicitly leaves it pending.
- An unrecoverable partial create reaches a terminal abort that frees only the exact still-owned ordinary claim; it cannot strand the slot or free a reservation.
- Every crash point from pending-intent creation through metadata publication is retryable without overwriting unrelated files or exposing a complete-looking unusable snapshot.
- A transaction whose base has already been reaped remains discoverable through `snapshot-recover`; recovery can publish the intent's exact planned `SnapshotMeta`, validate the artifacts, and clear the index without a live-room lookup or recomputation.
- Mocked failure tests cover output rejection under every managed room/jail tree and in both overlap directions for the snapshot/restore transaction indexes, writable jail-target staging/ownership/symlink refusal, pause, create, jail-to-host collection without losing Firecracker uid/gid readability, partial copies, missing/empty files, artifact sync, durable create-and-collect-success recording only after sync/validation, non-empty pairs without that marker, recoverable versus terminal partial-create classification, exact-claim abort, slot-token file/directory sync, GC before reservation, reservation-before-termination, concurrent GC at the handoff, failed egress/TAP teardown, incomplete jail/room teardown, metadata publication, indexed discovery with the base absent, and transaction recovery at every boundary.

## Test plan

Run `make check`. Use a mocked Unix-socket Firecracker API for request/order assertions. Exercise real `snapshot/create` on the rooms-host before landing, prove `/snapshot/load` can open the collected memory inode as the configured unprivileged Firecracker uid/gid, and preserve the output as task evidence.

## Non-goals

- Restore (`restore-single`).
- N clones/netns (`fork-clones`).
- Checkpoint receipts (`checkpoint-receipts-harden`).
