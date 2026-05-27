**Status**: draft
**Owner**: @michael (human:mh)
**Date**: 2026-05-27
**Related**: dossier task `gp-tests-proptest` (id: `tsk_01KSMXG43H8CKPPH1SHTT9YYDD`), [going-public driver](driver.md)

# tests: add proptest coverage for version parser, artifact validation, RoomGuard

## Scope

| Bucket | Files | Est. LOC | Weighted |
|---|---|---|---|
| Production | (none) | 0 | 0 |
| Tests | `src/doctor.rs` (proptest mod), `src/artifacts.rs` (proptest mod), `src/firecracker.rs` (proptest mod) | ~150 | 75 |
| Config | `Cargo.toml` (add proptest dev-dep) | ~2 | 0 |
| **Total weighted** | | | **~75** |

Band: **amazing**.

## Goal

Lift coverage from hand-picked unit cases to property-based testing on three shape-driven surfaces.

## Fix

1. Add `proptest = "1"` to `[dev-dependencies]` in `Cargo.toml`.

2. Add one `proptest!` module per surface:

- **`src/doctor.rs::tests::version_parser_properties`** — generate `(major, minor, patch)` tuples, format as `"Firecracker v{major}.{minor}.{patch}"` (and similar valid variants), assert `parse_firecracker_version` round-trips major + minor. Include adversarial inputs (empty, missing version, trailing junk).
- **`src/artifacts.rs::tests::path_validation_properties`** — generate path components including adversarial ones (`..`, leading `/`, embedded NUL, multi-segment escapes), assert rejection of escapes.
- **`src/firecracker.rs::tests::room_guard_properties`** — generate sequences of `set_*` / `dismiss` / `set_suppress_cleanup` calls, assert invariants: `dismiss` → no cleanup; `suppress_cleanup` → no cleanup; both → no cleanup; neither → cleanup.

Each module ~30-60 LOC including strategies.

## Acceptance

- `cargo test` runs the new property tests; each exercises ≥256 random inputs (proptest default).
- A regression introduced into any of the three surfaces (e.g. weakening symlink-escape rejection) fails the corresponding property test.

## Test plan

- Local: `cargo test --lib version_parser_properties path_validation_properties room_guard_properties` passes.
- After `gp-ci-mutants-workflow` lands: re-run cargo-mutants and confirm surviving mutants in these three surfaces drop.

## Non-goals

- Property tests for `runner` / `transport` / `firecracker::boot` paths — those touch I/O and need test doubles; cost > benefit at v0.
- Increasing proptest case counts past defaults.

## Conflict note

Touches `Cargo.toml`'s `[dev-dependencies]` section (same as `gp-tests-cli-integration`). Different lines added; low rebase risk.
