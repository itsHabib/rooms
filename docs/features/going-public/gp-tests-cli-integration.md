**Status**: draft
**Owner**: @michael (human:mh)
**Date**: 2026-05-27
**Related**: dossier task `gp-tests-cli-integration` (id: `tsk_01KSMXGCRJCMNHGR0Q61YV3PQD`), [going-public driver](driver.md)

# tests: add CLI integration tests subprocess-invoking target/debug/rooms

## Scope

| Bucket | Files | Est. LOC | Weighted |
|---|---|---|---|
| Tests | `tests/cli.rs` (new) | ~40 | 20 |
| Config | `Cargo.toml` (add `assert_cmd` + `predicates` dev-deps) | ~2 | 0 |
| **Total weighted** | | | **~20** |

Band: **amazing**.

## Goal

Cover the CLI surface (clap parsing, flag combinations, dispatch routing, exit codes) at the subprocess level — `tests/control_failures.rs` only exercises library calls.

## Fix

Add `assert_cmd = "2"` and `predicates = "3"` to `[dev-dependencies]` in `Cargo.toml`.

Create `tests/cli.rs` (no `e2e` feature gate — fast, no KVM):

```rust
use assert_cmd::Command;
use predicates::prelude::*;

#[test]
fn doctor_json_is_valid_json() {
    Command::cargo_bin("rooms").unwrap()
        .args(["doctor", "--json"])
        .assert()
        .stdout(predicate::function(|s: &str| {
            serde_json::from_str::<serde_json::Value>(s).is_ok()
        }));
}

#[test]
fn keep_and_command_are_mutually_exclusive() {
    Command::cargo_bin("rooms").unwrap()
        .args(["run", "--image", "/tmp/nonexistent", "--keep", "--command", "echo hi"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("cannot be used with"));
}

#[test]
fn run_without_image_fails_fast() {
    Command::cargo_bin("rooms").unwrap()
        .args(["run", "--command", "echo hi"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--image"));
}
```

## Acceptance

- `cargo test --test cli` passes on Linux AND Windows (no microVM boot involved).
- Tests catch regressions in clap definition, flag mutex, and `--json` stdout shape.
- Runs in <2s.

## Test plan

- Local: `cargo test --test cli` passes.
- Force a regression: rename `--json` to `--Json` in `src/main.rs`; `doctor_json_is_valid_json` test fails as expected.

## Non-goals

- E2E that actually boots a microVM via CLI — lives in `tests/control_failures.rs` (gated on `e2e`).
- Snapshot testing CLI help text — premature; clap output churn expected.

## Conflict note

Touches `Cargo.toml`'s `[dev-dependencies]` section (same as `gp-tests-proptest`). Different lines added; low rebase risk.
