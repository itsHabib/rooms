//! Host-e2e for `rooms run --witness` — the host-side egress witness. Each test
//! is a no-op skip on an unprovisioned host:
//!
//! - **captures egress:** a `--witness` run that contacts a known destination
//!   produces `out/witness.json` listing it with `capture_complete: true`, and
//!   `out/witness.pcap` exists. The lifecycle stream carries `witness_started`
//!   and `witness_done`.
//! - **off by default:** the same command *without* `--witness` produces neither
//!   artifact and emits no witness lifecycle events.
//! - **adversarial:** a guest that contacts a destination and then deletes its
//!   own guest-side `out/` still yields that destination in `witness.json` — the
//!   witness is observed on the host tap, so it does not depend on the guest.
//!
//! The unforgeability argument: `witness.pcap` is written by `tcpdump` on the
//! host's `tap-fc<k>`, an object the guest cannot reach. Deleting guest-side
//! traces cannot remove packets the host already recorded off the wire.
//!
//! Gated behind `e2e` + unix. Requires the rooms-host: root, `/dev/kvm`,
//! Firecracker + jailer + tcpdump on PATH, guest images under `~/rooms/images`,
//! `~/.ssh/id_rooms`, and the `ROOMS_FWD` chain installed. Every precondition it
//! can't meet is a skip, not a failure.
//!
//! Run on rooms-host:
//! `sudo -E cargo test --features e2e --test witness_e2e -- --nocapture`

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

/// A stable, well-known egress destination (example.com's canonical address).
/// Contacting it by raw IP keeps the witness assertion independent of DNS.
const TARGET_IP: &str = "93.184.216.34";

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

fn tcpdump_present() -> bool {
    Command::new("tcpdump")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success() || o.status.code().is_some())
}

/// The rootfs to boot, or a skip reason on stderr. Mirrors the other host-e2e
/// preflights, plus the witness-specific `tcpdump` requirement and the guest key
/// (every witness test runs a guest command).
fn preflight() -> Option<PathBuf> {
    if !is_root() {
        eprintln!("skipping: e2e needs root (tap creation + tcpdump on the tap)");
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
    if !tcpdump_present() {
        eprintln!("skipping: tcpdump not on PATH (the witness capture needs it)");
        return None;
    }
    if !guest_key().exists() {
        eprintln!("skipping: ~/.ssh/id_rooms missing (bake-rootfs-ssh.sh not run)");
        return None;
    }
    Some(rootfs)
}

/// Parse an NDJSON lifecycle stream into per-line JSON values.
fn read_stream(path: &Path) -> Vec<serde_json::Value> {
    let raw = std::fs::read_to_string(path).expect("read lifecycle stream");
    raw.lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("bad NDJSON line {l:?}: {e}")))
        .collect()
}

fn read_witness(out_dir: &Path) -> serde_json::Value {
    let path = out_dir.join("witness.json");
    let raw =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {} : {e}", path.display()));
    serde_json::from_str(&raw).expect("witness.json is valid JSON")
}

#[test]
fn witness_records_egress_destination_on_the_host_tap() {
    let Some(rootfs) = preflight() else {
        return;
    };

    let tmp = tempfile::tempdir().expect("tempdir");
    let out_dir = tmp.path().join("out");
    let stream_path = tmp.path().join("lifecycle.ndjson");

    // Contact the target by raw IP; `|| true` so a network hiccup doesn't fail
    // the run — the witness records the attempt regardless of the reply.
    let cmd = format!("wget -q -O- http://{TARGET_IP}/ || true");
    let out = Command::new(env!("CARGO_BIN_EXE_rooms"))
        .args(["run", "--witness", "--command", &cmd])
        .arg("--image")
        .arg(&rootfs)
        .arg("--out")
        .arg(&out_dir)
        .arg("--lifecycle")
        .arg(&stream_path)
        .output()
        .expect("spawn rooms run --witness");
    assert_eq!(
        out.status.code(),
        Some(0),
        "run should succeed; stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // The raw evidence survives the room.
    assert!(
        out_dir.join("witness.pcap").exists(),
        "witness.pcap must be copied into --out"
    );

    let witness = read_witness(&out_dir);
    assert_eq!(witness["schema_version"], 1);
    assert_eq!(
        witness["capture_complete"], true,
        "a clean capture is complete; witness:\n{witness:#}"
    );
    let hit = witness["destinations"]
        .as_array()
        .expect("destinations array")
        .iter()
        .any(|d| d["ip"] == TARGET_IP);
    assert!(
        hit,
        "the contacted destination {TARGET_IP} must appear in witness.json:\n{witness:#}"
    );

    // The lifecycle stream carries the witness transitions.
    let lines = read_stream(&stream_path);
    assert!(
        lines.iter().any(|l| l["event"] == "witness_started"),
        "witness_started must be on the stream: {lines:?}"
    );
    let done = lines
        .iter()
        .find(|l| l["event"] == "witness_done")
        .expect("witness_done on the stream");
    assert_eq!(done["complete"], true);
    assert!(done["destinations"].as_u64().unwrap() >= 1);
}

#[test]
fn without_witness_no_artifacts_and_no_events() {
    let Some(rootfs) = preflight() else {
        return;
    };

    let tmp = tempfile::tempdir().expect("tempdir");
    let out_dir = tmp.path().join("out");
    let stream_path = tmp.path().join("lifecycle.ndjson");

    let cmd = format!("wget -q -O- http://{TARGET_IP}/ || true");
    let out = Command::new(env!("CARGO_BIN_EXE_rooms"))
        .args(["run", "--command", &cmd])
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

    assert!(
        !out_dir.join("witness.json").exists(),
        "no --witness → no witness.json"
    );
    assert!(
        !out_dir.join("witness.pcap").exists(),
        "no --witness → no witness.pcap"
    );
    let lines = read_stream(&stream_path);
    assert!(
        !lines
            .iter()
            .any(|l| l["event"] == "witness_started" || l["event"] == "witness_done"),
        "no --witness → no witness lifecycle events: {lines:?}"
    );
}

#[test]
fn witness_survives_guest_tampering_with_its_own_out() {
    let Some(rootfs) = preflight() else {
        return;
    };

    let tmp = tempfile::tempdir().expect("tempdir");
    let out_dir = tmp.path().join("out");

    // The adversary: contact the destination, then scrub every guest-side trace
    // it can reach (its own /workspace/out). The host tap already recorded the
    // packets, so the witness stands regardless.
    let cmd = format!(
        "wget -q -O- http://{TARGET_IP}/ || true; rm -rf /workspace/out/* 2>/dev/null || true"
    );
    let out = Command::new(env!("CARGO_BIN_EXE_rooms"))
        .args(["run", "--witness", "--command", &cmd])
        .arg("--image")
        .arg(&rootfs)
        .arg("--out")
        .arg(&out_dir)
        .output()
        .expect("spawn rooms run --witness");
    // The guest's own scrub may perturb collection, but the run's exit is the
    // command's; either way the host witness is what we assert on.
    assert!(
        out.status.code().is_some(),
        "run terminated; stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let witness = read_witness(&out_dir);
    let hit = witness["destinations"]
        .as_array()
        .expect("destinations array")
        .iter()
        .any(|d| d["ip"] == TARGET_IP);
    assert!(
        hit,
        "the witness records {TARGET_IP} despite guest-side tampering:\n{witness:#}"
    );
}
