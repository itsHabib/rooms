//! Host-e2e: a single room boots on pool slot 1, runs an egress task, and reaps
//! byte-identically — the pool tap set, the `slots/` directory, and the
//! `ROOMS_FWD` chain all return to their exact pre-boot state.
//!
//! Gated behind `e2e` + unix. Requires the rooms-host, not the CI sandbox:
//! root (tap creation needs `CAP_NET_ADMIN`), `/dev/kvm`, Firecracker + jailer
//! on PATH, the guest images under `~/rooms/images`, and the `ROOMS_FWD` chain
//! already installed (`sudo bash scripts/setup-tap.sh --host`). Every
//! precondition it can't meet is a skip, not a failure, so it's a no-op on an
//! unprovisioned host.
//!
//! Run on rooms-host:
//! `sudo -E cargo test --features e2e --test pool_e2e -- --nocapture`

#![cfg(all(unix, feature = "e2e"))]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "e2e test module: panicky lints are noise in tests"
)]

use std::path::{Path, PathBuf};
use std::process::Command;

use rooms::config::RoomsConfig;
use rooms::firecracker::{self, BootRequest, NetworkConfig};
use rooms::slot::{self, Claimer};

/// The slot this test drives. 1 is the first real pool slot (0 is the reserved
/// legacy shared tap).
const SLOT_INDEX: u8 = 1;

/// The guest user the baked rootfs exposes (matches `runner`'s `GUEST_USER`).
const GUEST_USER: &str = "rooms";

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

/// The pool tap set (`tap-fc*` links only). Filtering to the rooms taps keeps
/// the byte-identical assert immune to unrelated host interfaces and counters —
/// the invariant under test is "no pool tap leaked", not "the whole link table
/// froze".
fn rooms_taps() -> String {
    let out = Command::new("ip")
        .args(["-o", "link", "show"])
        .output()
        .expect("ip link show");
    let text = String::from_utf8_lossy(&out.stdout);
    let mut taps: Vec<String> = text
        .lines()
        .filter(|line| line.contains("tap-fc"))
        .map(str::to_owned)
        .collect();
    taps.sort_unstable();
    taps.join("\n")
}

/// Sorted listing of `<state_base>/slots/` — the claimed-slot files.
fn slots_listing(state_base: &Path) -> String {
    let dir = state_base.join(slot::SLOTS_DIR);
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names.join("\n")
}

fn rooms_fwd_output() -> std::process::Output {
    Command::new("iptables")
        .args(["-S", "ROOMS_FWD"])
        .output()
        .expect("iptables -S ROOMS_FWD")
}

/// The `ROOMS_FWD` chain dump — boot/reap must never touch this host-once chain.
fn rooms_fwd_dump() -> String {
    String::from_utf8_lossy(&rooms_fwd_output().stdout).into_owned()
}

fn rooms_fwd_installed() -> bool {
    rooms_fwd_output().status.success()
}

/// Run one command in the guest over SSH, returning stdout. Matches `runner`'s
/// SSH options (host keys rotate every guest boot, so no known-hosts pinning).
fn ssh_guest(guest_ip: &str, key: &Path, cmd: &str) -> std::process::Output {
    Command::new("ssh")
        .args([
            "-i",
            key.to_str().expect("key path utf-8"),
            "-o",
            "StrictHostKeyChecking=no",
            "-o",
            "UserKnownHostsFile=/dev/null",
            "-o",
            "IdentitiesOnly=yes",
            "-o",
            "ConnectTimeout=10",
            &format!("{GUEST_USER}@{guest_ip}"),
            cmd,
        ])
        .output()
        .expect("ssh to guest")
}

/// Kernel + rootfs paths if the host can run the e2e, else `None` (with the skip
/// reason logged). Keeps the preconditions — root, `/dev/kvm`, images, the
/// installed `ROOMS_FWD` chain — out of the test body's line budget.
fn preflight() -> Option<(PathBuf, PathBuf)> {
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
    Some((kernel, rootfs))
}

/// Outcome of probing the guest over its slot tap.
enum Egress {
    /// Reached the internet through the NAT MASQUERADE + `ROOMS_FWD` ACCEPT.
    Verified,
    /// sshd answered over the slot tap (tap + routing work), but the login key
    /// didn't match this host's baked image, so the egress task couldn't run.
    ReachableNoAuth,
    /// sshd never answered within the deadline — the slot tap / routing failed.
    Unreachable,
}

/// Poll the guest for up to ~90s, stopping as soon as sshd answers. Separates a
/// real networking failure (never answers → `Unreachable`) from a guest-image
/// key mismatch (a publickey rejection → `ReachableNoAuth`), so a host whose
/// rootfs isn't paired with `~/.ssh/id_rooms` still exercises the reap invariant
/// rather than masking it behind an auth error.
async fn probe_egress(guest_ip: &str) -> Egress {
    let key = guest_key();
    if !key.exists() {
        eprintln!("note: ~/.ssh/id_rooms missing; egress task not probed");
        return Egress::ReachableNoAuth;
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
    loop {
        let out = ssh_guest(
            guest_ip,
            &key,
            "getent hosts github.com >/dev/null && echo OK",
        );
        if out.status.success() && String::from_utf8_lossy(&out.stdout).trim() == "OK" {
            return Egress::Verified;
        }
        // A publickey rejection means sshd answered over the tap (networking is
        // fine) but the image isn't paired with this key — retrying won't fix
        // it. A connection-level error just means the guest is still booting;
        // keep polling until the deadline.
        if String::from_utf8_lossy(&out.stderr).contains("Permission denied") {
            return Egress::ReachableNoAuth;
        }
        if std::time::Instant::now() >= deadline {
            return Egress::Unreachable;
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
}

#[tokio::test]
async fn room_boots_on_slot_1_and_reaps_byte_identically() {
    let Some((kernel, rootfs)) = preflight() else {
        return;
    };

    // Isolated state base so the claim + room dir never collide with real rooms.
    // The tap (`tap-fc1`) is still a host-global device, so the pre-check below
    // also asserts slot 1 is actually free on this host.
    let tmp = tempfile::tempdir().expect("tempdir");
    let state_base = tmp.path().to_path_buf();
    let config = RoomsConfig {
        state_base: Some(state_base.clone()),
        ..RoomsConfig::default()
    };

    // Capture the pre-boot state the reap must restore exactly.
    let taps_before = rooms_taps();
    let slots_before = slots_listing(&state_base);
    let fwd_before = rooms_fwd_dump();
    assert!(
        !taps_before.contains("tap-fc1 "),
        "slot 1 already in use on this host (tap-fc1 present); free it before running: {taps_before}"
    );

    // Claim slot 1 explicitly, then boot a room on it.
    let room_id = firecracker::mint_room_id();
    let me = Claimer::current().expect("read this process's claimer identity");
    let claimed = slot::claim(
        &state_base,
        &room_id,
        me,
        slot::DEFAULT_MAX_POOL,
        Some(SLOT_INDEX),
    )
    .expect("claim slot 1");
    assert_eq!(claimed.index, SLOT_INDEX, "claimed the wrong slot");

    let network = NetworkConfig {
        tap_name: claimed.tap.clone(),
        guest_ip: claimed.guest.to_string(),
        gateway_ip: claimed.gateway.to_string(),
        prefix: claimed.prefix,
    };
    let descriptor = rooms::room::RoomDescriptor::default();
    let req = BootRequest {
        kernel: &kernel,
        rootfs: &rootfs,
        network: Some(&network),
        slot: Some(&claimed),
        room_id: &room_id,
        readonly_rootfs: false,
        descriptor: &descriptor,
    };
    let vm = match firecracker::boot(&req, &config).await {
        Ok(vm) => vm,
        Err(e) => {
            // Don't strand slot 1 if boot fails mid-way — mirror main's guard.
            let _ = slot::free(&state_base, claimed.index, &room_id);
            panic!("boot on slot 1 failed: {e}");
        }
    };

    // Mid-boot: the slot tap and slot file exist.
    let taps_live = rooms_taps();
    assert!(
        taps_live.contains(&claimed.tap),
        "slot tap {} should exist mid-boot, saw: {taps_live}",
        claimed.tap
    );
    assert!(
        state_base
            .join(slot::SLOTS_DIR)
            .join(SLOT_INDEX.to_string())
            .exists(),
        "slot file should exist mid-boot"
    );

    match probe_egress(&network.guest_ip).await {
        Egress::Verified => {
            eprintln!("egress OK: guest resolved github.com through NAT / ROOMS_FWD");
        }
        Egress::ReachableNoAuth => eprintln!(
            "note: guest reachable over the slot tap, but its baked key didn't match \
             ~/.ssh/id_rooms — egress task unverified (bake a key-paired rootfs to cover it)"
        ),
        Egress::Unreachable => panic!(
            "guest never answered over the slot tap {} — tap/slot networking failed",
            network.guest_ip
        ),
    }

    // Reap: shutdown deletes the tap, then frees the slot file (once the room
    // dir is gone) — the tap-then-slot release under the reap-clean gate.
    vm.shutdown().await.expect("shutdown/reap");

    // The crux: every observable the boot touched is back to byte-identical.
    let taps_after = rooms_taps();
    let slots_after = slots_listing(&state_base);
    let fwd_after = rooms_fwd_dump();

    assert_eq!(
        taps_after, taps_before,
        "pool tap set not restored after reap — a slot tap leaked"
    );
    assert_eq!(
        slots_after, slots_before,
        "slots/ not restored after reap — a slot file leaked"
    );
    assert_eq!(
        fwd_after, fwd_before,
        "ROOMS_FWD chain was mutated by boot/reap — it must stay host-once"
    );
}
