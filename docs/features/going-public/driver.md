---
driver_version: 1
generated_at: 2026-05-27T14:45:00Z
generated_by: work-driver-prep
source:
  project: rooms
  phase: going-public
repo: rooms
repo_url: https://github.com/itsHabib/rooms
default_runtime: cloud

batches:
  - id: 1
    label: ready now — file-disjoint, fully parallel-safe (11 streams; gp-ci-dependabot dropped 2026-05-27 per operator)
    depends_on: []
    status: pending
    streams:
      - task_id: tsk_01KSMXCW5RFB7NXXZY8KWNMDQ2
        task_slug: gp-personal-path-in-spec-doc
        spec_path: docs/features/going-public/gp-personal-path-in-spec-doc.md
        runtime: cloud
        touches: [docs/features/poc-m2-1-hardening-followups/spec.md]
        status: pending
      - task_id: tsk_01KSMXD86ZG2P32WJFD98F9KY4
        task_slug: gp-gitignore-missing-secret-patterns
        spec_path: docs/features/going-public/gp-gitignore-missing-secret-patterns.md
        runtime: cloud
        touches: [.gitignore]
        status: pending
      - task_id: tsk_01KSMXDG38XGDTW31M9WC76HRD
        task_slug: gp-no-changelog
        spec_path: docs/features/going-public/gp-no-changelog.md
        runtime: cloud
        touches: [CHANGELOG.md]
        status: pending
      - task_id: tsk_01KSMXE7E7TKMAPTRMD5NBMC0K
        task_slug: gp-ci-coverage-workflow
        spec_path: docs/features/going-public/gp-ci-coverage-workflow.md
        runtime: cloud
        touches: [.github/workflows/coverage.yml]
        status: pending
      - task_id: tsk_01KSMXEDXV4VC2M3V8A1R12B3G
        task_slug: gp-ci-mutants-workflow
        spec_path: docs/features/going-public/gp-ci-mutants-workflow.md
        runtime: cloud
        touches: [.github/workflows/mutants.yml, .cargo/mutants.toml]
        status: pending
      - task_id: tsk_01KSMXF01G4WN7ATTE9QKVW1DZ
        task_slug: gp-docs-followups-file
        spec_path: docs/features/going-public/gp-docs-followups-file.md
        runtime: cloud
        touches: [docs/follow-ups.md, CLAUDE.md]
        status: pending
      - task_id: tsk_01KSMXF7CJ6RGJ890S2FXQ16WJ
        task_slug: gp-docs-index
        spec_path: docs/features/going-public/gp-docs-index.md
        runtime: cloud
        touches: [docs/README.md]
        status: pending
      - task_id: tsk_01KSMXFD7DSW48DA1D7V36Y8FN
        task_slug: gp-contributing-md
        spec_path: docs/features/going-public/gp-contributing-md.md
        runtime: cloud
        touches: [CONTRIBUTING.md]
        status: pending
      - task_id: tsk_01KSMXFHE8TCVF0H6A82880ZVC
        task_slug: gp-code-of-conduct-md
        spec_path: docs/features/going-public/gp-code-of-conduct-md.md
        runtime: cloud
        touches: [CODE_OF_CONDUCT.md]
        status: pending
      - task_id: tsk_01KSMXFT5SE6K5HT57YGR0E4EX
        task_slug: gp-security-md
        spec_path: docs/features/going-public/gp-security-md.md
        runtime: cloud
        touches: [SECURITY.md]
        status: pending
      - task_id: tsk_01KSMXFW4NGPS2HCZZ0V6PWXMX
        task_slug: gp-github-templates
        spec_path: docs/features/going-public/gp-github-templates.md
        runtime: cloud
        touches:
          - .github/ISSUE_TEMPLATE/bug_report.md
          - .github/ISSUE_TEMPLATE/feature_request.md
          - .github/PULL_REQUEST_TEMPLATE.md
        status: pending

  - id: 2
    label: ci.yml + README.md sub-region overlaps — parallel-safe with rebase
    depends_on: []
    status: pending
    streams:
      - task_id: tsk_01KSMXDMNTP3J7SDV23EFJNG33
        task_slug: gp-ci-cargo-audit
        spec_path: docs/features/going-public/gp-ci-cargo-audit.md
        runtime: cloud
        touches: [.github/workflows/ci.yml]
        status: pending
      - task_id: tsk_01KSMXE0H143PJXP89XHZ7VWJ1
        task_slug: gp-ci-os-matrix
        spec_path: docs/features/going-public/gp-ci-os-matrix.md
        runtime: cloud
        touches: [.github/workflows/ci.yml]
        status: pending
      - task_id: tsk_01KSMXEKQ0DSTVR13Q0PEB2NDQ
        task_slug: gp-readme-badges
        spec_path: docs/features/going-public/gp-readme-badges.md
        runtime: cloud
        touches: [README.md]
        status: pending
      - task_id: tsk_01KSMXETRJRKACM7Z96E4RH8VV
        task_slug: gp-readme-doctor-drift
        spec_path: docs/features/going-public/gp-readme-doctor-drift.md
        runtime: cloud
        touches: [README.md]
        status: pending

  - id: 3
    label: Cargo.toml [dev-dependencies] overlap — parallel-safe with rebase
    depends_on: []
    status: pending
    streams:
      - task_id: tsk_01KSMXG43H8CKPPH1SHTT9YYDD
        task_slug: gp-tests-proptest
        spec_path: docs/features/going-public/gp-tests-proptest.md
        runtime: cloud
        touches:
          - Cargo.toml
          - src/doctor.rs
          - src/artifacts.rs
          - src/firecracker.rs
        status: pending
      - task_id: tsk_01KSMXGCRJCMNHGR0Q61YV3PQD
        task_slug: gp-tests-cli-integration
        spec_path: docs/features/going-public/gp-tests-cli-integration.md
        runtime: cloud
        touches: [Cargo.toml, tests/cli.rs]
        status: pending

  - id: 4
    label: tag v0.1.0 (must follow every other gp-* landing)
    depends_on: [1, 2, 3]
    status: pending
    streams:
      - task_id: tsk_01KSMXD1T4BXZD8B745PMGE5T7
        task_slug: gp-no-git-tags
        spec_path: docs/features/going-public/gp-no-git-tags.md
        runtime: cloud
        touches: []
        status: pending

conflict_notes:
  - kind: file_overlap
    file: .github/workflows/ci.yml
    tasks: [gp-ci-cargo-audit (adds new top-level audit: job), gp-ci-os-matrix (modifies existing test: job to matrix)]
    note: |
      Sub-regions are disjoint (cargo-audit adds a NEW top-level job; os-matrix
      modifies the existing test: job). Parallel-safe; second-to-merge needs a
      one-minute rebase.

  - kind: file_overlap
    file: README.md
    tasks: [gp-readme-badges (insert at top, before elevator pitch), gp-readme-doctor-drift (edit one row in CLI surface table)]
    note: |
      Sub-regions are disjoint (badges sit above the elevator pitch; doctor row
      is in the CLI surface table further down). Parallel-safe; second-to-merge
      needs a small rebase.

  - kind: file_overlap
    file: Cargo.toml
    tasks: [gp-tests-proptest (adds proptest = "1"), gp-tests-cli-integration (adds assert_cmd = "2" + predicates = "3")]
    note: |
      Both add lines to [dev-dependencies] but the keys are different
      (proptest vs assert_cmd + predicates). Low rebase risk; second-to-merge
      may need to re-order alphabetically.

  - kind: dep_signal
    from: gp-no-git-tags
    to: gp-no-changelog
    reason: "gp-no-git-tags's release notes reference CHANGELOG.md; ideally CHANGELOG exists first so `--notes-file CHANGELOG.md` works."

  - kind: dep_signal
    from: gp-no-git-tags
    to: "*"
    reason: "Must be the final gp-* to ship. Enforced by batch 4's depends_on: [1, 2, 3]."

  - kind: dep_signal
    from: gp-readme-badges
    to: gp-ci-coverage-workflow
    reason: "Soft — coverage badge is deferred per gp-readme-badges Non-goals; only CI + license badges land now."

skipped_during_resolution: []
---

# Going public — driver manifest

Generated by `/work-driver-prep project:rooms:phase:going-public` on 2026-05-27.
Consumed by `/work-driver docs/features/going-public/driver.md`.

## Entry condition

Both in-flight bug-fix PRs (#11 `fix-doctor-tap-check`, #12 `fix-control-failures-test-isolation`) merged. main is clean.

## Exit condition

rooms is launch-ready:
- Personal path scrubbed from spec doc (P0)
- Standard env/secret patterns in `.gitignore` (P2)
- `CHANGELOG.md` exists with v0.1.0 section (P2)
- CI maturity: cargo-audit on PRs, dependabot weekly, OS matrix, coverage + mutants workflows
- README has CI + license badges; doctor row updated to reflect shipped state
- `docs/follow-ups.md` exists; `docs/README.md` indexes the docs
- Community files: `CONTRIBUTING.md`, `CODE_OF_CONDUCT.md`, `SECURITY.md`, `.github/ISSUE_TEMPLATE/` + `PULL_REQUEST_TEMPLATE.md`
- Test depth: proptest coverage on parsers + state machines, subprocess CLI integration tests
- `v0.1.0` tag + GitHub release anchor the first public-facing version

## Batches

### Batch 1 — ready now, 11 streams (parallel-safe, file-disjoint)

| # | task | runtime | touches | spec |
|---|---|---|---|---|
| 1 | gp-personal-path-in-spec-doc | cloud | docs/features/poc-m2-1-hardening-followups/spec.md | [spec](gp-personal-path-in-spec-doc.md) |
| 2 | gp-gitignore-missing-secret-patterns | cloud | .gitignore | [spec](gp-gitignore-missing-secret-patterns.md) |
| 3 | gp-no-changelog | cloud | CHANGELOG.md | [spec](gp-no-changelog.md) |
| 4 | gp-ci-coverage-workflow | cloud | .github/workflows/coverage.yml | [spec](gp-ci-coverage-workflow.md) |
| 5 | gp-ci-mutants-workflow | cloud | .github/workflows/mutants.yml + .cargo/mutants.toml | [spec](gp-ci-mutants-workflow.md) |
| 6 | gp-docs-followups-file | cloud | docs/follow-ups.md + CLAUDE.md | [spec](gp-docs-followups-file.md) |
| 7 | gp-docs-index | cloud | docs/README.md | [spec](gp-docs-index.md) |
| 8 | gp-contributing-md | cloud | CONTRIBUTING.md | [spec](gp-contributing-md.md) |
| 9 | gp-code-of-conduct-md | cloud | CODE_OF_CONDUCT.md | [spec](gp-code-of-conduct-md.md) |
| 10 | gp-security-md | cloud | SECURITY.md | [spec](gp-security-md.md) |
| 11 | gp-github-templates | cloud | .github/ISSUE_TEMPLATE/* + PULL_REQUEST_TEMPLATE.md | [spec](gp-github-templates.md) |

### Batch 2 — 4 streams (sub-region overlaps in ci.yml + README.md, parallel-safe with rebase)

| # | task | runtime | touches | spec |
|---|---|---|---|---|
| 13 | gp-ci-cargo-audit | cloud | .github/workflows/ci.yml (adds audit job) | [spec](gp-ci-cargo-audit.md) |
| 14 | gp-ci-os-matrix | cloud | .github/workflows/ci.yml (modifies test job) | [spec](gp-ci-os-matrix.md) |
| 15 | gp-readme-badges | cloud | README.md (top) | [spec](gp-readme-badges.md) |
| 16 | gp-readme-doctor-drift | cloud | README.md (CLI table row) | [spec](gp-readme-doctor-drift.md) |

### Batch 3 — 2 streams (Cargo.toml dev-deps overlap, parallel-safe with rebase)

| # | task | runtime | touches | spec |
|---|---|---|---|---|
| 17 | gp-tests-proptest | cloud | Cargo.toml + src/doctor.rs + src/artifacts.rs + src/firecracker.rs | [spec](gp-tests-proptest.md) |
| 18 | gp-tests-cli-integration | cloud | Cargo.toml + tests/cli.rs | [spec](gp-tests-cli-integration.md) |

### Batch 4 — 1 stream (sequential, must follow batches 1-3)

| # | task | runtime | touches | spec |
|---|---|---|---|---|
| 19 | gp-no-git-tags | cloud | (none — `git tag` + `gh release create`) | [spec](gp-no-git-tags.md) |

## Runtime-suggestion rationale

All 19 streams suggested **cloud** per:
- Operator's `feedback_cloud_ship_defaults` memory (locked 2026-05-25): cloud is canonical for rooms ship.ship work.
- None of the 19 tasks need rooms-host KVM access, local-only env vars, or multi-repo changes — all are cloud-safe.

Per-stream override if needed via the manifest's per-stream `runtime` field.

## Invocations

Run the whole manifest in dep order (recommended):

```
/work-driver docs/features/going-public/driver.md
```

Or batch-by-batch, operator-paced:

```
/work-driver docs/features/going-public/driver.md --batch 1
/work-driver docs/features/going-public/driver.md --batch 2
/work-driver docs/features/going-public/driver.md --batch 3
/work-driver docs/features/going-public/driver.md --batch 4
```

## Status (updated by /work-driver as the manifest runs)

All four batches: pending.
