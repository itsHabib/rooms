**Status**: ready — remaining execution slice after PRs #93 and #95
**Owner**: @mh
**Date**: 2026-07-29
**Model/effort**: opus / extra — the pause/create order and slot-ownership transfer are correctness-critical.
**Related**: dossier task `snapshot-create` (id: `tsk_01KYGMJCQW6GT0GKTS3NNGJY2X`), `docs/features/snapshot-fork-replay/spec.md` D2/D8

# Wire `rooms snapshot`: execute the landed plan and transfer slot ownership

## Scope

| Bucket | Files | Est. LOC | Weighted |
|---|---|---|---|
| Production | `src/firecracker.rs`, `src/main.rs`, existing `src/snapshot.rs` and `src/slot.rs` integration | ~260 | 260 |
| Tests | mocked API execution, artifact/permission failures, reservation transfer | ~180 | 90 |
| **Total** | | | **~350** |

Band: **ideal** (upper).

## Landed foundation

- PR #93: `snapshot::plan`, metadata schema, neutral/vsock/slot refusal, jail-visible paths, and atomic metadata writer.
- PR #95: snapshot-owned reservation tokens and lease/return semantics.

Do not redesign those APIs. This slice supplies the live Firecracker and CLI mechanism.

## Goal

Execute a Full Firecracker snapshot of a sealed neutral base, durably publish its protected artifact set, destroy the template, and transfer the frozen network slot to the snapshot without ever exposing it to the walk allocator.

## Behavior

- Add `rooms snapshot <room-id> [--out <dir>] [--json]`.
- Resolve the live room and require `RoomMeta::is_snapshottable`; there is no force bypass.
- Gather `firecracker --version`, the exact rootfs SHA-256, repo SHA when available, and live-vsock state. Build the existing `SnapshotRequest` and call `snapshot::plan`.
- Execute the returned operations verbatim against the room API socket:
  1. `PATCH /vm {"state":"Paused"}`
  2. `PUT /snapshot/create` with `snapshot_type:"Full"` and the plan's jail-visible paths.
- Create the host artifact directory at `0700`; keep memory, vmstate, and metadata owner-only. Refuse to overwrite a non-empty snapshot directory.
- Verify both Firecracker-created files exist and are non-empty before `snapshot::write_meta_atomic`. A completed `snapshot.json` must never advertise partial artifacts.
- After all three artifacts are durable, terminate the base and call the landed `slot::reserve` to transfer ownership from the base room to `snapshot_id`.
- Failure before snapshot creation attempts to resume a paused base. Failure after creation never silently frees the frozen slot; preserve a named recoverable state and the primary error.
- Emit human/JSON success with snapshot id, directory, base room, slot, and provenance.

## Acceptance

- A sealed neutral room produces non-empty `snapshot.vmstate`, `snapshot.mem`, and `snapshot.json`.
- A provisioning, tainted, slotless, or active-vsock room is refused before any pause.
- API requests use the exact jail-visible paths and `Paused → CreateFullSnapshot` order.
- Metadata is written only after both binary artifacts are durable.
- Successful completion leaves a never-reclaim snapshot reservation, not a free slot or dead base claim.
- Mocked failure tests cover pause, create, missing/empty files, metadata write, teardown, and reservation transfer.

## Test plan

Run `make check`. Use a mocked Unix-socket Firecracker API for request/order assertions. Exercise real `snapshot/create` on the rooms-host before landing and preserve the output as task evidence.

## Non-goals

- Restore (`restore-single`).
- N clones/netns (`fork-clones`).
- Checkpoint receipts (`checkpoint-receipts-harden`).
