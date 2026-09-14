# Reusable Nix environments for repository checks

Status: first slice implemented and exercised on the local Lima host (see
"As implemented" and "Measured result"). This extends the existing
neutral-base/snapshot/restore path; it is not a new runtime.

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

## As implemented

The contract review on #123 found gaps between this contract and the landed
code. The implementation closes them as follows.

- **Surface.** `base-create` gains `--toolstore DIR`, `--cpus`, `--memory` (same
  limits and defaults as `run`; `--disk` stays refused for bases) and
  `--base-sha REV` (requires `--repo`). `restore`, `clone` and `matrix` gain
  `--toolstore DIR`. A snapshot fixes its machine shape, so a restored room has
  the vCPU/RAM its base booted with.
- **Admission record.** `base-create` opens the toolstore with the cold-run
  validator and records its digest in the base's `room.json`
  (`toolstore_sha256`). Snapshot creation opens the base jail's
  `toolstore.sqfs` bind, requires the immutable seal, and hashes the whole held
  inode with the same before/after identity check used for the rootfs. It
  refuses before pausing unless that hash equals the recorded digest. An
  attachment without a record, or a record without an attachment, is refused.
- **Schema.** A snapshot with a toolstore is written as schema v2 with
  `toolstore_sha256`; a snapshot without one stays the exact v1 shape. A
  pre-toolstore build refuses v2 as an unsupported schema during preparation.
  This build accepts only v1 without a toolstore and v2 with one, and refuses a
  missing, extra or different toolstore (and any other schema/field
  combination) in the pure policy. Both refusals come before any restore
  intent, process or lease; `clone`/`matrix` allocate their host networks
  concurrently with preparation and drop them on refusal.
- **Mixed binaries.** `room.json` has no version gate older builds enforce, so a
  toolstore base must be snapshotted by a toolstore-aware build. A
  pre-toolstore `rooms snapshot` ignores the recorded digest and publishes a v1
  snapshot whose restore fails inside Firecracker's load, after the restore
  claims its lease (cleanup still runs).
- **Device identity.** Bases are always read-only and never have scratch, so a
  toolstore is always the second drive at the fixed jail path
  `/toolstore.sqfs`. Metadata records only the digest; it never names a path.
- **Restore staging.** `prepare_restore` opens the supplied toolstore once
  (full hash) and holds the descriptor; `clone`/`matrix` share it across the
  batch. The restore jail binds that descriptor at `/toolstore.sqfs` after the
  memory bind; staging rollback and teardown unmount it. The staged bind is
  checked for device/inode identity before load, and the seal is rechecked at
  each existing revalidation boundary. The restored `room.json` and the
  restore/clone records carry the digest.
- **Revision pinning.** With `--base-sha`, the host resolves the revision to a
  full commit and builds a bundle holding only that commit's history behind a
  detached `HEAD` (via a shared bare clone, so the caller's repository is never
  modified). The guest's plain clone therefore checks out exactly that commit.
  The commit is recorded in `room.json` (`base_repo_sha`), then in the snapshot
  and the restore records. The restored task still checks `git rev-parse HEAD`
  before applying its patch.
- **PATH.** Restored SSH sessions start sshd from the canonical resume config,
  which has no `SetEnv PATH`, and the neutral warm command runs with a scrubbed
  `PATH`. Warm and restored commands therefore name `/nix/var/rooms/env/bin`
  explicitly, which also puts the toolchain path in the command receipt.
  Adding it to the resume config is an image change and is out of scope here.
- **Phase timings.** `restore` and `clone` have no lifecycle stream, so restored
  readiness, execution, collection and cleanup come from host log timestamps
  plus `result.json`.
- **Writable-state asymmetry.** A cold room with `--disk` writes to a private
  ext4 disk; a restored room writes to the tmpfs overlay frozen in snapshot
  memory (half of guest RAM).

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

## Measured result

On 2026-09-14 the local nested aarch64 Lima host ran the Workbench fixture
(`92a706a`, patch `83da8be4…`, sealed Python toolstore `85074bed…`) through
five cold controls, two toolstore bases (one snapshot on disk, one on a
`huge=always` tmpfs), ten restores and four clone batches (n=2 and n=4). All 27
returned patches were byte-identical and produced tree `8e0ed976…`; 16 of 16
tests passed everywhere. No restored room saw another's sentinel, each had a
distinct sshd host key, and restores without the toolstore or with a different
sealed one were refused before admission with no residue.

In interleaved rounds under the same contention, cold took 18.2 s end to end
and restore 15.8 s (disk snapshot) or 16.4 s (tmpfs snapshot). Restore reached
a ready, pinned repository in 1.9 s instead of 11.4 s, but the tests ran in
10 s instead of 3 s: after a restore, each first write to a guest page is a
nested stage-2 fault on this host. The hugepage snapshot helped readiness, not
the write-heavy tests. Concurrent clones gave no throughput gain here. These
numbers are specific to one nested host with shared CPUs and say nothing about
bare-metal KVM. Evidence, per-phase timings and the orchestration:
`~/Documents/Codex/2026-09-14/rooms-snapshot/RESULTS.md` (summarized in #123).
