**Status**: ready — remaining execution slice after PRs #92 and #96
**Owner**: @mh
**Date**: 2026-07-29
**Model/effort**: opus / extra — this is the load-bearing security primitive that keeps secrets and duplicated identity out of `snapshot.mem`.
**Related**: dossier task `sealed-neutral-base` (id: `tsk_01KYGQWEGWMT8VS080BGG9BZ5Y`), `docs/features/snapshot-fork-replay/spec.md` D2/D7

# Finish sealed neutral-base: agent provisioning, verified quiesce, and seal

## Scope

| Bucket | Files | Est. LOC | Weighted |
|---|---|---|---|
| Production | guest provisioning/resume agent, rootfs service wiring, `src/vsock.rs`, `src/runner.rs`, `src/main.rs` | ~340 | 340 |
| Tests | provisioning protocol, beacon-gated seal, no-SSH base shape, failure cleanup | ~220 | 110 |
| **Total** | | | **~450** |

Band: **stretch**.

## Landed foundation

- PR #92: persisted `Provisioning | Neutral | Tainted` state machine.
- PR #96: distinct `rooms base-create`, `/vsock` without secrets, no workload launch, warm-failure teardown, and a persisted `Provisioning` candidate.

Do not reimplement these pieces. Start from `origin/main` after PR #96 and finish the transition to `Neutral`.

## Goal

PR #96 deliberately stops at `Provisioning`; it warms through SSH and cannot be snapshotted. Replace that provisional path with credential-free agent provisioning, verify the guest is quiet, and persist `Neutral` only after a terminal vsock beacon.

## Decision

No pre-snapshot SSH. A `base-create` guest must never start `sshd` or load a host private key. Repo staging and the optional warm command run through the guest agent channel. Ordinary `rooms run` behavior remains unchanged.

## Behavior

- Install a minimal guest provisioning/resume agent and order it before `sshd` in the rootfs.
- Give provisioning a dedicated vsock port and typed framing; do not overload the first-read-then-delete secrets port.
- Resolve `--repo` on the host. Create a git bundle without embedding host credentials, serve it with the optional warm command, and require phase ACKs after stage, clone, and warm.
- In base mode, suppress `sshd`, verify there is no listener/session, stop non-essential services, and validate the retained process set structurally.
- Remove the staged bundle and command payload before sealing.
- Close every provisioning connection, emit one exact `quiesced` beacon on a separate one-shot endpoint, then enter the snapshot-safe poll/retry wait used by restore. Hold no connection across the snapshot.
- The host waits with a bounded timeout. Only the exact beacon permits `RoomMeta::seal` followed by atomic `room.json` persistence.
- A malformed beacon, timeout, agent error, unexpected process, or persistence error leaves the room non-neutral and tears the candidate down.
- A snapshot-capable base must not load baked host keys. Restored rooms later generate a fresh overlay key before starting `sshd`.

## Acceptance

- `rooms base-create --repo ... --warm ...` completes with `provenance=neutral`.
- Repo content is present, while no repository credential was delivered to the guest.
- No `sshd` process, listener, session child, or loaded host private key exists at seal time.
- `Neutral` is persisted only after the exact beacon and after its connection closes.
- The retained agent waits without a live vsock connection and can reconnect after restore.
- Every failure branch tears down or remains `Provisioning`; none accidentally seals.
- Plain `rooms run` remains unchanged.

## Test plan

Run `make check`. Add protocol tests for framing and ACK order; refusal tests for malformed/late beacon and unexpected processes; persistence/cleanup tests; and rootfs-script tests proving base mode suppresses `sshd`. Validate the bundle transfer, warm command, retained process set, beacon, and durable `Neutral` on the rooms-host before landing.

## Non-goals

- Firecracker snapshot execution (`snapshot-create`).
- Restore and per-resume hygiene (`restore-single`).
- N-clone fan-out (`fork-clones`).
