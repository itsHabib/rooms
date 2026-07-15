//! Host-e2e for the `rooms run --command` exec path — the regression gate for
//! the silent-reap bug (a `--command` run torn down before the command ran,
//! producing no output). One test, a no-op skip on an unprovisioned host:
//!
//! - **echo token:** a real alpine-rootfs `rooms run --command "echo <TOKEN>"`
//!   exits 0, and the collected `--out` `logs/stdout.log` carries the token.
//!   The alpine agent rootfs accepts sshd only ~3.2s after boot — past the
//!   bare-boot 3s auto-shutdown window — so this fails the moment the exec path
//!   is (re)bounded by that fixed timer instead of waiting for sshd.
//!
//! The unit companion (`exec_survives_when_guest_ready_after_auto_shutdown_window`
//! in `src/main.rs`) pins the same invariant on a paused clock with no KVM; this
//! is its on-host counterpart, run at the batch gate.
//!
//! Gated behind `e2e` + unix. Requires the rooms-host: root, `/dev/kvm`,
//! Firecracker + jailer on PATH, guest images under `~/rooms/images`,
//! `~/.ssh/id_rooms`, and the `ROOMS_FWD` chain installed. Every precondition
//! it can't meet is a skip, not a failure.
//!
//! Run on rooms-host:
//! `sudo -E cargo test --features e2e --test run_command_e2e -- --nocapture`

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

/// The guest SSH key path. Must stay in lockstep with the binary's own
/// `key_path()` (`~/.ssh/id_rooms`); this only guards the skip below, so a
/// silent divergence would run the test without the key instead of skipping.
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

/// The rootfs to boot, or a skip reason on stderr. Mirrors the other host-e2e
/// preflights: root, `/dev/kvm`, images, and the installed `ROOMS_FWD` chain.
fn preflight() -> Option<PathBuf> {
    if !is_root() {
        eprintln!("skipping: e2e needs root (tap creation via `ip tuntap ... user firecracker`)");
        return None;
    }
    if !Path::new("/dev/kvm").exists() {
        eprintln!("skipping: no /dev/kvm (nested virt off?)");
        return None;
    }
    let kernel = image_path("vmlinux.bin");
    let rootfs = image_path("rootfs.ext4");
    if !kernel.exists() || !rootfs.exists() {
        eprintln!("skipping: guest images not present under ~/rooms/images");
        return None;
    }
    if !rooms_fwd_installed() {
        eprintln!(
            "skipping: ROOMS_FWD not installed; run `sudo bash scripts/setup-tap.sh --host` first"
        );
        return None;
    }
    Some(rootfs)
}

#[test]
fn run_command_echo_survives_slow_sshd_and_collects_token() {
    let Some(rootfs) = preflight() else {
        return;
    };
    if !guest_key().exists() {
        eprintln!("skipping: ~/.ssh/id_rooms missing (bake-rootfs-ssh.sh not run)");
        return;
    }

    // A distinct token so a stray match from an unrelated log can't pass this.
    let token = "ROOMS_ECHO_9F3A21";
    let tmp = tempfile::tempdir().expect("tempdir");
    let out_dir = tmp.path().join("out");

    let out = Command::new(env!("CARGO_BIN_EXE_rooms"))
        .args(["run", "--command", &format!("echo {token}")])
        .arg("--image")
        .arg(&rootfs)
        .arg("--out")
        .arg(&out_dir)
        .output()
        .expect("spawn rooms run");

    // The exit code must be the command's own 0 — not a silent no-op that also
    // happens to be 0. The collected stdout below is what proves the command
    // actually ran.
    assert_eq!(
        out.status.code(),
        Some(0),
        "echo should exit 0; stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // The runner captures the guest command's stdout under
    // `/workspace/out/logs/stdout.log`, collected into `--out` by the run.
    // The token being present is the proof the exec was NOT reaped before it
    // ran (the silent-reap bug produced an empty/absent log).
    let stdout_log = out_dir.join("logs/stdout.log");
    let captured = std::fs::read_to_string(&stdout_log).unwrap_or_else(|e| {
        panic!(
            "collected stdout log {} unreadable ({e}) — the exec likely never ran (silent reap)",
            stdout_log.display()
        )
    });
    assert!(
        captured.contains(token),
        "collected stdout must carry the echoed token {token} — the --command exec ran to \
         completion; got:\n{captured}"
    );
}
