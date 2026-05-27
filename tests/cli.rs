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
