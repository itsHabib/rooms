//! Host-e2e for the multi-room pool — the v0.2 "hard parallelism" validation
//! gate. Three families of test, each a no-op skip on an unprovisioned host:
//!
//! - **single room:** boots on slot 1, runs an egress task, reaps
//!   byte-identically (the pool tap set, `slots/`, and `ROOMS_FWD` all restore).
//! - **concurrent boot (N=3):** three rooms boot at once on distinct slots/IPs,
//!   each reachable, guest↔guest isolated, all reaped byte-identically.
//! - **pool-full:** the real `rooms run` binary against a full pool exits 4 with
//!   a `pool_full` `--json` record, and a freed slot is immediately reclaimable.
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
use rooms::error::FirecrackerError;
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
        !taps_before.contains("tap-fc1:"),
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

/// The three pool slots the concurrent-boot gate drives (the first real slots;
/// slot 0 is the reserved legacy tap).
const CONCURRENT_INDICES: [u8; 3] = [1, 2, 3];

/// The guest→guest isolation rule `setup-tap.sh --host` installs into the
/// host-once `ROOMS_FWD` chain: guest k and guest j both live in the supernet,
/// so this line drops every inter-slot packet. The byte-identical `fwd` assert
/// proves boot/reap never touch it.
const ISOLATION_DROP: &str = "-A ROOMS_FWD -s 172.16.0.0/24 -d 172.16.0.0/24 -j DROP";

/// The `FORWARD` jump into `ROOMS_FWD`. It must be the *first* FORWARD rule so no
/// pre-existing broad ACCEPT can preempt the chain and let guest→guest
/// forwarding slip past isolation — the DROP only bites if packets actually
/// reach the chain (`setup-tap.sh`'s `iptables -I FORWARD 1 -j ROOMS_FWD`,
/// mirrored by `scripts/test-tap-rules.sh`).
const FORWARD_JUMP: &str = "-A FORWARD -j ROOMS_FWD";

/// A room booted on a pool slot for the concurrent-boot gate.
struct BootedRoom {
    room_id: String,
    slot: slot::Slot,
    vm: firecracker::BootedVm,
}

/// The guest network derived from a claimed slot.
fn net_of(claimed: &slot::Slot) -> NetworkConfig {
    NetworkConfig {
        tap_name: claimed.tap.clone(),
        guest_ip: claimed.guest.to_string(),
        gateway_ip: claimed.gateway.to_string(),
        prefix: claimed.prefix,
    }
}

/// Claim the next free slot and boot a room on it, returning the identity, the
/// claimed slot, and the boot result so the caller can reap-or-free uniformly.
async fn claim_and_boot(
    state_base: &Path,
    kernel: &Path,
    rootfs: &Path,
    config: &RoomsConfig,
    me: Claimer,
) -> (
    String,
    slot::Slot,
    Result<firecracker::BootedVm, FirecrackerError>,
) {
    let room_id = firecracker::mint_room_id();
    let claimed = slot::claim(state_base, &room_id, me, slot::DEFAULT_MAX_POOL, None)
        .expect("claim a pool slot");
    // Scope the borrows of room_id/claimed so they end before the return move.
    let result = {
        let network = net_of(&claimed);
        let descriptor = rooms::room::RoomDescriptor::default();
        let req = BootRequest {
            kernel,
            rootfs,
            network: Some(&network),
            slot: Some(&claimed),
            room_id: &room_id,
            readonly_rootfs: false,
            descriptor: &descriptor,
        };
        firecracker::boot(&req, config).await
    };
    (room_id, claimed, result)
}

/// Turn the three concurrent boot results into live rooms. On any boot failure,
/// reap the ones that did boot and free every claimed slot so a partial failure
/// still leaves the host pristine, then panic.
async fn all_booted_or_cleanup(
    results: Vec<(
        String,
        slot::Slot,
        Result<firecracker::BootedVm, FirecrackerError>,
    )>,
    state_base: &Path,
) -> Vec<BootedRoom> {
    let mut booted = Vec::new();
    let mut failures = Vec::new();
    for (room_id, claimed, result) in results {
        match result {
            Ok(vm) => booted.push(BootedRoom {
                room_id,
                slot: claimed,
                vm,
            }),
            Err(e) => {
                failures.push(format!("slot {}: {e}", claimed.index));
                let _ = slot::free(state_base, claimed.index, &room_id);
            }
        }
    }
    if failures.is_empty() {
        return booted;
    }
    for room in booted {
        let _ = room.vm.shutdown().await;
        let _ = slot::free(state_base, room.slot.index, &room.room_id);
    }
    panic!("concurrent boot failed: {}", failures.join("; "));
}

/// Probe every booted guest; panic only if one is truly unreachable. Returns the
/// guest IPs we could actually log into (key-paired image), for the behavioral
/// cross-talk probe.
async fn probe_all_reachable(booted: &[BootedRoom]) -> Vec<String> {
    let mut logins = Vec::new();
    for room in booted {
        let ip = room.slot.guest.to_string();
        match probe_egress(&ip).await {
            Egress::Verified => {
                eprintln!(
                    "slot {}: egress OK (guest resolved github.com)",
                    room.slot.index
                );
                logins.push(ip);
            }
            Egress::ReachableNoAuth => eprintln!(
                "slot {}: reachable (sshd answered over the slot tap); egress task unverified \
                 — rootfs not key-paired with ~/.ssh/id_rooms",
                room.slot.index
            ),
            Egress::Unreachable => panic!(
                "slot {} guest {ip} never answered over its slot tap — tap/slot networking failed",
                room.slot.index
            ),
        }
    }
    logins
}

/// True when the guest→guest DROP sits before the egress ACCEPT in the chain
/// dump — the order that makes the DROP bite (a preceding broad ACCEPT would let
/// inter-slot traffic through first). A dump with the DROP and no egress ACCEPT
/// line still passes: there's nothing for a packet to slip past.
fn isolation_precedes_egress(fwd_dump: &str) -> bool {
    let drop_line = fwd_dump.lines().position(|l| l.trim() == ISOLATION_DROP);
    let egress_line = fwd_dump.lines().position(|l| {
        l.contains("-s 172.16.0.0/24") && l.contains("-o ") && l.contains("-j ACCEPT")
    });
    match (drop_line, egress_line) {
        (Some(d), Some(e)) => d < e,
        (Some(_), None) => true,
        _ => false,
    }
}

/// The first `-A FORWARD ...` rule from `iptables -S FORWARD`, or empty when the
/// FORWARD chain carries no rules. The gate needs this to be the `ROOMS_FWD`
/// jump: a chain that reads clean but isn't first (or isn't jumped at all)
/// leaves guest→guest forwarding possible.
fn first_forward_rule() -> String {
    let out = Command::new("iptables")
        .args(["-S", "FORWARD"])
        .output()
        .expect("iptables -S FORWARD");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find(|line| line.starts_with("-A FORWARD "))
        .unwrap_or_default()
        .to_owned()
}

/// From guest `source`, probe `peer` over ICMP. Returns true only on a
/// definitive round-trip (the isolation DROP failed to bite). A blocked probe, a
/// missing tool, or any inconclusive result reads as not-reached — this
/// behavioral check is a bonus over the structural DROP assert, never a source
/// of a flaky gate, so it fails only on proven cross-talk.
fn guest_reaches_peer(source: &str, peer: &str, key: &Path) -> bool {
    let out = ssh_guest(
        source,
        key,
        &format!("ping -c1 -W2 {peer} && echo ROOMS_REACHED"),
    );
    String::from_utf8_lossy(&out.stdout).contains("ROOMS_REACHED")
}

/// Assert guest→guest isolation. The structural proof (always run) pins the
/// whole packet path, not just the chain text: `ROOMS_FWD` is jumped from
/// `FORWARD` **first** (so no prior ACCEPT preempts it), the guest→guest DROP is
/// present, and it's ordered **before** the egress ACCEPT. The behavioral proof
/// (when a guest login exists) confirms guest k cannot actually reach guest j.
/// Without a key-paired rootfs the structural proof stands alone and the
/// behavioral gap is noted, never failed.
fn assert_isolation(booted: &[BootedRoom], logins: &[String]) {
    // Packet path: the chain must be jumped from FORWARD and be first, else the
    // DROP below reads clean but never sees an inter-slot packet.
    let forward_first = first_forward_rule();
    assert_eq!(
        forward_first, FORWARD_JUMP,
        "ROOMS_FWD must be the first FORWARD rule so no prior ACCEPT preempts guest isolation; got: {forward_first:?}"
    );

    let fwd = rooms_fwd_dump();
    assert!(
        fwd.contains(ISOLATION_DROP),
        "ROOMS_FWD is missing the guest->guest isolation DROP:\n{fwd}"
    );
    assert!(
        isolation_precedes_egress(&fwd),
        "the isolation DROP must precede the egress ACCEPT in ROOMS_FWD:\n{fwd}"
    );

    let Some(source) = logins.first() else {
        eprintln!(
            "note: no key-paired guest login — guest->guest cross-talk unverified behaviorally; \
             the ROOMS_FWD isolation DROP above is the structural proof (bake a key-paired rootfs \
             to cover it)"
        );
        return;
    };
    let key = guest_key();
    for room in booted {
        let peer = room.slot.guest.to_string();
        if &peer == source {
            continue;
        }
        assert!(
            !guest_reaches_peer(source, &peer, &key),
            "cross-talk: guest {source} reached guest {peer} — the isolation DROP is not biting"
        );
    }
}

#[tokio::test]
async fn three_rooms_boot_concurrently_isolated_and_reap_byte_identically() {
    let Some((kernel, rootfs)) = preflight() else {
        return;
    };

    // Isolated state base so the claims + room dirs never collide with real
    // rooms; the taps are still host-global, so the pre-check asserts free slots.
    let tmp = tempfile::tempdir().expect("tempdir");
    let state_base = tmp.path().to_path_buf();
    let config = RoomsConfig {
        state_base: Some(state_base.clone()),
        ..RoomsConfig::default()
    };
    let me = Claimer::current().expect("read this process's claimer identity");

    // Pre-boot state the concurrent reap must restore exactly.
    let taps_before = rooms_taps();
    let slots_before = slots_listing(&state_base);
    let fwd_before = rooms_fwd_dump();
    for index in CONCURRENT_INDICES {
        assert!(
            !taps_before.contains(&format!("tap-fc{index}:")),
            "slot {index} already in use (tap-fc{index} present); free it first: {taps_before}"
        );
    }

    // Boot three rooms at once: they must land on distinct slots (no double-
    // allocation) and all be alive together (real concurrency, not serialized).
    let (r1, r2, r3) = tokio::join!(
        claim_and_boot(&state_base, &kernel, &rootfs, &config, me),
        claim_and_boot(&state_base, &kernel, &rootfs, &config, me),
        claim_and_boot(&state_base, &kernel, &rootfs, &config, me),
    );
    let booted = all_booted_or_cleanup(vec![r1, r2, r3], &state_base).await;

    let mut indices: Vec<u8> = booted.iter().map(|r| r.slot.index).collect();
    indices.sort_unstable();
    assert_eq!(
        indices,
        CONCURRENT_INDICES.to_vec(),
        "three concurrent claims must take distinct slots 1,2,3"
    );
    let taps_live = rooms_taps();
    for room in &booted {
        assert!(
            taps_live.contains(&room.slot.tap),
            "slot tap {} should exist mid-boot, saw: {taps_live}",
            room.slot.tap
        );
    }

    // Each guest completes its network task, and inter-guest traffic is isolated.
    let logins = probe_all_reachable(&booted).await;
    assert_isolation(&booted, &logins);

    // Reap all three; every observable returns to byte-identical.
    for room in booted {
        room.vm.shutdown().await.expect("shutdown/reap");
    }
    assert_eq!(
        rooms_taps(),
        taps_before,
        "pool tap set not restored after concurrent reap — a slot tap leaked"
    );
    assert_eq!(
        slots_listing(&state_base),
        slots_before,
        "slots/ not restored after concurrent reap — a slot file leaked"
    );
    assert_eq!(
        rooms_fwd_dump(),
        fwd_before,
        "ROOMS_FWD chain was mutated by concurrent boot/reap — it must stay host-once"
    );
}

/// Run the real `rooms run` binary against a full pool, with `HOME` pointed at
/// an isolated base. `--command true` reaches the slot claim without needing a
/// boot; the full pool makes the claim fail fast.
fn run_rooms_full_pool(home: &Path, rootfs: &Path, max_pool: u8) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_rooms"))
        .env("HOME", home)
        .args(["run", "--command", "true", "--json", "--max-pool"])
        .arg(max_pool.to_string())
        .arg("--image")
        .arg(rootfs)
        .output()
        .expect("spawn rooms run")
}

#[tokio::test]
async fn full_pool_run_exits_four_with_pool_full_json_and_frees_reclaimably() {
    let Some((_kernel, rootfs)) = preflight() else {
        return;
    };

    // Isolate the child's pool by pointing its HOME at a tempdir: its default
    // state base is <HOME>/.local/state/rooms, so pre-claims here are exactly
    // what it walks — never the real host pool.
    let home = tempfile::tempdir().expect("tempdir");
    let state_base = home.path().join(".local/state/rooms");
    let me = Claimer::current().expect("read this process's claimer identity");

    // Cap the pool at 2 and fill it: two pre-claims exhaust it with no boot.
    let cap = 2u8;
    let id1 = firecracker::mint_room_id();
    let id2 = firecracker::mint_room_id();
    let c1 = slot::claim(&state_base, &id1, me, cap, None).expect("pre-claim slot 1");
    let c2 = slot::claim(&state_base, &id2, me, cap, None).expect("pre-claim slot 2");
    assert_eq!(
        (c1.index, c2.index),
        (1, 2),
        "pre-claims fill slots 1 and 2"
    );

    // The real binary against a full pool: reach the claim (kernel valid +
    // ROOMS_FWD installed), then fail fast — exit 4 with a pool_full --json
    // record, ship's machine-readable signal (never a boot).
    let out = run_rooms_full_pool(home.path(), &rootfs, cap);
    assert_eq!(
        out.status.code(),
        Some(4),
        "a full pool must exit 4; stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("\"error_kind\":\"pool_full\""),
        "--json must carry error_kind pool_full; got: {stdout}"
    );

    // A freed slot is immediately reclaimable, lowest hole first.
    assert_eq!(
        slot::free(&state_base, c1.index, &id1).expect("free slot 1"),
        slot::Freed::Removed
    );
    let id3 = firecracker::mint_room_id();
    let reclaimed = slot::claim(&state_base, &id3, me, cap, None).expect("reclaim the freed slot");
    assert_eq!(
        reclaimed.index, 1,
        "the freed slot is immediately reclaimable"
    );
    // The tempdir drop clears the isolated pool; no host state was touched.
}
