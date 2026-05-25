//! Failure-injection tests for the Firecracker control plane.
//!
//! Gated behind the `e2e` feature — requires Linux + KVM for the `guest_unreachable` test;
//! the stub-binary tests only need a Unix host.

#![cfg(all(unix, feature = "e2e"))]

use std::path::{Path, PathBuf};
use std::time::Duration;

use rooms::config::RoomsConfig;
use rooms::error::{FirecrackerError, RoomsError};
use rooms::firecracker;
use rooms::runner;

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
    let stub = fixture_path("firecracker_exit2.sh");
    assert!(stub.exists(), "missing fixture {}", stub.display());

    let kernel = image_path("vmlinux.bin");
    let rootfs = image_path("rootfs.ext4");
    if !kernel.exists() || !rootfs.exists() {
        eprintln!("skipping: images not present");
        return;
    }

    let mut config = RoomsConfig::default();
    config.firecracker_binary = stub;
    config.api_socket_timeout = Duration::from_secs(2);

    let before = room_dirs_glob();
    let err = firecracker::boot(&kernel, &rootfs, None, &config)
        .await
        .expect_err("stub should exit early");

    match &err {
        FirecrackerError::ProcessExitedEarly { exit_code, .. } => {
            assert_eq!(*exit_code, 2);
        }
        other => panic!("expected ProcessExitedEarly, got {other}"),
    }

    if before.exists() {
        assert!(
            latest_room_dir(&before).is_none()
                || latest_room_dir(&before).is_some_and(|d| !d.exists()),
            "room work dir should be cleaned up"
        );
    }
}

#[tokio::test]
async fn api_socket_never_appears() {
    let stub = fixture_path("firecracker_no_socket.sh");
    assert!(stub.exists(), "missing fixture {}", stub.display());

    let kernel = image_path("vmlinux.bin");
    let rootfs = image_path("rootfs.ext4");
    if !kernel.exists() || !rootfs.exists() {
        eprintln!("skipping: images not present");
        return;
    }

    let mut config = RoomsConfig::default();
    config.firecracker_binary = stub;
    config.api_socket_timeout = Duration::from_secs(2);

    let err = firecracker::boot(&kernel, &rootfs, None, &config)
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
    let kernel = image_path("vmlinux.bin");
    let rootfs = image_path("rootfs.ext4");
    if !kernel.exists() || !rootfs.exists() {
        eprintln!("skipping: images not present");
        return;
    }

    let mut config = RoomsConfig::default();
    config.guest_reach_timeout = Duration::from_secs(5);
    config.guest_reach_poll_interval = Duration::from_secs(1);

    // Boot without network so SSH can never succeed.
    let mut vm = firecracker::boot(&kernel, &rootfs, None, &config)
        .await
        .expect("boot without network should succeed");

    let key = PathBuf::from(std::env::var("HOME").expect("HOME").to_string() + "/.ssh/id_rooms");

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
