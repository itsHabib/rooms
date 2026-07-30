**Status**: ready — remaining execution slice after PRs #92 and #96
**Owner**: @mh
**Date**: 2026-07-29
**Model/effort**: opus / extra — this is the load-bearing security primitive that keeps secrets and duplicated identity out of `snapshot.mem`.
**Related**: dossier task `sealed-neutral-base` (id: `tsk_01KYGQWEGWMT8VS080BGG9BZ5Y`), `docs/features/snapshot-fork-replay/spec.md` D2/D7

# Finish sealed neutral-base: agent provisioning, verified quiesce, and seal

## Scope

| Bucket | Files | Est. LOC | Weighted |
|---|---|---|---|
| Production | guest provisioning/resume agent, rootfs service wiring, `src/egress.rs`, `src/firecracker.rs`, `src/vsock.rs`, `src/runner.rs`, `src/main.rs` | ~340 | 340 |
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

No pre-snapshot SSH. A `base-create` guest must never start `sshd` or load a host private key. Repo staging and an optional credential-free warm command run through the guest agent channel. Warm input is part of the future snapshot contents: authenticated or secret-bearing warm-up is unsupported and must run after restore. The host forces `egress::Policy::None` before the VMM can transmit and retains it through seal and snapshot consumption. For `None`, enforcement covers both forwarded traffic and tap-originated host-local `INPUT`; base boot also disables IPv6 before the guest interface comes up, closing link-local bypass. A warm command therefore cannot ingest secrets from the rooms host, metadata, internal, public, or authenticated endpoints. Snapshot bases always use the read-only rootfs plus tmpfs-overlay path; there is no writable-rootfs opt-out. Ordinary `rooms run` behavior remains unchanged.

## Behavior

- Install a minimal guest provisioning/resume agent and order it before `sshd` in the rootfs.
- Make the snapshot-capable rootfs build omit `/etc/ssh/ssh_host_*` entirely. Before boot, have `base-create` inspect the supplied image and fail closed if it contains any baked SSH host private key or lacks the overlay entry point; deleting a key during quiesce is not an allowed fallback.
- Force `base-create` to boot that validated image as a read-only block device with `init=/sbin/overlay-init`. Warm writes live only in the captured tmpfs upper layer and never mutate the shared backing image.
- Give provisioning a dedicated vsock port and typed framing; do not overload the first-read-then-delete secrets port.
- Resolve `--repo` on the host. Create a git bundle without embedding host credentials, serve it with the optional warm command, and require phase ACKs after stage, clone, and warm.
- Extend `egress::Policy::None` to install both the existing tap-keyed `FORWARD` drop and a tap-keyed host-local `INPUT` drop. Install both on the base's pool TAP before the VMM can transmit, keep them throughout provisioning, warm-up, quiesce, and the later snapshot, and remove both idempotently during checked teardown. Failure to install either rule prevents boot. The guest receives repo input only through the host-served vsock bundle.
- Append `ipv6.disable=1` to the snapshot-base kernel command line before boot and verify IPv6 remains disabled before accepting the quiesced beacon. Do not rely on IPv4 `iptables` rules to cover link-local IPv6; failure to prove the disabled state leaves the candidate non-neutral.
- Execute warm-up with a fixed scrubbed environment and no credential files, forwarded host variables, secret channel, or network access. Treat the command bytes as snapshot-persistent input and refuse configured credential sources; callers needing credentials or network access warm after restore.
- In base mode, suppress `sshd`, verify there is no listener/session, stop non-essential services, and validate the retained process set structurally.
- Remove the staged bundle and command payload before sealing as filesystem hygiene only. Unlinking is not memory sanitization and does not justify `Neutral`; neutrality comes from the credential-free input and execution contract.
- Close every provisioning connection, emit one exact `quiesced` beacon on a separate one-shot endpoint, then enter the snapshot-safe poll/retry wait used by restore. Hold no connection across the snapshot.
- The host waits with a bounded timeout. Only the exact beacon permits `RoomMeta::seal` followed by atomic `room.json` persistence.
- A malformed beacon, timeout, agent error, unexpected process, or persistence error leaves the room non-neutral and tears the candidate down.
- A snapshot-capable base image contains no baked host keys. Restored rooms later generate a fresh overlay key before starting `sshd`.

## Acceptance

- `rooms base-create --repo ...` and a credential-free `--warm ...` complete with `provenance=neutral`; secret-bearing or authenticated warm-up is outside the command contract.
- Repo content is present, while no repository credential was delivered to the guest.
- Warm-up receives a fixed scrubbed environment and no host credential source; payload deletion is never treated as proof that secret bytes left memory.
- Neutral provisioning has enforced `Policy::None` across IPv4 `FORWARD` and host-local `INPUT`, with guest IPv6 disabled before interface setup and through seal; a warm command cannot reach the rooms host, metadata, internal, authenticated, or public network endpoints.
- No `sshd` process, listener, session child, or loaded host private key exists at seal time.
- The snapshot-capable image build contains no SSH host private key, and `base-create` refuses a supplied image containing one before it can reach `Neutral`.
- The backing rootfs is mounted read-only, its hash/mtime is unchanged by base creation and warm-up, and the guest root remains writable only through the tmpfs overlay.
- `Neutral` is persisted only after the exact beacon and after its connection closes.
- The retained agent waits without a live vsock connection and can reconnect after restore.
- Every failure branch tears down or remains `Provisioning`; none accidentally seals.
- Plain `rooms run` remains unchanged.

## Test plan

Run `make check`. Add protocol tests for framing and ACK order; refusal tests for malformed/late beacon, unexpected processes, configured credential sources, images without overlay-init, images containing baked SSH host private keys, failure to install either half of `Policy::None`, and failure to prove IPv6 disabled; persistence/cleanup tests; egress tests for tap-keyed `FORWARD` and host-local `INPUT` install/remove; boot-argument tests for base-only `ipv6.disable=1`; and rootfs-script tests proving the snapshot-capable build omits host keys, base mode suppresses `sshd`, scrubs the warm environment, and forces the read-only drive payload. Validate credential-free bundle transfer, blocked IPv4/IPv6 external and rooms-host-local network access during warm-up, retained process set, beacon, durable `Neutral`, writable overlay root, absent backing-image host keys, and unchanged backing-image hash/mtime on the rooms-host before landing.

## Non-goals

- Firecracker snapshot execution (`snapshot-create`).
- Restore and per-resume hygiene (`restore-single`).
- N-clone fan-out (`fork-clones`).
