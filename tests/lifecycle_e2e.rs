//! Host-e2e for the `--lifecycle` NDJSON stream — the machine-readable
//! lifecycle surface a supervising process consumes. Two tests, each a no-op
//! skip on an unprovisioned host:
//!
//! - **real boot:** the real `rooms run` binary boots a room, runs a command,
//!   collects, and tears down; the stream must be contiguous (seq from 1), in
//!   lifecycle order (allocation → VMM → guest ready → ssh ready → workload →
//!   collection → cleanup), and carry the workload's real exit.
//! - **pool full:** against a pre-filled pool the stream records a structured
//!   `pool_full` with the walked cap — admission rejection distinguishable
//!   without message-matching.
//!
//! Gated behind `e2e` + unix. Requires the rooms-host: root, `/dev/kvm`,
//! Firecracker + jailer on PATH, guest images under `~/rooms/images`,
//! `~/.ssh/id_rooms`, and the `ROOMS_FWD` chain installed. Every precondition
//! it can't meet is a skip, not a failure.
//!
//! Run on rooms-host:
//! `sudo -E cargo test --features e2e --test lifecycle_e2e -- --nocapture`

#![cfg(all(unix, feature = "e2e"))]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "e2e test module: panicky lints are noise in tests"
)]

use std::path::{Path, PathBuf};
use std::process::Command;

use rooms::firecracker;
use rooms::slot::{self, Claimer};

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

/// Everything the real-boot test needs, or a skip reason on stderr.
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

/// Parse the stream file into per-line JSON values, asserting every line is
/// standalone JSON and `seq` is contiguous from 1.
fn read_stream(path: &Path) -> Vec<serde_json::Value> {
    let raw = std::fs::read_to_string(path).expect("read lifecycle stream");
    let lines: Vec<serde_json::Value> = raw
        .lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("bad NDJSON line {l:?}: {e}")))
        .collect();
    for (i, line) in lines.iter().enumerate() {
        assert_eq!(
            line["seq"].as_u64(),
            Some(i as u64 + 1),
            "seq must be contiguous from 1; stream:\n{raw}"
        );
        assert!(line["ts"].is_string(), "every event carries a timestamp");
        assert!(
            line["room_id"].is_string(),
            "every event carries the room id"
        );
    }
    lines
}

/// Index of the first event with this tag, or a panic naming the stream.
fn position(lines: &[serde_json::Value], tag: &str) -> usize {
    lines
        .iter()
        .position(|l| l["event"] == tag)
        .unwrap_or_else(|| panic!("no {tag} event in stream: {lines:?}"))
}

#[test]
fn real_boot_streams_ordered_contiguous_lifecycle() {
    let Some(rootfs) = preflight() else {
        return;
    };
    if !guest_key().exists() {
        eprintln!("skipping: ~/.ssh/id_rooms missing (bake-rootfs-ssh.sh not run)");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let stream_path = tmp.path().join("lifecycle.ndjson");
    let out_dir = tmp.path().join("out");

    let out = Command::new(env!("CARGO_BIN_EXE_rooms"))
        .args(["run", "--command", "echo lifecycle-e2e"])
        .arg("--image")
        .arg(&rootfs)
        .arg("--out")
        .arg(&out_dir)
        .arg("--lifecycle")
        .arg(&stream_path)
        .output()
        .expect("spawn rooms run");
    assert_eq!(
        out.status.code(),
        Some(0),
        "run should succeed; stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let lines = read_stream(&stream_path);

    // One room, one id, across the whole stream.
    let room_id = lines[0]["room_id"].as_str().expect("room id").to_owned();
    assert!(
        lines.iter().all(|l| l["room_id"] == room_id.as_str()),
        "room_id constant across the stream"
    );

    // The full lifecycle, in order: each stage strictly after its predecessor.
    let order = [
        "slot_allocated",
        "vmm_started",
        "guest_ready",
        "ssh_ready",
        "workload_started",
        "workload_exited",
        "collection_started",
        "collection_done",
        "cleanup_done",
    ];
    let positions: Vec<usize> = order.iter().map(|tag| position(&lines, tag)).collect();
    for pair in positions.windows(2) {
        assert!(
            pair[0] < pair[1],
            "lifecycle order violated; stream: {lines:?}"
        );
    }

    // vmm_started is not readiness: the started/ready distinction is the point.
    let vmm = &lines[position(&lines, "vmm_started")];
    assert!(vmm["pid"].as_u64().is_some(), "vmm_started carries the pid");

    // The workload's real exit rides the stream, statused like result.json.
    let exited = &lines[position(&lines, "workload_exited")];
    assert_eq!(exited["exit_code"], 0);
    assert_eq!(exited["status"], "succeeded");
}

#[test]
fn full_pool_streams_structured_pool_full() {
    let Some(rootfs) = preflight() else {
        return;
    };

    // Isolate the child's pool by pointing its HOME at a tempdir: its default
    // state base is <HOME>/.local/state/rooms, so the pre-claim here is exactly
    // what it walks — never the real host pool.
    let home = tempfile::tempdir().expect("tempdir");
    let state_base = home.path().join(".local/state/rooms");
    let stream_path = home.path().join("lifecycle.ndjson");
    let me = Claimer::current().expect("read this process's claimer identity");

    let cap = 1u8;
    let holder = firecracker::mint_room_id();
    let claimed = slot::claim(&state_base, &holder, me, cap, None).expect("pre-claim slot 1");
    assert_eq!(claimed.index, 1, "pre-claim fills the only slot");

    let out = Command::new(env!("CARGO_BIN_EXE_rooms"))
        .env("HOME", home.path())
        .args(["run", "--command", "true", "--max-pool", "1"])
        .arg("--image")
        .arg(&rootfs)
        .arg("--lifecycle")
        .arg(&stream_path)
        .output()
        .expect("spawn rooms run");
    assert_eq!(
        out.status.code(),
        Some(4),
        "a full pool must exit 4; stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // The stream's one event is the structured admission rejection.
    let lines = read_stream(&stream_path);
    assert_eq!(
        lines.len(),
        1,
        "an admission-rejected run emits exactly the rejection: {lines:?}"
    );
    assert_eq!(lines[0]["event"], "pool_full");
    assert_eq!(lines[0]["cap"], 1);
}
