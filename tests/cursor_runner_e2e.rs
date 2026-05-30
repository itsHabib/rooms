#![cfg(feature = "e2e")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "e2e test: setup failures are test bugs; needs Firecracker + KVM on the rooms-host"
)]

//! End-to-end cursor-runner tests. Gated behind the `e2e` feature and the
//! `ROOMS_E2E_IMAGE` / `ROOMS_E2E_REPO` env vars. They require Firecracker +
//! `/dev/kvm`, a built `agent-alpine-cursor.ext4`, and an up `tap-fc0`
//! (`scripts/setup-tap.sh`) on the rooms-host. CI compiles these (via
//! `--all-features`) but never runs them (`cargo test` leaves `e2e` off).
//!
//! The success round-trip (a patch landing in `result.patch`) is validated by
//! the host dogfood under `--keep`, since the non-`--keep` path tears the VM
//! down before artifacts can be collected host-side.

use std::io::Write;

use assert_cmd::Command;

/// Read a required env var, or signal skip (printing why) when it is absent.
fn env_or_skip(key: &str) -> Option<String> {
    match std::env::var(key) {
        Ok(value) if !value.is_empty() => Some(value),
        _ => {
            eprintln!("skipping cursor e2e: {key} not set");
            None
        }
    }
}

/// `rooms run --runner cursor` with no `CURSOR_API_KEY` must exit non-zero:
/// `cursor-runner.js` takes the `api_key` error path (exit 2) and the substrate
/// propagates it. Exercises the clone -> stage -> exec orchestration end-to-end
/// plus the auth-failure taxonomy, without spending API tokens.
#[test]
fn cursor_auth_failure_exits_nonzero() {
    let Some(image) = env_or_skip("ROOMS_E2E_IMAGE") else {
        return;
    };
    let Some(repo) = env_or_skip("ROOMS_E2E_REPO") else {
        return;
    };
    let base_sha = std::env::var("ROOMS_E2E_BASE_SHA").unwrap_or_else(|_| "HEAD".to_owned());
    let model = std::env::var("ROOMS_E2E_MODEL").unwrap_or_else(|_| "claude-4.5-sonnet".to_owned());

    let mut task = tempfile::NamedTempFile::new().unwrap();
    writeln!(task, "Append a line `# rooms` to README.md.").unwrap();

    Command::cargo_bin("rooms")
        .unwrap()
        .env_remove("CURSOR_API_KEY")
        .args([
            "run",
            "--image",
            &image,
            "--runner",
            "cursor",
            "--repo",
            &repo,
            "--task",
            task.path().to_str().unwrap(),
            "--model",
            &model,
            "--base-sha",
            &base_sha,
        ])
        .assert()
        .failure();
}
