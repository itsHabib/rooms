---
driver_version: 1
generated_at: 2026-07-29T18:05:00Z
generated_by: work-driver-prep
source:
  project: rooms
  phase: snapshot-restore
repo: rooms
repo_url: https://github.com/itsHabib/rooms
branch_prefix: snapshot-
default_runtime: local

batches:
  - id: 1
    label: "finish the security primitive after PRs #92 and #96"
    depends_on: []
    status: pending
    streams:
      - task_id: tsk_01KYGQWEGWMT8VS080BGG9BZ5Y
        task_slug: sealed-neutral-base
        spec_path: docs/features/snapshot-restore/sealed-neutral-base.md
        branch_name: snapshot-sealed-neutral-base-finish
        runtime: local
        model: opus
        effort: extra
        touches: [scripts/lib, scripts/build-rootfs-alpine.sh, src/vsock.rs, src/runner.rs, src/main.rs]
        status: pending
  - id: 2
    label: after batch 1 — execute the landed snapshot policy
    depends_on: [1]
    status: pending
    streams:
      - task_id: tsk_01KYGMJCQW6GT0GKTS3NNGJY2X
        task_slug: snapshot-create
        spec_path: docs/features/snapshot-restore/snapshot-create.md
        branch_name: snapshot-snapshot-create-exec
        runtime: local
        model: opus
        effort: extra
        touches: [src/firecracker.rs, src/main.rs, src/snapshot.rs, src/slot.rs]
        status: pending
  - id: 3
    label: after batch 2 — execute restore + hygiene and run the phase gate
    depends_on: [2]
    status: pending
    streams:
      - task_id: tsk_01KYGMJSHR5QGBSJ8B05F5N0ZG
        task_slug: restore-single
        spec_path: docs/features/snapshot-restore/restore-single.md
        branch_name: snapshot-restore-single-exec
        runtime: local
        model: opus
        effort: extra
        touches: [src/firecracker.rs, src/restore.rs, src/slot.rs, src/runner.rs, src/room.rs, src/main.rs]
        status: pending

conflict_notes:
  - kind: dep_signal
    from: restore-single
    to: snapshot-create
    reason: "restore consumes the snapshot artifact and reservation created by snapshot-create"
  - kind: dep_signal
    from: snapshot-create
    to: sealed-neutral-base
    reason: "snapshot refuses the base until the quiesced beacon persists Neutral"
  - kind: file_overlap
    file: src/main.rs
    tasks: [sealed-neutral-base, snapshot-create, restore-single]
    note: "all three add orchestration on the same CLI surface; the dependency chain serializes them"
  - kind: file_overlap
    file: src/firecracker.rs
    tasks: [snapshot-create, restore-single]
    note: "snapshot execution and restore process staging share the Firecracker mechanism layer"
---

# snapshot/fork P1 — remaining execution driver manifest

Refreshed by `/work-driver-prep project:rooms:phase:snapshot-restore` on 2026-07-29 after reconciling PRs #92–#96.
Consumed by `/work-driver docs/features/snapshot-restore/driver.md`.

The foundations have landed: provenance (#92), snapshot planning (#93), restore planning (#94),
reservation/lease (#95), and the provisioning base boot (#96). These batches contain only the
remaining mechanisms; do not redo the landed foundations.

This remains fully serial: sealing creates the security precondition, snapshot consumes it, and
restore consumes the resulting artifact and reservation.

## Batches

**Batch 1 — `sealed-neutral-base`**
- Replace SSH warm-up with host-bundled agent provisioning, verify quiescence over vsock, and
  persist `Provisioning → Neutral` only after the terminal beacon.

**Batch 2 — `snapshot-create`**
- Execute the existing snapshot plan against Firecracker, persist the protected artifact set,
  tear the template down, and transfer its slot to the snapshot reservation.

**Batch 3 — `restore-single`**
- Execute the fresh-process restore flow, enforce custody-before-resume, return leases
  persistently, require the resume hygiene ACK, and run the double-restore rooms-host gate.

## Runtime

All streams use the local runtime at opus/extra. The driver serializes them and fast-forwards
each new worktree to the prior batch's merge commit.
