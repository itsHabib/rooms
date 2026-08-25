# Snapshot matrix — distinct-case fan-out

**Status:** implemented for review

**Owner:** agent:codex

**Date:** 2026-08-25

**Related:** [`../snapshot-fork-replay/spec.md`](../snapshot-fork-replay/spec.md),
[`../../product-directions.md`](../../product-directions.md) §3 Counterfactual CI

## Problem

`rooms clone -n N --command ...` proves one command can run across an isolated
snapshot fleet, but it broadcasts that command. E2E and counterfactual tests
need the adjacent primitive: start every case from the same immutable snapshot,
run a different command in each clone, and bind every result to the exact case
policy that selected it.

The first motivating consumer is RoxIQ's release gauntlet. Its clean confidence
control and a frozen browser mutant should start from equivalent state and
produce independently attributable evidence. Rooms must not learn RoxIQ's
Confidence schema, Playwright policy, deployment rules, or repair authority.

## Contract

```sh
rooms matrix <snapshot-dir> \
  --image <rootfs.ext4> \
  --cases <matrix.json> \
  --out <evidence-dir> \
  [--witness] [--egress <policy>] [--max-wall <duration>] [--json]
```

The strict manifest is deliberately small:

```json
{
  "schema": "rooms.matrix.v1",
  "cases": [
    {"id": "clean", "command": "./test-clean.sh"},
    {"id": "browser-mutant", "command": "./test-mutant-control.sh"}
  ]
}
```

- One to eight cases, matching the bounded clone fleet.
- Case order is significant and maps to allocation ordinal, independent of
  task completion order.
- IDs are unique path-safe slugs matching
  `[a-z0-9][a-z0-9_-]{0,63}`.
- Unknown fields, trailing JSON, empty/NUL commands, oversized commands, and
  oversized manifests fail before snapshot or network effects.
- The manifest is read once. Execution uses owned immutable values and never
  rereads the source path.

Each case receives one normal clone lifecycle: distinct netns/veth/NAT identity,
pre-resume egress and witness custody, hygiene ACK, command execution, artifact
collection, and exact teardown. Output is partitioned under
`<evidence-dir>/<case-id>`.

## Evidence

`--json` emits `rooms.matrix.result.v1`. The envelope carries:

- a terminal `status` and `clones` array on completed, failed, and cancelled
  outcomes once a valid manifest digest exists;
- a semantic SHA-256 of the strict manifest, encoded as
  `sha256:<lowercase-hex>`;
- declared case ID and command SHA-256 using the same encoding;
- snapshot, room, frozen slot, namespace, veth, and guest identity;
- terminal status, exit code, and case output directory.

The semantic digest ignores JSON whitespace but preserves case order and exact
command bytes. Successful, failed, and cancelled records retain case identity
and command digest where the lifecycle reached that assignment. On a partial
restore failure, successfully restored siblings are reported as `aborted`
before teardown and the rejected members remain structured failure records. A
manifest admission error cannot carry a trusted manifest digest and therefore
uses the ordinary pre-admission error record.

For matrix cases, primary `/workspace/out` collection and requested witness
persistence are part of the terminal result, not best-effort decoration. If
collection or teardown fails after the workload exits, the completed case
record is retained under `clones` and the infrastructure fault is reported
separately under `failures`; the matrix envelope is `failed`.

Rooms does **not** decide whether a product observation is acceptable. A case
command returns the consumer's mechanical verdict, or the consumer evaluates
the emitted result. A negative control should therefore return zero only when
the intended failure is observed—for example, `! ./confidence-browser.sh`.

## Authority boundary

- No production URL, Stripe mode, database role, or deployment credential is
  implied by `matrix`.
- `--secret NAME` retains the existing named-secret admission and delivery
  boundary. Secret values do not enter the manifest or result.
- Rooms cannot deploy, publish, repair, merge, or weaken the consumer's oracle.
- RoxIQ Confidence remains RoxIQ's product judge; a matrix result proves only
  isolated execution and attributable observations.

## Acceptance

- Strict loader tests cover whitespace-independent semantic identity, declared
  ordering, unknown/trailing input, invalid/duplicate IDs, empty commands,
  schema mismatch, and case bounds.
- CLI tests prove `matrix` derives its fleet size from the manifest and requires
  an evidence root rather than a broadcast count.
- Output-path tests prove matrix cases use the validated case ID, while ordinary
  clone output remains room-ID partitioned.
- Result tests bind manifest, case, command, room, and output identities.
- Failure tests pin the uniform terminal envelope, partial-restore evidence,
  cancellation case retention, completed-observation retention across
  collection/teardown failures, and NUL-command rejection.
- Existing `clone` JSON and CLI behavior remain backward compatible.
- `make check` passes. Privileged acceptance uses a neutral snapshot and
  [`examples/matrix/positive-mutant.json`](../../../examples/matrix/positive-mutant.json)
  to prove both commands run in distinct clones and leave distinct artifacts.
  The retained result is indexed in
  [`snapshot-matrix-2026-08-25.md`](../../experiments/snapshot-matrix-2026-08-25.md).

## Next experiment

Use a RoxIQ snapshot warmed through its local disposable fixture setup. Run one
clean full-confidence case and one frozen browser-mutant negative control. Kill
the idea if the matrix cannot prove identical snapshot lineage, distinct case
commands/artifacts, expected control behavior, and clean teardown in one retained
record. If it wins, add a consumer-side reducer over controlled inputs; do not
move RoxIQ's oracle or authorization policy into Rooms.

## Non-goals

- No expected-exit or product-verdict policy in Rooms.
- No automatic mutant injection, repair agent, deployment, or PR publication.
- No claim of deterministic replay for clock, entropy, scheduling, or live
  network responses.
- No generalized scheduler, service mesh, web preview, or cross-host control.
