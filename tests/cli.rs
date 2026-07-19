#![allow(
    clippy::unwrap_used,
    reason = "integration test: assert_cmd setup failures are test bugs"
)]

use assert_cmd::Command;
use predicates::prelude::*;

#[test]
fn doctor_json_is_valid_json() {
    Command::cargo_bin("rooms")
        .unwrap()
        .args(["doctor", "--json"])
        .assert()
        .code(predicate::in_iter([0_i32, 1_i32]))
        .stdout(predicate::function(|s: &str| {
            serde_json::from_str::<serde_json::Value>(s).is_ok()
        }));
}

#[test]
fn keep_and_command_are_mutually_exclusive() {
    Command::cargo_bin("rooms")
        .unwrap()
        .args([
            "run",
            "--image",
            "/tmp/nonexistent",
            "--keep",
            "--command",
            "echo hi",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("cannot be used with"));
}

#[test]
fn run_without_image_fails_fast() {
    Command::cargo_bin("rooms")
        .unwrap()
        .args(["run", "--command", "echo hi"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--image"));
}

#[test]
fn witness_without_tcpdump_fails_closed_before_boot() {
    // The witness tcpdump check is the earliest guard in `run`, before kernel
    // validation or any slot claim — so with PATH pointing at an empty dir (no
    // tcpdump) the run must fail with a clear message and never reach boot. This
    // is the acceptance criterion "`--witness` on a host without tcpdump fails
    // before VMM start"; it needs no KVM, so it runs in `make check`.
    let empty = tempfile::tempdir().unwrap();
    Command::cargo_bin("rooms")
        .unwrap()
        .env("PATH", empty.path())
        .args([
            "run",
            "--witness",
            "--image",
            "/tmp/nonexistent-rooms-image",
            "--command",
            "true",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--witness").and(predicate::str::contains("tcpdump")));
}
