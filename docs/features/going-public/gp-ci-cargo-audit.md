**Status**: draft
**Owner**: @michael (human:mh)
**Date**: 2026-05-27
**Related**: dossier task `gp-ci-cargo-audit` (id: `tsk_01KSMXDMNTP3J7SDV23EFJNG33`), [going-public driver](driver.md)

# CI: add cargo-audit job for RustSec advisory coverage

## Scope

| Bucket | Files | Est. LOC | Weighted |
|---|---|---|---|
| CI | `.github/workflows/ci.yml` (modify — add new job) | ~10 | 10 |
| **Total weighted** | | | **~10** |

Band: **amazing**.

## Goal

Block PRs that introduce dependencies with unaddressed RustSec advisories.

## Fix

Add an `audit` job to `.github/workflows/ci.yml`, alongside the existing `fmt` / `clippy` / `test` jobs:

```yaml
audit:
  runs-on: ubuntu-latest
  steps:
    - uses: actions/checkout@v4
    - uses: rustsec/audit-check@v2
      with:
        token: ${{ secrets.GITHUB_TOKEN }}
```

Same `push: main` + `pull_request` triggers as the others (inherited from workflow-level `on:`).

## Acceptance

- `cargo audit` runs on every PR.
- Local: `cargo install cargo-audit && cargo audit` succeeds on current `Cargo.lock`.
- A test PR introducing a known-vulnerable dep (e.g. `time = "0.1.43"`) fails the audit job.

## Non-goals

- `cargo-deny` (broader license + ban policy) — follow-up if `audit` alone proves insufficient.
- Auto-fix PRs — `gp-ci-dependabot` handles that.

## Conflict note

Touches `.github/workflows/ci.yml` (same file as `gp-ci-os-matrix`). They modify DIFFERENT regions (this adds a new top-level `audit:` job; os-matrix modifies the existing `test:` job). Safe to parallel-run; second-to-merge needs a rebase.
