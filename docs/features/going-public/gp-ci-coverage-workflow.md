**Status**: draft
**Owner**: @michael (human:mh)
**Date**: 2026-05-27
**Related**: dossier task `gp-ci-coverage-workflow` (id: `tsk_01KSMXE7E7TKMAPTRMD5NBMC0K`), [going-public driver](driver.md)

# CI: add workflow_dispatch coverage job using cargo-llvm-cov

## Scope

| Bucket | Files | Est. LOC | Weighted |
|---|---|---|---|
| CI | `.github/workflows/coverage.yml` (new) | ~25 | 0 |
| **Total weighted** | | | **~0** |

Band: **amazing**.

## Goal

Surface coverage trends across batches without blocking PRs.

## Fix

Create `.github/workflows/coverage.yml`:

```yaml
name: coverage

on:
  workflow_dispatch:

jobs:
  coverage:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          components: llvm-tools-preview
      - uses: Swatinem/rust-cache@v2
      - run: cargo install cargo-llvm-cov
      - run: cargo llvm-cov --lcov --output-path lcov.info
      - uses: actions/upload-artifact@v4
        with:
          name: coverage
          path: lcov.info
```

`workflow_dispatch` only — fires on demand via `gh workflow run`, no scheduled runs. Uploads lcov as an artifact; no third-party subscription required.

## Acceptance

- `gh workflow run coverage.yml --repo itsHabib/rooms` produces an `lcov.info` artifact attached to the run.

## Non-goals

- Coverage threshold gates (fail-CI-on-drop) — wait until baseline established.
- Codecov / Coveralls / SonarCloud integration — artifact upload is enough for v0.
- `e2e` feature coverage — needs KVM, not available in GitHub runners.
