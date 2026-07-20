//! Host-e2e for `rooms run --secret` — the vsock first-read-then-delete
//! hand-off (`docs/features/vsock-secrets/spec.md` §10). Three gates:
//!
//! - **admission:** an unset `--secret` var fails before any slot is claimed.
//! - **delivery:** on an image carrying the fetch hook, the secret is staged
//!   at `/run/rooms/secrets.env`, `secrets_delivered` precedes
//!   `workload_started` on the stream, and the value is absent from the
//!   workload's environment.
//! - **fail closed:** on an image WITHOUT the hook, the gate times out —
//!   `secrets_failed` is emitted, `workload_started` never is, and the room
//!   still reaches `cleanup_done` (the spec §6 old-image row).
//!
//! Gated behind `e2e` + unix; every unmet precondition is a skip, not a
//! failure. Run on rooms-host:
//! `sudo -E cargo test --features e2e --test secrets_e2e -- --nocapture`

#![cfg(all(unix, feature = "e2e"))]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "e2e test module: panicky lints are noise in tests"
)]

use std::path::{Path, PathBuf};
use std::process::Command;

fn image_path(name: &str) -> PathBuf {
    let home = std::env::var("HOME").expect("HOME");
    PathBuf::from(home).join("rooms/images").join(name)
}

fn guest_key() -> PathBuf {
    let home = std::env::var("HOME").expect("HOME");
    PathBuf::from(home).join(".ssh/id_rooms")
}

fn is_root() -> bool {
    Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .is_some_and(|s| s.trim() == "0")
}

fn rooms_fwd_installed() -> bool {
    Command::new("iptables")
        .args(["-S", "ROOMS_FWD"])
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Whether the ext4 image carries the guest fetch hook, via debugfs. `None`
/// when debugfs can't answer (missing tool) — callers skip rather than guess.
fn image_has_fetch_hook(image: &Path) -> Option<bool> {
    let out = Command::new("debugfs")
        .args(["-R", "stat /sbin/rooms-secrets-fetch"])
        .arg(image)
        .output()
        .ok()?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    Some(!text.contains("File not found"))
}

/// Common preflight: root, KVM, kernel, key, firewall chain. Returns the
/// kernel-adjacent images dir marker (the kernel itself) or skips.
fn preflight() -> bool {
    if !is_root() {
        eprintln!("skipping: e2e needs root");
        return false;
    }
    if !Path::new("/dev/kvm").exists() {
        eprintln!("skipping: no /dev/kvm (nested virt off?)");
        return false;
    }
    if !image_path("vmlinux.bin").exists() {
        eprintln!("skipping: kernel not present under ~/rooms/images");
        return false;
    }
    if !guest_key().exists() {
        eprintln!("skipping: ~/.ssh/id_rooms missing");
        return false;
    }
    if !rooms_fwd_installed() {
        eprintln!("skipping: ROOMS_FWD not installed");
        return false;
    }
    true
}

/// Events on the stream, or empty when the file was never created — an
/// admission failure precedes stream creation (both are pre-claim inputs),
/// so "no file" is the strongest possible "no slot was claimed".
fn lifecycle_events(path: &Path) -> Vec<String> {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    raw.lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter_map(|v| v.get("event").and_then(|e| e.as_str()).map(str::to_owned))
        .collect()
}

#[test]
fn unset_secret_fails_at_admission_without_claiming_a_slot() {
    if !preflight() {
        return;
    }
    let rootfs = image_path("rootfs.ext4");
    if !rootfs.exists() {
        eprintln!("skipping: rootfs.ext4 not present");
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let lifecycle = tmp.path().join("lc.ndjson");

    let out = Command::new(env!("CARGO_BIN_EXE_rooms"))
        .args(["run", "--command", "true"])
        .arg("--image")
        .arg(&rootfs)
        .arg("--lifecycle")
        .arg(&lifecycle)
        .args(["--secret", "ROOMS_E2E_DEFINITELY_UNSET"])
        .env_remove("ROOMS_E2E_DEFINITELY_UNSET")
        .output()
        .expect("spawn rooms run");

    assert_ne!(out.status.code(), Some(0), "unset secret must fail the run");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("not set in the host environment"),
        "admission error should name the cause; stderr:\n{stderr}"
    );
    let events = lifecycle_events(&lifecycle);
    assert!(
        !events.iter().any(|e| e == "slot_allocated"),
        "admission failure must precede the slot claim; events: {events:?}"
    );
}

#[test]
fn secret_is_staged_via_vsock_and_absent_from_workload_env() {
    if !preflight() {
        return;
    }
    let rootfs = image_path("agent-alpine.ext4");
    if !rootfs.exists() {
        eprintln!("skipping: agent-alpine.ext4 not present");
        return;
    }
    match image_has_fetch_hook(&rootfs) {
        Some(true) => {}
        Some(false) => {
            eprintln!("skipping: agent-alpine.ext4 predates the vsock fetch hook (rebuild it)");
            return;
        }
        None => {
            eprintln!("skipping: debugfs unavailable to probe the image");
            return;
        }
    }

    let token = "ROOMS_VSOCK_S3CRET_71B2";
    let tmp = tempfile::tempdir().expect("tempdir");
    let out_dir = tmp.path().join("out");
    let lifecycle = tmp.path().join("lc.ndjson");

    // The workload prints the staged file and its own environment: the file
    // must carry the secret, the environment must NOT (T3 — vsock delivery
    // replaces ambient env, it doesn't duplicate into it).
    let command = "echo FILE:$(cat /run/rooms/secrets.env); echo ENV_BEGIN; env; echo ENV_END";
    let out = Command::new(env!("CARGO_BIN_EXE_rooms"))
        .args(["run", "--command", command])
        .arg("--image")
        .arg(&rootfs)
        .arg("--out")
        .arg(&out_dir)
        .arg("--lifecycle")
        .arg(&lifecycle)
        .args(["--secret", "ROOMS_E2E_SECRET"])
        .env("ROOMS_E2E_SECRET", token)
        .output()
        .expect("spawn rooms run");
    assert_eq!(
        out.status.code(),
        Some(0),
        "secreted run should succeed; stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let events = lifecycle_events(&lifecycle);
    let delivered = events.iter().position(|e| e == "secrets_delivered");
    let started = events.iter().position(|e| e == "workload_started");
    assert!(
        delivered.is_some_and(|d| started.is_some_and(|s| d < s)),
        "secrets_delivered must precede workload_started; events: {events:?}"
    );

    let captured =
        std::fs::read_to_string(out_dir.join("logs/stdout.log")).expect("stdout log collected");
    assert!(
        captured.contains(&format!("ROOMS_E2E_SECRET={token}")),
        "staged secrets.env must carry the secret; got:\n{captured}"
    );
    let env_dump = captured
        .split("ENV_BEGIN")
        .nth(1)
        .and_then(|s| s.split("ENV_END").next())
        .expect("env dump markers present");
    assert!(
        !env_dump.contains(token),
        "the secret must be absent from the workload's environment; env dump:\n{env_dump}"
    );
}

#[test]
fn image_without_the_hook_fails_closed_before_the_workload() {
    if !preflight() {
        return;
    }
    let rootfs = image_path("rootfs.ext4");
    if !rootfs.exists() {
        eprintln!("skipping: rootfs.ext4 not present");
        return;
    }
    match image_has_fetch_hook(&rootfs) {
        Some(false) => {}
        Some(true) => {
            eprintln!(
                "skipping: rootfs.ext4 now carries the fetch hook; this row needs a hookless image"
            );
            return;
        }
        None => {
            eprintln!("skipping: debugfs unavailable to probe the image");
            return;
        }
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let lifecycle = tmp.path().join("lc.ndjson");
    let out = Command::new(env!("CARGO_BIN_EXE_rooms"))
        .args(["run", "--command", "echo MUST_NEVER_RUN"])
        .arg("--image")
        .arg(&rootfs)
        .arg("--lifecycle")
        .arg(&lifecycle)
        .args(["--secret", "ROOMS_E2E_SECRET"])
        .env("ROOMS_E2E_SECRET", "any-value")
        .output()
        .expect("spawn rooms run");
    assert_ne!(
        out.status.code(),
        Some(0),
        "a room that never got its secret must fail"
    );

    let events = lifecycle_events(&lifecycle);
    assert!(
        events.iter().any(|e| e == "secrets_failed"),
        "secrets_failed must be on the stream; events: {events:?}"
    );
    assert!(
        !events.iter().any(|e| e == "workload_started"),
        "fail closed: no path to workload_started without a confirmed delivery; events: {events:?}"
    );
    assert!(
        events.iter().any(|e| e == "cleanup_done"),
        "the failed room must still be reaped; events: {events:?}"
    );
}
