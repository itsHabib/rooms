//! Host-e2e for `rooms run --egress` — the per-room egress *enforcer*. Each test
//! is a no-op skip on an unprovisioned host:
//!
//! - **`none` blocks all external:** a `--egress none` run that tries to reach a
//!   known destination cannot — the guest's request times out / is refused,
//!   verified *from inside the guest* by the command's own exit.
//! - **`none` blocks a spoofed source:** a guest that forges a *different*
//!   `172.16.0.x` source under `--egress none` is still blocked — the tap-keyed
//!   jump catches it, where a source-keyed rule would leak. The anti-spoof case.
//! - **`allowlist` permits only the listed dest:** a permitted destination
//!   succeeds and a denied one does not, again observed from the guest.
//! - **blocked attempts land in the receipt:** under `--witness`, a `none` run's
//!   `witness.json` shows policy `none`, an empty permitted set, and the guest's
//!   attempted destination in `blocked`. A run with no attempts is the
//!   proof-of-absence artifact: policy `none`, permitted `[]`, blocked `[]`.
//! - **absent flag is observe-only:** no `--egress` ⇒ the destination is
//!   reachable and the witness records `egress_policy: observe`, unchanged.
//!
//! The enforcement is keyed on the room's ingress tap `tap-fc<k>`, the one
//! surface the root-capable guest cannot forge — the same unforgeable object the
//! witness captures on. A source-keyed rule would be a spoofing bypass; the
//! spoof test proves the tap key holds.
//!
//! Gated behind `e2e` + unix. Requires the rooms-host: root, `/dev/kvm`,
//! Firecracker + jailer on PATH, guest images under `~/rooms/images`,
//! `~/.ssh/id_rooms`, `tcpdump` (for the receipt test), and the `ROOMS_FWD`
//! chain installed. Every precondition it can't meet is a skip, not a failure.
//!
//! Run on rooms-host:
//! `sudo -E cargo test --features e2e --test egress_e2e -- --nocapture`

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
/// Contacting it by raw IP keeps the assertion independent of DNS — which
/// matters doubly here, since `--egress none` also blocks DNS.
const TARGET_IP: &str = "93.184.216.34";

/// A second reachable IP used as the *denied* destination in the allowlist test
/// (Cloudflare's resolver — stable and unrelated to `TARGET_IP`).
const DENIED_IP: &str = "1.1.1.1";

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

/// The rootfs to boot, or a skip reason on stderr. Mirrors the witness-e2e
/// preflight — root, KVM, images, the chain, the guest key.
fn preflight() -> Option<PathBuf> {
    if !is_root() {
        eprintln!("skipping: e2e needs root (tap + iptables chain)");
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
    if !guest_key().exists() {
        eprintln!("skipping: ~/.ssh/id_rooms missing (bake-rootfs-ssh.sh not run)");
        return None;
    }
    Some(rootfs)
}

fn read_witness(out_dir: &Path) -> serde_json::Value {
    let path = out_dir.join("witness.json");
    let raw =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {} : {e}", path.display()));
    serde_json::from_str(&raw).expect("witness.json is valid JSON")
}

/// Run `rooms run` with the given extra args + guest command, returning the exit
/// code and captured stderr. Shared plumbing for every case below.
fn run_room(
    rootfs: &Path,
    out_dir: &Path,
    extra: &[&str],
    guest_cmd: &str,
) -> (Option<i32>, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_rooms"));
    cmd.arg("run");
    cmd.args(extra);
    cmd.args(["--command", guest_cmd]);
    cmd.arg("--image").arg(rootfs);
    cmd.arg("--out").arg(out_dir);
    let out = cmd.output().expect("spawn rooms run");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// A guest command that succeeds (exit 0) only if `ip` is reachable within a few
/// seconds — a short-timeout TCP connect, so a blocked destination fails fast
/// rather than hanging the whole run.
fn reach_cmd(ip: &str) -> String {
    // `nc -w` bounds the connect; fall back to a bounded wget for images without
    // netcat. Either way exit 0 ⇒ reached, non-zero ⇒ blocked.
    format!("timeout 8 sh -c 'nc -z -w4 {ip} 80 || wget -q -T4 -O- http://{ip}/'")
}

#[test]
fn egress_none_blocks_all_external() {
    let Some(rootfs) = preflight() else {
        return;
    };
    let tmp = tempfile::tempdir().expect("tempdir");
    let out_dir = tmp.path().join("out");
    let (code, stderr) = run_room(
        &rootfs,
        &out_dir,
        &["--egress", "none"],
        &reach_cmd(TARGET_IP),
    );
    // The guest command must FAIL — the destination is unreachable under `none`.
    assert_ne!(
        code,
        Some(0),
        "--egress none must block {TARGET_IP}; the guest reach unexpectedly succeeded. stderr:\n{stderr}"
    );
}

#[test]
fn egress_none_blocks_spoofed_source() {
    let Some(rootfs) = preflight() else {
        return;
    };
    let tmp = tempfile::tempdir().expect("tempdir");
    let out_dir = tmp.path().join("out");
    // The guest forges a DIFFERENT 172.16.0.x source, then tries to egress. A
    // source-keyed rule would miss the forged source and leak; the tap-keyed
    // jump catches every packet off tap-fc<k> regardless of source. Requires
    // raw-socket tooling in the guest; `|| true` on the spoof setup keeps the
    // assertion on the reach result, which must still fail.
    let spoof = format!(
        "ip addr add 172.16.0.200/30 dev eth0 2>/dev/null || true; \
         ip route add {TARGET_IP} src 172.16.0.200 dev eth0 2>/dev/null || true; \
         {}",
        reach_cmd(TARGET_IP)
    );
    let (code, stderr) = run_room(&rootfs, &out_dir, &["--egress", "none"], &spoof);
    assert_ne!(
        code,
        Some(0),
        "a spoofed source must not bypass --egress none (the tap-keyed jump catches it). stderr:\n{stderr}"
    );
}

#[test]
fn egress_allowlist_permits_only_listed() {
    let Some(rootfs) = preflight() else {
        return;
    };
    // Permitted destination reaches.
    let tmp = tempfile::tempdir().expect("tempdir");
    let permitted_out = tmp.path().join("permitted");
    let allow = format!("allowlist:{TARGET_IP}");
    let (permit_code, permit_err) = run_room(
        &rootfs,
        &permitted_out,
        &["--egress", &allow],
        &reach_cmd(TARGET_IP),
    );
    assert_eq!(
        permit_code,
        Some(0),
        "the allowlisted destination {TARGET_IP} must be reachable. stderr:\n{permit_err}"
    );
    // A destination NOT on the list is blocked.
    let denied_out = tmp.path().join("denied");
    let (deny_code, deny_err) = run_room(
        &rootfs,
        &denied_out,
        &["--egress", &allow],
        &reach_cmd(DENIED_IP),
    );
    assert_ne!(
        deny_code,
        Some(0),
        "a non-allowlisted destination {DENIED_IP} must be blocked. stderr:\n{deny_err}"
    );
}

#[test]
fn blocked_attempt_lands_in_receipt() {
    let Some(rootfs) = preflight() else {
        return;
    };
    if !tcpdump_present() {
        eprintln!("skipping: tcpdump not on PATH (the receipt needs the witness capture)");
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let out_dir = tmp.path().join("out");
    // A `none` run whose guest attempts an egress: the witness pcap records the
    // attempted SYN on the tap even though FORWARD drops it, so the receipt
    // shows the attempt in `blocked`.
    let _ = run_room(
        &rootfs,
        &out_dir,
        &["--egress", "none", "--witness"],
        &reach_cmd(TARGET_IP),
    );
    let witness = read_witness(&out_dir);
    assert_eq!(witness["egress_policy"], "none");
    assert_eq!(
        witness["permitted"]
            .as_array()
            .expect("permitted array")
            .len(),
        0,
        "under `none` nothing is permitted"
    );
    let blocked_hit = witness["blocked"]
        .as_array()
        .expect("blocked array")
        .iter()
        .any(|d| d["ip"] == TARGET_IP);
    assert!(
        blocked_hit,
        "the guest's attempted {TARGET_IP} must appear in the blocked set:\n{witness:#}"
    );
}

#[test]
fn egress_none_with_no_attempts_is_proof_of_absence() {
    let Some(rootfs) = preflight() else {
        return;
    };
    if !tcpdump_present() {
        eprintln!("skipping: tcpdump not on PATH (proof-of-absence needs the witness)");
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let out_dir = tmp.path().join("out");
    // A `none` run that never attempts egress: the proof-of-absence artifact —
    // policy none, empty permitted, empty blocked, empty destinations.
    let _ = run_room(
        &rootfs,
        &out_dir,
        &["--egress", "none", "--witness"],
        "true",
    );
    let witness = read_witness(&out_dir);
    assert_eq!(witness["egress_policy"], "none");
    assert_eq!(witness["permitted"].as_array().unwrap().len(), 0);
    assert_eq!(witness["blocked"].as_array().unwrap().len(), 0);
    assert_eq!(
        witness["destinations"].as_array().unwrap().len(),
        0,
        "a room that never egressed leaves zero destinations — provably nothing left:\n{witness:#}"
    );
    assert_eq!(witness["capture_complete"], true);
}

#[test]
fn absent_flag_is_observe_only() {
    let Some(rootfs) = preflight() else {
        return;
    };
    if !tcpdump_present() {
        eprintln!("skipping: tcpdump not on PATH");
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let out_dir = tmp.path().join("out");
    // No --egress: the destination is reachable (unchanged behavior) and the
    // witness records the observe policy.
    let (code, stderr) = run_room(&rootfs, &out_dir, &["--witness"], &reach_cmd(TARGET_IP));
    assert_eq!(
        code,
        Some(0),
        "without --egress the destination must remain reachable. stderr:\n{stderr}"
    );
    let witness = read_witness(&out_dir);
    assert_eq!(witness["egress_policy"], "observe");
    assert_eq!(
        witness["blocked"].as_array().expect("blocked array").len(),
        0,
        "observe enforces nothing"
    );
}
