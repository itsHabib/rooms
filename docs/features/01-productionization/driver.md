---
driver_version: 1
generated_at: 2026-05-23T22:15:00Z
generated_by: work-driver-prep
source:
  project: rooms
  phase: 01-productionization
repo: rooms
repo_url: https://github.com/itsHabib/rooms
branch_prefix: prod-
default_runtime: local

batches:
  - id: 1
    label: ready now (no upstream deps)
    depends_on: []
    status: done
    completed_at: 2026-05-25T04:24:00Z
    streams:
      - task_id: tsk_01KSBE3JAWA0THZXWV6KBSBKTT
        task_slug: ci-and-claude-workflow
        spec_path: docs/features/ci-and-claude-workflow/spec.md
        branch_name: prod-ci-and-claude-workflow
        runtime: cloud
        touches: [.github/workflows/ci.yml, .github/workflows/claude.yml, README.md]
        status: superseded
        pr_number: 5
        closed_at: 2026-05-25T03:20:00Z
        note: |
          Workflow YAMLs already existed on main from earlier POC work; the
          PR's only contribution was a README CI paragraph that was made
          redundant by docs-vision-and-readme's new `## CI` section. Closed
          rather than merged.
      - task_id: tsk_01KSBE3RNJKH9HF7YP3HSW61X4
        task_slug: harden-firecracker-control
        spec_path: docs/features/harden-firecracker-control/spec.md
        branch_name: prod-harden-firecracker-control
        runtime: local
        touches:
          [
            src/firecracker.rs,
            src/main.rs,
            src/runner.rs,
            src/transport.rs,
            src/rootfs.rs,
            tests/control_failures.rs,
          ]
        status: done
        pr_number: 8
        merge_commit: 46c96302
        merged_at: 2026-05-25T04:21:00Z
        followup_pr: 10
        followup_merge_commit: 7efbb5fc
        followup_merged_at: 2026-05-26T03:49:24Z
        note: |
          Required a manual three-way merge against PR #7's runner-contract
          changes (both touched lib.rs / main.rs / runner.rs). Took #7's
          runner.rs (EXIT= marker + GuestExecOutcome) as canonical, layered
          harden's net-new modules (config, doctor, error, transport) and
          firecracker.rs/main.rs changes on top.

          Follow-up PR #10 (merge_commit 7efbb5fc, merged 2026-05-26T03:49Z):
          the three-way merge inadvertently dropped #8's structured
          `wait_for_ssh(&config) -> Result<(), FirecrackerError>` contract.
          The regression was Linux-only because the integration test in
          tests/control_failures.rs is gated on `cfg(unix, feature = "e2e")`,
          so Windows `make check` stayed green. Surfaced via the cfg(unix)
          unused-imports clippy error on the next CI run and restored in
          three squashed commits.
      - task_id: tsk_01KSBE3Z0WDF397EDMMP1N2FWX
        task_slug: runner-contract
        spec_path: docs/features/runner-contract/spec.md
        branch_name: prod-runner-contract
        runtime: cloud
        touches: [src/artifacts.rs, src/runner.rs, docs/runner-contract.md]
        status: done
        pr_number: 7
        merge_commit: 4a354e73
        merged_at: 2026-05-25T04:17:00Z
      - task_id: tsk_01KSBE4PGS5THA41HF4EMWE8HW
        task_slug: rootfs-builder
        spec_path: docs/features/rootfs-builder/spec.md
        branch_name: prod-rootfs-builder
        runtime: local
        touches:
          [
            scripts/build-rootfs.sh,
            scripts/lib/rootfs-helpers.sh,
            scripts/test-rootfs.sh,
            scripts/README.md,
            README.md,
            images/.gitignore,
          ]
        status: done
        pr_number: 9
        merge_commit: b55a92bb
        merged_at: 2026-05-25T04:15:00Z
      - task_id: tsk_01KSBE572WQ1VJ3E471BVXX202
        task_slug: docs-vision-and-readme
        spec_path: docs/features/docs-vision-and-readme/spec.md
        branch_name: prod-docs-vision-and-readme
        runtime: cloud
        touches: [docs/vision.md, README.md, CLAUDE.md]
        status: done
        pr_number: 6
        merge_commit: 573c4e25
        merged_at: 2026-05-25T04:11:00Z

  - id: 2
    label: after batch 1 (deps on runner-contract and rootfs-builder)
    depends_on: [1]
    status: pending
    streams:
      - task_id: tsk_01KSBE46THH7TXHNKBP49X9AG3
        task_slug: cursor-sdk-runner
        spec_path: docs/features/cursor-sdk-runner/spec.md
        branch_name: prod-cursor-sdk-runner
        runtime: local
        touches:
          [
            scripts/rootfs/cursor-runner.js,
            scripts/rootfs/install-cursor.sh,
            scripts/build-rootfs.sh,
            src/runner.rs,
            src/main.rs,
            tests/cursor_runner_e2e.rs,
          ]
        status: pending
        depends_on_tasks: [tsk_01KSBE3Z0WDF397EDMMP1N2FWX, tsk_01KSBE4PGS5THA41HF4EMWE8HW]
      - task_id: tsk_01KSBE4ZP40VZ1D69RNZMFRRGA
        task_slug: nix-flake-input
        spec_path: docs/features/nix-flake-input/spec.md
        branch_name: prod-nix-flake-input
        runtime: local
        touches:
          [
            src/rootfs.rs,
            src/main.rs,
            src/domain.rs,
            profiles/node-dev/flake.nix,
            profiles/node-dev/flake.lock,
            profiles/node-dev/README.md,
            docs/flakes.md,
            README.md,
          ]
        status: pending
        depends_on_tasks: [tsk_01KSBE4PGS5THA41HF4EMWE8HW]

  - id: 3
    label: after batch 2 (cross-repo; lives in pers/ship)
    depends_on: [2]
    status: pending
    streams:
      - task_id: tsk_01KSBE4EWNZJ69GGGSYK7VKFRK
        task_slug: ship-rooms-backend
        spec_path: ../../../ship/docs/features/rooms-backend/spec.md  # cross-repo
        repo_override: ship                                            # ship's git repo, not rooms
        branch_name: rooms-backend
        runtime: local
        touches:
          [
            packages/cursor-runner/src/room-runner.ts,
            packages/cursor-runner/src/runner.ts,
            packages/cursor-runner/src/index.ts,
            packages/cursor-runner/src/fake.ts,
            packages/workflow/src/...,
            packages/mcp-server/src/...,
            CLAUDE.md,
          ]
        status: pending
        depends_on_tasks: [tsk_01KSBE46THH7TXHNKBP49X9AG3]

conflict_notes:
  - kind: file_overlap
    file: src/runner.rs
    tasks:
      [harden-firecracker-control (production-light, lifecycle hooks ~30 LOC),
       runner-contract (light touch for Runner enum integration ~20 LOC),
       cursor-sdk-runner (Runner::Cursor variant ~40 LOC; batch 2, sequenced after both)]
    note: |
      Two batch-1 streams touch src/runner.rs in non-overlapping regions:
      - harden-firecracker-control wires the new RoomGuard + error enum into existing lifecycle calls
      - runner-contract adds the RunnerArtifacts integration with collect logic
      Production-light + small; rebase risk is low. Default behavior is still
      parallel-safe in batch 1; if the second to merge needs a rebase it's
      a 5-minute conflict.

  - kind: file_overlap
    file: src/main.rs
    tasks: [harden-firecracker-control (doctor rewrite), nix-flake-input (CLI --flake arg, batch 2)]
    note: |
      Batch-separated by dep ordering — harden-firecracker-control merges in
      batch 1, nix-flake-input rebases against it in batch 2. No actual collision.

  - kind: file_overlap
    file: README.md
    tasks:
      [ci-and-claude-workflow (one-paragraph CI note),
       rootfs-builder (Building the rootfs subsection),
       docs-vision-and-readme (full README structure),
       nix-flake-input (Profile section, batch 2)]
    note: |
      All three batch-1 streams touch README. docs-vision-and-readme owns the
      structure; the other two add specific subsections. Suggested order at
      merge: docs-vision-and-readme first (claim the structure), then
      ci-and-claude-workflow and rootfs-builder rebase to add their sections.

  - kind: dep_signal
    from: cursor-sdk-runner
    to: runner-contract
    reason: "spec body: 'Tested end-to-end with a trivial task'; needs result.json schema from runner-contract"
  - kind: dep_signal
    from: cursor-sdk-runner
    to: rootfs-builder
    reason: "spec body: 'Rootfs builder includes node, @cursor/sdk'; soft dep — rootfs-builder must exist"
  - kind: dep_signal
    from: ship-rooms-backend
    to: cursor-sdk-runner
    reason: "spec body: depends on cursor SDK runner being callable inside the microVM"
  - kind: dep_signal
    from: nix-flake-input
    to: rootfs-builder
    reason: "spec body: 'need the debootstrap path as the reference for what the flake must reproduce'"

skipped_during_resolution: []
---

# v0 productionization driver manifest

Generated by `/work-driver-prep phase rooms/01-productionization` on 2026-05-23.
Consumed by `/work-driver docs/features/01-productionization/driver.md`.

## Entry condition

POC upper bar met: `rooms run --repo <path> --task <task.md>` produces a microVM boot + repo at `/workspace/repo` + `claude -p` run + `result.patch` returned, end-to-end.

Do NOT fan this manifest out until that's true.

## Exit condition

v0 is shippable:
- Lints enforced in CI; `make check` matrix green
- Error handling on every Firecracker control path; `rooms doctor` runs real checks
- Runner contract documented + Rust types match + collect validates
- Cursor SDK runner working inside the microVM
- Ship can select `runtime: "rooms"` (the spec's ED-1 refines the original task's `backend` wording — see `pers/ship/docs/features/rooms-backend/spec.md`)
- Rootfs reproducibly buildable via `debootstrap` script
- Nix flake input works; reference flake at `profiles/node-dev/flake.nix` boots a working room
- `docs/vision.md` + `README.md` + `CLAUDE.md` exist and are accurate

## Batches

### Batch 1 — ready now, 5 streams (parallel-safe)

| # | task | runtime | branch | spec |
|---|---|---|---|---|
| 1 | ci-and-claude-workflow | cloud | `prod-ci-and-claude-workflow` | `docs/features/ci-and-claude-workflow/spec.md` |
| 2 | harden-firecracker-control | local | `prod-harden-firecracker-control` | `docs/features/harden-firecracker-control/spec.md` |
| 3 | runner-contract | cloud | `prod-runner-contract` | `docs/features/runner-contract/spec.md` |
| 6 | rootfs-builder | local | `prod-rootfs-builder` | `docs/features/rootfs-builder/spec.md` |
| 8 | docs-vision-and-readme | cloud | `prod-docs-vision-and-readme` | `docs/features/docs-vision-and-readme/spec.md` |

**Sub-region conflicts to watch** (see `conflict_notes` in frontmatter):
- `src/runner.rs` touched by #2 and #3 in non-overlapping regions (production lifecycle vs Runner enum integration). Low rebase risk.
- `README.md` touched by #1, #6, #8. Merge #8 first (owns structure), rebase the others.

### Batch 2 — after batch 1, 2 streams (parallel-safe)

| # | task | runtime | branch | spec |
|---|---|---|---|---|
| 4 | cursor-sdk-runner | local | `prod-cursor-sdk-runner` | `docs/features/cursor-sdk-runner/spec.md` |
| 7 | nix-flake-input | local | `prod-nix-flake-input` | `docs/features/nix-flake-input/spec.md` |

**Note:** #7 may split into two PRs per its spec (flake input plumbing + reference flake) if it exceeds the ideal band.

### Batch 3 — after batch 2, 1 stream (cross-repo)

| # | task | runtime | branch | spec | repo |
|---|---|---|---|---|---|
| 5 | ship-rooms-backend | local | `rooms-backend` | `pers/ship/docs/features/rooms-backend/spec.md` | `ship` |

Lives in `pers/ship`. Worktree creation runs against the ship repo, not rooms.

## Runtime-suggestion rationale

- **Cloud-suggested**: pure-docs / pure-Rust-types tasks (#1, #3, #8) — no Firecracker, no infra, parallelize well in cloud cursor.
- **Local-suggested**: anything touching Firecracker / rootfs / debootstrap / Nix (#2, #4, #6, #7) — needs the `rooms-host` Ubuntu VM with `/dev/kvm`. Cross-repo (#5) is also local because the ship integration smoke needs a real rooms-host.

Operator can override per stream when they know better.

## Invocations

Run the whole manifest in dep order (recommended):

```
/work-driver docs/features/01-productionization/driver.md
```

Or batch-by-batch, operator-paced:

```
/work-driver docs/features/01-productionization/driver.md --batch 1
/work-driver docs/features/01-productionization/driver.md --batch 2
/work-driver docs/features/01-productionization/driver.md --batch 3
```

## Status (updated by /work-driver as the manifest runs)

**Batch 1: done 2026-05-25** — 4 PRs merged (#6, #7, #8, #9); PR #5 closed as superseded by #6's README structure. 4 review cycles per PR. PR #8 required a manual three-way merge against PR #7's runner-contract restructure (both touched the same files in non-overlapping but intricately tangled regions).

**Follow-up: PR #10 merged 2026-05-26** — CI hotfix that restored #8's structured `wait_for_ssh(&config) -> Result<(), FirecrackerError>` contract dropped during the three-way merge. Regression was Linux-only (the failure-injection tests are `cfg(unix, feature = "e2e")`), so Windows `make check` missed it; surfaced on first CI run after merge.

Batches 2 and 3: pending.
