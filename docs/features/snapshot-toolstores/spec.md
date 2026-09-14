# Reusable Nix environments for repository checks

Status: implementation proposal, based on the reviewed cold-toolstore tree in #121.
This extends the existing neutral-base/snapshot/restore path; it is not a new runtime.

## Useful result

Prepare a credential-free Python environment once. Restore it into independent
rooms that apply a real Workbench patch and execute its semantic tests. Return the
patch, test output and execution/cleanup evidence. Compare useful end-to-end time
against the same task in a cold Nix room before claiming an improvement.

The headless integration lead owns the author, verifier, frozen patch and recovery
workflow. Rooms owns the VM/device boundary and measurements. The first fixture is
Workbench at `92a706a7982a527ade967e43b68afd4bc1e5d667`, with the independently
verified report-builder patch and Python unittest discovery under
`cmd/fleet/examples/headless/task`. Its patch hash must be supplied, not invented.

## Existing boundary and missing work

- `firecracker::BootRequest` accepts a verified `Toolstore`, but rejects it for
  base creation. Cold boot binds its open inode at `/toolstore.sqfs` and attaches
  the read-only device. Existing cleanup includes that bind.
- `SnapshotMeta` pins the rootfs and Firecracker version, but no toolstore.
  Snapshot creation must identify every backing device it depends on; a vmstate
  filename is not evidence that the backing bytes match.
- Restore stages rootfs, memory and vmstate before loading. It does not recreate
  the toolstore path referenced by a snapshot. The guest's already-mounted `/nix`
  cannot be repaired by merely changing PATH after restore.
- Neutral base creation supports host-delivered repository content and a scrubbed,
  offline warm command. Its CLI currently resolves repository HEAD; the experiment
  needs a host-recorded exact revision and refusal on mismatch.

## First implementation contract

Add optional `--toolstore DIR` to base creation and snapshot restore. Retain the
existing cold-run validator: architecture, full content hash, immutable source,
held descriptor, verified bind identity and compatible rootfs. Reuse that mechanism
instead of accepting a caller-supplied digest as an attestation.

Persist an optional toolstore descriptor in snapshot metadata: digest and the
fixed device identity required by this supported machine shape. Host-local source
paths are caller inputs, never authority to mount arbitrary metadata paths. Capture
the actual base attachment at snapshot creation and bind it into existing snapshot
integrity/attestation checks. Restore rejects an absent, extra or mismatched
toolstore before claiming execution resources. Legacy snapshots without a
toolstore remain the existing machine shape.

Before snapshot load, stage the verified toolstore at the exact jail-visible path
captured by the device. Ownership of the descriptor, mount and cleanup guard must
survive cancellation and cover the entire restored VMM lifetime. Only per-room
binds are removed; a shared toolstore is never deleted or unsealed by a run.

Start with the existing tmpfs writable overlay captured in snapshot memory.
Each restored room gets its own mutable memory/overlay state, while the Nix disk
remains read-only. Do not add writable scratch-disk snapshotting in this change:
that requires a separate coherent disk/memory checkpoint and per-clone disk-copy
contract. The Python workload fits the simpler existing overlay mechanism.

Keep the existing neutral-base hygiene: no task credentials, offline warm-up,
quiesced channels, fresh identity/RNG/clock/SSH state on resume. Warm Python imports
and the repository test prerequisites, not the author agent or its secrets.

Pin the repository at base preparation and record the resolved commit. Apply the
frozen patch only after restore. The restored task must reject a different base
commit and must not inherit a previous attempt's patch or artifacts.

Restore currently executes literal commands, not the cold repository runner.
The example therefore explicitly runs in `/workspace/repo`, applies the bounded
patch, runs unittest discovery, and writes its patch under `/workspace/out`.
Keep task exit distinct from export/collection/cleanup failures. Do not silently
promise cold-run flags or artifact behavior on restore where they are absent.

## Verification that decides usefulness

1. Cold Nix control: the exact frozen base/patch runs its meaningful tests and
   returns the expected edit, complete output and cleanup evidence.
2. Prepare and snapshot once; restore twice sequentially. Both run the same tests
   with equivalent results. A sentinel written by the first is absent in the
   second, and snapshot/toolstore inputs retain their hashes.
3. Exercise the existing clone path only after attachment propagation is correct
   across that shared restore mechanism. Two siblings must have distinct identity,
   independent writable state and separate result paths.
4. Reject missing/replaced/mutated/wrong-architecture toolstores before admission;
   test cancellation during staging, failed snapshot load, workload failure and
   cleanup. No residual VMM, lease, bind or jail may be represented as success.
5. Compare preparation cost, cold and restored end-to-end task latency, readiness,
   execution, collection and cleanup separately. Then sweep concurrency 1/2/4/8
   only within measured host capacity. Report completed verified tasks per minute,
   tail latency and process PSS with host-cache limitations explicitly stated.

Use the existing local Linux/KVM host in a coordinated resource window. Record
the candidate revision/binary, rootfs, toolstore, snapshot, repository and patch
identities with the attempt results. No new cloud resources or installation is
required by this milestone. Retained cloud results are prior evidence, not a
benchmark of this implementation. Snapshot restore being fast at the VMM layer
does not imply the guest is ready or the useful task is complete.

## Delivery order

Land #121 first. Review this attachment/identity contract, implement it in this
isolated branch, and exercise a single restored real task before extending the
same path to concurrent clones. The headless workflow remains owned by its existing
lead. No KV service, agent messaging protocol, scheduler or cross-host migration
is needed to establish this result.
