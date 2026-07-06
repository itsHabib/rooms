//! Failure-injection tests for the Firecracker control plane.
//!
//! Gated behind the `e2e` feature — requires Linux + KVM for the `guest_unreachable` test;
//! the stub-binary tests only need a Unix host.

#![cfg(all(unix, feature = "e2e"))]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test module: panicky lints are noise in tests"
)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use rooms::config::RoomsConfig;
use rooms::error::{FirecrackerError, RoomsError};
use rooms::firecracker;
use rooms::runner;
use tokio::sync::Mutex;

// All three tests boot a microVM and create a room dir under
// `~/.local/state/rooms/`. `firecracker_exits_early_is_caught` walks that
// dir at the end and asserts the latest entry has been cleaned up — when
// run in parallel, that "latest" can be a sibling test's still-in-flight
// room dir, falsely failing the cleanup-leak check. Serializing all three
// guarantees the assertion only sees its own state. `--test-threads=1`
// already proves serialization is sufficient; production cleanup
// (`RoomGuard`) is correct.
//
// `tokio::sync::Mutex::const_new` keeps the guard `Send` across `.await`
// (no `clippy::await_holding_lock` trip). The `_serial` binding name is
// load-bearing: `let _ = SERIAL.lock().await` would drop the guard
// immediately and defeat the lock; the leading underscore + name keeps
// it alive to end of scope while suppressing `unused_variables`.
static SERIAL: Mutex<()> = Mutex::const_new(());

fn image_path(name: &str) -> PathBuf {
    let home = std::env::var("HOME").expect("HOME");
    PathBuf::from(home).join("rooms/images").join(name)
}

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn room_dirs_glob() -> PathBuf {
    let home = std::env::var("HOME").expect("HOME");
    PathBuf::from(home).join(".local/state/rooms")
}

fn latest_room_dir(base: &Path) -> Option<PathBuf> {
    let mut dirs: Vec<_> = std::fs::read_dir(base)
        .ok()?
        .filter_map(Result::ok)
        .filter(|e| e.path().is_dir())
        .map(|e| e.path())
        .collect();
    dirs.sort();
    dirs.pop()
}

#[tokio::test]
async fn firecracker_exits_early_is_caught() {
    let _serial = SERIAL.lock().await;
    let stub = fixture_path("firecracker_exit2.sh");
    assert!(stub.exists(), "missing fixture {}", stub.display());

    let kernel = image_path("vmlinux.bin");
    let rootfs = image_path("rootfs.ext4");
    if !kernel.exists() || !rootfs.exists() {
        eprintln!("skipping: images not present");
        return;
    }

    let config = RoomsConfig {
        firecracker_binary: stub,
        api_socket_timeout: Duration::from_secs(2),
        ..RoomsConfig::default()
    };

    let before = room_dirs_glob();
    let id = firecracker::mint_room_id();
    let descriptor = rooms::room::RoomDescriptor::default();
    let req = firecracker::BootRequest {
        kernel: &kernel,
        rootfs: &rootfs,
        network: None,
        slot: None,
        room_id: &id,
        readonly_rootfs: false,
        descriptor: &descriptor,
    };
    let err = firecracker::boot(&req, &config)
        .await
        .expect_err("stub should exit early");

    match &err {
        FirecrackerError::ProcessExitedEarly { exit_code, .. } => {
            assert_eq!(*exit_code, 2);
        }
        other => panic!("expected ProcessExitedEarly, got {other}"),
    }

    if before.exists() {
        // is_none_or = "no room dirs at all" OR "the latest one was already cleaned up"
        assert!(
            latest_room_dir(&before).is_none_or(|d| !d.exists()),
            "room work dir should be cleaned up"
        );
    }
}

#[tokio::test]
async fn api_socket_never_appears() {
    let _serial = SERIAL.lock().await;
    let stub = fixture_path("firecracker_no_socket.sh");
    assert!(stub.exists(), "missing fixture {}", stub.display());

    let kernel = image_path("vmlinux.bin");
    let rootfs = image_path("rootfs.ext4");
    if !kernel.exists() || !rootfs.exists() {
        eprintln!("skipping: images not present");
        return;
    }

    let config = RoomsConfig {
        firecracker_binary: stub,
        api_socket_timeout: Duration::from_secs(2),
        ..RoomsConfig::default()
    };

    let id = firecracker::mint_room_id();
    let descriptor = rooms::room::RoomDescriptor::default();
    let req = firecracker::BootRequest {
        kernel: &kernel,
        rootfs: &rootfs,
        network: None,
        slot: None,
        room_id: &id,
        readonly_rootfs: false,
        descriptor: &descriptor,
    };
    let err = firecracker::boot(&req, &config)
        .await
        .expect_err("stub should never open socket");

    match &err {
        FirecrackerError::ApiSocketNeverAppeared { timeout_ms } => {
            assert!(*timeout_ms >= 2000);
        }
        other => panic!("expected ApiSocketNeverAppeared, got {other}"),
    }
}

#[tokio::test]
async fn guest_unreachable() {
    let _serial = SERIAL.lock().await;
    let kernel = image_path("vmlinux.bin");
    let rootfs = image_path("rootfs.ext4");
    if !kernel.exists() || !rootfs.exists() {
        eprintln!("skipping: images not present");
        return;
    }

    let config = RoomsConfig {
        guest_reach_timeout: Duration::from_secs(5),
        guest_reach_poll_interval: Duration::from_secs(1),
        ..RoomsConfig::default()
    };

    // Boot without network so SSH can never succeed.
    let id = firecracker::mint_room_id();
    let descriptor = rooms::room::RoomDescriptor::default();
    let req = firecracker::BootRequest {
        kernel: &kernel,
        rootfs: &rootfs,
        network: None,
        slot: None,
        room_id: &id,
        readonly_rootfs: false,
        descriptor: &descriptor,
    };
    let vm = firecracker::boot(&req, &config)
        .await
        .expect("boot without network should succeed");

    let key = PathBuf::from(std::env::var("HOME").expect("HOME") + "/.ssh/id_rooms");

    let err = runner::wait_for_ssh("172.16.0.2", &key, &config)
        .await
        .expect_err("SSH should fail without network");

    let rooms_err = RoomsError::Firecracker(err);
    match rooms_err {
        RoomsError::Firecracker(FirecrackerError::GuestUnreachable { .. }) => {}
        other => panic!("expected GuestUnreachable, got {other}"),
    }

    vm.shutdown().await.expect("shutdown");
}
