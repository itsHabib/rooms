**Status**: ready — remaining execution slice after PRs #93 and #95
**Owner**: @mh
**Date**: 2026-07-29
**Model/effort**: opus / extra — the pause/create order and slot-ownership transfer are correctness-critical.
**Related**: dossier task `snapshot-create` (id: `tsk_01KYGMJCQW6GT0GKTS3NNGJY2X`), `docs/features/snapshot-fork-replay/spec.md` D2/D8

# Wire `rooms snapshot`: execute the landed plan and transfer slot ownership

## Scope

| Bucket | Files | Est. LOC | Weighted |
|---|---|---|---|
| Production | `src/config.rs`, `src/firecracker.rs`, `src/main.rs`, existing `src/snapshot.rs` and `src/slot.rs` integration | ~320 | 320 |
| Tests | mocked API execution, artifact/permission failures, transaction recovery, reservation transfer | ~220 | 110 |
| **Total** | | | **~430** |

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
- Execute the returned operations verbatim against the room API socket:
  1. `PATCH /vm {"state":"Paused"}`
  2. `PUT /snapshot/create` with `snapshot_type:"Full"` and the plan's jail-visible paths.
- Default `--out` to `<state-base>/snapshots/<snapshot-id>`, outside every per-room directory that `RoomGuard` may delete. Refuse an explicit output at or below the base room directory.
- Create the host artifact directory at `0700`; keep memory, vmstate, metadata, and transaction state owner-only. Refuse an unrelated non-empty directory, but resume an exact matching pending transaction idempotently.
- Before snapshot creation, durably write a pending intent at `<state-base>/snapshot-intents/<snapshot-id>.json` that binds snapshot id, base room, slot, artifact paths, and completed transaction boundaries. This stable index is outside the room and output trees and remains discoverable after either is absent.
- Verify both Firecracker-created files exist and are non-empty, `sync_all` each file, and sync the artifact directory before publishing metadata. Strengthen `snapshot::write_meta_atomic` to sync its temporary file and the parent directory around the rename. A completed `snapshot.json` must never advertise partial artifacts after a host crash.
- After both binary artifacts are durable, call the landed `slot::reserve` while the live base still owns the claim, then cleanly terminate and reap the base. This protected handoff must leave no dead-claim interval in which `rooms gc` can free the frozen slot; cleanup observes the snapshot reservation and cannot return it to the walk allocator.
- Publish `snapshot.json` only after the reservation is durable and the base reap-clean gate succeeds; metadata is the public completion marker. Remove and directory-sync the indexed intent after publication. `snapshot-recover` must idempotently validate and finish any indexed transaction left before or after publication, including the reservation-complete/base-absent case, while the non-empty-directory guard continues to reject unrelated contents.
- Failure before snapshot creation attempts to resume a paused base. Failure after creation never silently frees the frozen slot. A failed reservation leaves the live base claim intact; a crash or teardown failure after reservation preserves the snapshot reservation and pending intent for recovery. Preserve the primary error.
- Emit human/JSON success with snapshot id, directory, base room, slot, and provenance.

## Acceptance

- A sealed neutral room produces non-empty `snapshot.vmstate`, `snapshot.mem`, and `snapshot.json`.
- A provisioning, tainted, slotless, or active-vsock room is refused before any pause.
- API requests use the exact jail-visible paths and `Paused → CreateFullSnapshot` order.
- The default artifact directory survives base cleanup; an output inside the base room is refused.
- Metadata is written only after both binary artifacts and their directory are synced, the slot is reserved, and the base is cleanly reaped; the atomic metadata publish is itself synced.
- Successful completion leaves a never-reclaim snapshot reservation, not a free slot or dead base claim, and concurrent GC cannot observe an unowned handoff gap.
- Every crash point from pending-intent creation through metadata publication is retryable without overwriting unrelated files or exposing a complete-looking unusable snapshot.
- A transaction whose base has already been reaped remains discoverable through `snapshot-recover`; recovery can publish the validated artifacts and clear the index without a live-room lookup.
- Mocked failure tests cover pause, create, missing/empty files, sync, reservation-before-termination, concurrent GC at the handoff, incomplete teardown, metadata publication, indexed discovery with the base absent, and transaction recovery at every boundary.

## Test plan

Run `make check`. Use a mocked Unix-socket Firecracker API for request/order assertions. Exercise real `snapshot/create` on the rooms-host before landing and preserve the output as task evidence.

## Non-goals

- Restore (`restore-single`).
- N clones/netns (`fork-clones`).
- Checkpoint receipts (`checkpoint-receipts-harden`).
