//! Firecracker process + API control.
//!
//! POC: shells out to `firecracker` and `curl --unix-socket` for API calls.
//! A proper HTTP-over-Unix-socket client lands in task #2 (`harden-firecracker-control`).

use std::env;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::process::{Child, Command};
use tokio::time::sleep;
use tracing::{debug, info, warn};
use ulid::Ulid;

/// Network configuration for a microVM.
///
/// The TAP device named by `tap_name` must already exist on the host (the
/// POC ships `scripts/setup-tap.sh` to create the conventional `tap-fc0`).
/// The guest IP is plumbed via the Linux kernel's built-in IP autoconfig
/// (`boot_args` `ip=...`), so no DHCP / systemd-networkd / `/etc/network`
/// fiddling is needed inside the rootfs.
pub struct NetworkConfig {
    /// TAP device name on the host (e.g. `"tap-fc0"`).
    pub tap_name: String,
    /// IP address the guest's eth0 takes (e.g. `"172.16.0.2"`).
    pub guest_ip: String,
    /// Gateway IP — the host-side TAP IP (e.g. `"172.16.0.1"`).
    pub gateway_ip: String,
    /// Netmask in dotted form (e.g. `"255.255.255.0"`).
    pub netmask: String,
}

/// Unique identifier for a room's on-disk state directory.
#[derive(Debug, Clone, Copy)]
pub struct RoomId(Ulid);

impl RoomId {
    fn new() -> Self {
        Self(Ulid::new())
    }
}

/// A booted Firecracker microVM. Dropping the handle kills the process.
pub struct BootedVm {
    // Field order matters: `child` drops first so `kill_on_drop` fires before
    // any cleanup that might reference `room_dir`.
    child: Child,
    socket: PathBuf,
    room_dir: PathBuf,
}

impl BootedVm {
    /// Truly best-effort: terminate the firecracker process and remove room
    /// state. Continues cleanup even if `kill` fails — process may have
    /// already exited, and we still want the socket file + per-room dir gone.
    /// (Reviewer feedback on PR #1: earlier `?` on kill could leak the dir.)
    pub async fn shutdown(mut self) -> Result<()> {
        // SIGKILL is fine for the POC; SIGTERM-then-SIGKILL with grace is
        // a task #2 concern.
        if let Err(e) = self.child.kill().await {
            // Don't bail — process may have already exited (expected) or
            // failed to die for a non-fatal reason. Log so the operator
            // can investigate stray firecrackers, then proceed with file
            // cleanup. (Reviewer PR #1 round 2: prior `let _ = kill()`
            // hid real failures.)
            warn!(error = %e, "failed to kill firecracker child; continuing cleanup");
        }
        if self.socket.exists() {
            tokio::fs::remove_file(&self.socket).await.ok();
        }
        tokio::fs::remove_dir_all(&self.room_dir).await.ok();
        Ok(())
    }

    /// Returns true if the firecracker process is still running.
    pub fn is_alive(&mut self) -> Result<bool> {
        Ok(self.child.try_wait().context("try_wait failed")?.is_none())
    }
}

struct RoomDirGuard {
    path: PathBuf,
    dismiss: bool,
}

impl Drop for RoomDirGuard {
    fn drop(&mut self) {
        if !self.dismiss {
            // Drop is sync; async drop isn't stable — blocking cleanup is fine here.
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

/// Boot a Firecracker microVM with the given kernel + rootfs, optionally
/// attaching a network interface.
///
/// POC: minimal config — 1 vCPU, 256 MiB. Caller is responsible for invoking
/// [`BootedVm::shutdown`] when done.
#[allow(
    clippy::too_many_lines,
    reason = "POC: cohesive boot orchestrator. Splitting into prepare_dir / spawn / configure / start helpers belongs in task #2 (harden-firecracker-control) alongside the structured-error refactor."
)]
pub async fn boot(
    kernel: &Path,
    rootfs: &Path,
    network: Option<&NetworkConfig>,
) -> Result<BootedVm> {
    let room_id = RoomId::new();
    let home = env::var("HOME").context("failed to read HOME env var")?;
    let per_room_dir = PathBuf::from(home)
        .join(".local/state/rooms")
        .join(room_id.0.to_string().to_lowercase());

    // Ensure parent dir exists (not security-critical; ~/.local/state/rooms
    // uses HOME's mode). Recursive create is safe here — no TOCTOU concern
    // because the parent isn't the secret-holding leaf.
    if let Some(parent) = per_room_dir.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .context("failed to create rooms parent dir")?;
    }

    // Atomically create the leaf dir WITH mode 0700 — no TOCTOU window
    // between create + chmod (reviewer feedback PR #1 from Copilot). Uses
    // spawn_blocking because std::fs::DirBuilder is sync.
    let leaf = per_room_dir.clone();
    tokio::task::spawn_blocking(move || {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().mode(0o700).create(&leaf)
    })
    .await
    .context("spawn_blocking for dir create failed (panic or cancellation)")?
    .context("failed to create per-room state dir with mode 0700")?;

    // Construct the cleanup guard IMMEDIATELY after the dir exists so any
    // subsequent failure path is caught by Drop. (Reviewer feedback PR #1
    // from Codex: earlier order constructed the guard after set_permissions,
    // leaking the dir if set_permissions failed.)
    let mut guard = RoomDirGuard {
        path: per_room_dir.clone(),
        dismiss: false,
    };
    let socket = per_room_dir.join("api.sock");
    let log_path = per_room_dir.join("firecracker.log");

    // Route firecracker's own logs AND the guest serial console (kernel boot
    // log via `console=ttyS0`) into a per-room log file, so `rooms run
    // --command '...' | jq` doesn't see kernel boot output interleaved with
    // the guest command's stdout. The log file is cleaned up with the room dir
    // on shutdown; operators who need to debug a hung boot can `--keep` and
    // tail the file.
    let log_file = std::fs::File::create(&log_path)
        .with_context(|| format!("create firecracker log file at {}", log_path.display()))?;
    let log_file_stderr = log_file
        .try_clone()
        .context("clone firecracker log file handle for stderr")?;

    info!(socket = %socket.display(), log = %log_path.display(), "spawning firecracker");
    let child = Command::new("firecracker")
        .arg("--api-sock")
        .arg(&socket)
        .stdout(std::process::Stdio::from(log_file))
        .stderr(std::process::Stdio::from(log_file_stderr))
        .kill_on_drop(true)
        .spawn()
        .context("failed to spawn firecracker; is it on PATH?")?;

    wait_for_socket(&socket, Duration::from_secs(5)).await?;

    // Kernel cmdline: when networking is requested, append Linux's built-in IP
    // autoconfig string (`ip=<client>::<gw>:<mask>::<dev>:<autoconf>`) so eth0
    // comes up before userspace, with no DHCP needed in the rootfs.
    let boot_args = network.map_or_else(
        || String::from("console=ttyS0 reboot=k panic=1 pci=off"),
        |net| {
            format!(
                "console=ttyS0 reboot=k panic=1 pci=off ip={}::{}:{}::eth0:off",
                net.guest_ip, net.gateway_ip, net.netmask
            )
        },
    );

    api_put(
        &socket,
        "/boot-source",
        &serde_json::json!({
            "kernel_image_path": kernel,
            "boot_args": boot_args,
        }),
    )
    .await
    .context("PUT /boot-source")?;

    api_put(
        &socket,
        "/drives/rootfs",
        &serde_json::json!({
            "drive_id": "rootfs",
            "path_on_host": rootfs,
            "is_root_device": true,
            "is_read_only": false,
        }),
    )
    .await
    .context("PUT /drives/rootfs")?;

    api_put(
        &socket,
        "/machine-config",
        &serde_json::json!({
            "vcpu_count": 1,
            "mem_size_mib": 256,
        }),
    )
    .await
    .context("PUT /machine-config")?;

    if let Some(net) = network {
        // Firecracker auto-generates the guest MAC if we don't supply one.
        api_put(
            &socket,
            "/network-interfaces/eth0",
            &serde_json::json!({
                "iface_id": "eth0",
                "host_dev_name": net.tap_name,
            }),
        )
        .await
        .context("PUT /network-interfaces/eth0")?;
        info!(tap = %net.tap_name, guest_ip = %net.guest_ip, "network attached");
    }

    api_put(
        &socket,
        "/actions",
        &serde_json::json!({
            "action_type": "InstanceStart",
        }),
    )
    .await
    .context("PUT /actions (InstanceStart)")?;

    info!("microVM booted");
    guard.dismiss = true;
    Ok(BootedVm {
        child,
        socket,
        room_dir: per_room_dir,
    })
}

async fn wait_for_socket(socket: &Path, timeout: Duration) -> Result<()> {
    let start = Instant::now();
    let mut last_err: Option<std::io::Error> = None;
    while start.elapsed() < timeout {
        match tokio::net::UnixStream::connect(socket).await {
            Ok(_) => {
                // Connection succeeded → Firecracker has listen()ed.
                // Drop the stream immediately; next API call opens a fresh one.
                debug!("api socket accepting connections");
                return Ok(());
            }
            Err(e) => last_err = Some(e),
        }
        sleep(Duration::from_millis(50)).await;
    }
    // Surface the last connect error so failures don't look like a generic
    // timeout (reviewer feedback PR #1 from Copilot: permission errors and
    // similar would otherwise be hidden).
    let detail = last_err.map_or_else(
        || String::from("no connect attempts completed"),
        |e| format!("last error: {e}"),
    );
    anyhow::bail!(
        "firecracker api socket at {} did not accept connections within {:?} ({detail})",
        socket.display(),
        timeout,
    )
}

async fn api_put(socket: &Path, endpoint: &str, body: &serde_json::Value) -> Result<()> {
    let body_str = serde_json::to_string(body)?;
    debug!(endpoint, body = %body_str, "PUT");
    let output = Command::new("curl")
        .arg("--unix-socket")
        .arg(socket)
        .arg("-X")
        .arg("PUT")
        .arg(format!("http://localhost{endpoint}"))
        .arg("-H")
        .arg("Content-Type: application/json")
        .arg("-d")
        .arg(&body_str)
        .arg("--fail-with-body")
        .arg("--silent")
        .arg("--show-error")
        .output()
        .await
        .context("curl invocation failed")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        anyhow::bail!(
            "api PUT {endpoint} failed (exit {}): stderr={stderr}, stdout={stdout}",
            output.status,
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        reason = "test module: panicky lints are noise in tests"
    )]

    use std::time::Duration;

    use super::{wait_for_socket, RoomDirGuard};
    use tokio::net::UnixListener;

    #[test]
    fn room_dir_guard_cleans_up_on_drop_when_not_dismissed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().to_path_buf();
        std::fs::write(path.join("marker"), b"x").expect("write marker");

        drop(RoomDirGuard {
            path: path.clone(),
            dismiss: false,
        });

        assert!(!path.exists(), "guard should remove the directory");
    }

    #[test]
    fn room_dir_guard_preserves_dir_when_dismissed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().to_path_buf();

        drop(RoomDirGuard {
            path: path.clone(),
            dismiss: true,
        });

        assert!(path.exists(), "dismissed guard should leave the directory");
    }

    #[tokio::test]
    async fn wait_for_socket_requires_listener_not_just_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("fake.sock");
        tokio::fs::File::create(&socket_path)
            .await
            .expect("create fake socket file");

        let err = wait_for_socket(&socket_path, Duration::from_millis(300))
            .await
            .expect_err("file without listener should time out");
        assert!(
            err.to_string().contains("did not accept connections"),
            "unexpected error: {err}"
        );

        // Remove the fake file before binding — UnixListener::bind refuses
        // to overwrite an existing path.
        tokio::fs::remove_file(&socket_path)
            .await
            .expect("remove fake socket file");
        let _listener = UnixListener::bind(&socket_path).expect("bind listener");
        wait_for_socket(&socket_path, Duration::from_millis(300))
            .await
            .expect("listening socket should be ready");
    }
}

#[cfg(all(test, feature = "e2e"))]
mod e2e_tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        reason = "test module: panicky lints are noise in tests"
    )]

    use std::path::PathBuf;
    use std::time::Duration;

    use super::boot;

    fn image_path(name: &str) -> PathBuf {
        let home = std::env::var("HOME").expect("HOME env var must be set");
        PathBuf::from(home).join("rooms/images").join(name)
    }

    #[tokio::test]
    async fn firecracker_boots_and_survives_briefly() {
        let kernel = image_path("vmlinux.bin");
        let rootfs = image_path("rootfs.ext4");

        assert!(
            kernel.exists(),
            "kernel missing at {kernel:?} — run scripts/setup-rooms-host.sh"
        );
        assert!(
            rootfs.exists(),
            "rootfs missing at {rootfs:?} — run scripts/setup-rooms-host.sh"
        );

        // e2e smoke test runs without networking — proves the no-net path
        // still works after the NetworkConfig refactor.
        let mut vm = boot(&kernel, &rootfs, None)
            .await
            .expect("boot should succeed");

        // Give the guest kernel + init a moment to come up.
        tokio::time::sleep(Duration::from_secs(3)).await;

        assert!(
            vm.is_alive().expect("is_alive probe"),
            "firecracker exited prematurely — check serial console output"
        );

        vm.shutdown().await.expect("shutdown should succeed");
    }
}
