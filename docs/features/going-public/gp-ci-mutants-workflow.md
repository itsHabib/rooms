**Status**: draft
**Owner**: @michael (human:mh)
**Date**: 2026-05-27
**Related**: dossier task `gp-ci-mutants-workflow` (id: `tsk_01KSMXEDXV4VC2M3V8A1R12B3G`), [going-public driver](driver.md)

# CI: add workflow_dispatch mutation-testing job using cargo-mutants

## Scope

| Bucket | Files | Est. LOC | Weighted |
|---|---|---|---|
| Config | `.cargo/mutants.toml` (new) | ~4 | 0 |
| CI | `.github/workflows/mutants.yml` (new) | ~20 | 0 |
| **Total weighted** | | | **~0** |

Band: **amazing**.

## Goal

Verify the unit-test suite catches mutated behavior — signal on assertion strength.

## Fix

Create `.cargo/mutants.toml`:

```toml
exclude_globs = ["src/main.rs"]
exclude_re = ["tests/fixtures/.*"]
timeout_multiplier = 3.0
```

Create `.github/workflows/mutants.yml`:

```yaml
name: mutants

on:
  workflow_dispatch:

jobs:
  mutants:
    runs-on: ubuntu-latest
    timeout-minutes: 60
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2
      - run: cargo install cargo-mutants
      - run: cargo mutants --no-shuffle --in-place
      - uses: actions/upload-artifact@v4
        if: always()
        with:
          name: mutants
          path: mutants.out/
```

Manual trigger only — runs take 30-60 min.

## Acceptance

- `gh workflow run mutants.yml --repo itsHabib/rooms` produces a `mutants.out/` artifact.
- Locally: `cargo install cargo-mutants && cargo mutants --in-place --timeout-multiplier 3 -- --lib` completes within 30-60 min.

## Non-goals

- Acting on surviving mutants — separate follow-ups, one task per critical region.
- Running mutants on PR (too slow + noisy).
