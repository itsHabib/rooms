**Status**: draft
**Owner**: @michael (human:mh)
**Date**: 2026-05-27
**Related**: dossier task `gp-ci-os-matrix` (id: `tsk_01KSMXE0H143PJXP89XHZ7VWJ1`), [going-public driver](driver.md)

# CI: add windows-latest to test job matrix

## Scope

| Bucket | Files | Est. LOC | Weighted |
|---|---|---|---|
| CI | `.github/workflows/ci.yml` (modify — test job to matrix) | ~10 | 10 |
| **Total weighted** | | | **~10** |

Band: **amazing**.

## Goal

Catch Windows-side breaks of cross-platform library code in CI rather than waiting for the operator's local push.

## Fix

Convert the `test` job in `.github/workflows/ci.yml` from single-OS to matrix:

```yaml
test:
  strategy:
    fail-fast: false
    matrix:
      os: [ubuntu-latest, windows-latest]
  runs-on: ${{ matrix.os }}
  steps:
    - uses: actions/checkout@v4
    - uses: dtolnay/rust-toolchain@stable
    - uses: Swatinem/rust-cache@v2
    - run: cargo test
```

`fmt` + `clippy` stay on `ubuntu-latest` (deterministic across OSes; saves runner time). The `e2e` feature stays opt-in, so the Windows leg runs unit tests only — same as `make check` does on the operator's Windows host.

## Acceptance

- Both matrix legs green on a no-op PR.
- A Windows-only compile break (e.g. stray `#[cfg(unix)]` mishap) fails CI rather than slipping through.

## Non-goals

- macOS leg — not part of operator's loop; cost without benefit.
- `e2e` on Windows — Firecracker is Linux-only; remains rooms-host-VM-only.

## Conflict note

Touches `.github/workflows/ci.yml` (same file as `gp-ci-cargo-audit`). They modify DIFFERENT regions (this modifies the `test:` job; cargo-audit adds a new top-level `audit:` job). Safe to parallel-run; second-to-merge needs a rebase.
