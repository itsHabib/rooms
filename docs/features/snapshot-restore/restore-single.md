**Status**: ready — remaining execution slice after PRs #94 and #95
**Owner**: @mh
**Date**: 2026-07-29
**Model/effort**: opus / extra — load-before-resume, custody ordering, and the hygiene gate are load-bearing.
**Related**: dossier task `restore-single` (id: `tsk_01KYGMJSHR5QGBSJ8B05F5N0ZG`), `docs/features/snapshot-fork-replay/spec.md` D4/D7

# Wire `rooms restore`: fresh Firecracker load, custody-before-resume, and hygiene ACK

## Scope

| Bucket | Files | Est. LOC | Weighted |
|---|---|---|---|
| Production | `src/firecracker.rs`, `src/main.rs`, `src/runner.rs`, guest resume-agent protocol, existing `src/restore.rs` and `src/slot.rs` integration | ~420 | 420 |
| Tests | live-flow mechanism, custody ordering, lease round-trip, ACK failure matrix | ~240 | 120 |
| **Total** | | | **~540** |

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
- Lease the snapshot reservation for the new room. A busy or foreign slot fails closed with no fallback.
- Reuse jail/guard/staging mechanisms, but preserve restore's distinct order.
- Bind-mount `snapshot.mem` into the jail so later clones share the inode privately; the small vmstate file may be copied.
- Install witness and egress controls before `Resumed`.
- Execute the landed plan exactly: load snapshot first, resume, then serve the resume nudge and await the guest ACK.
- The guest nudge reseeds randomness, corrects the clock, assigns the new room/run identity, generates a fresh SSH host key in the overlay, starts `sshd`, and ACKs only after all steps succeed.
- No active vsock connection is assumed to survive the snapshot. The agent reconnects with bounded poll/retry and read deadlines.
- Re-probe SSH only after the ACK. Persist additive `snapshot_lineage` before reporting readiness.
- Every failure after lease acquisition tears down the process/jail and calls `release_lease`, returning ownership to the snapshot reservation. Preserve the primary error if cleanup also fails.

## Acceptance

- A compatible snapshot restores on the same slot/IP and reaches SSH through a freshly keyed server.
- Custody and snapshot load both precede `Resumed`; no network transmit gap exists.
- `snapshot.mem` is bind-mounted, not copied.
- No hygiene ACK means no SSH/workload readiness.
- Compatibility mismatch loads nothing.
- Restore → teardown → restore again succeeds; the reservation persists and the slot is never walk-claimable.
- Two restores have distinct RNG output, corrected clocks, fresh host keys, and distinct room identity.

## Test plan

Run `make check`. Unit tests assert ordering, bind-mount behavior, compatibility refusal, lease return on every failure, and the ACK gate. On the rooms-host: snapshot one sealed base, restore it twice, force one compatibility mismatch, and prove distinct RNG, clock, key, identity, and custody-before-resume.

## Non-goals

- N-clone fan-out/netns (`fork-clones`).
- The P2 killer-demo gate.
