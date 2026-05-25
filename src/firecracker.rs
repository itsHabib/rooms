//! Firecracker process + API control.

#![allow(
    clippy::missing_const_for_fn,
    reason = "many helpers include cfg-gated non-const bodies"
)]

use std::env;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::process::{Child, Command};
use tracing::{debug, info, warn};
use ulid::Ulid;

use crate::config::RoomsConfig;
use crate::error::FirecrackerError;
use crate::transport;

/// Network configuration for a microVM.
pub struct NetworkConfig {
    pub tap_name: String,
    pub guest_ip: String,
    pub gateway_ip: String,
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

/// RAII guard that cleans up room resources on drop or explicit shutdown.
pub struct RoomGuard {
    room_dir: PathBuf,
    socket: PathBuf,
    child_pid: Option<u32>,
    tap_name: Option<String>,
    tap_owned: bool,
    suppress_cleanup: bool,
    dismiss: bool,
    cleanup_grace: Duration,
}

impl RoomGuard {
    fn new(room_dir: PathBuf, socket: PathBuf, config: &RoomsConfig) -> Self {
        Self {
            room_dir,
            socket,
            child_pid: None,
            tap_name: None,
            tap_owned: false,
            suppress_cleanup: false,
            dismiss: false,
            cleanup_grace: config.cleanup_grace,
        }
    }

    fn set_child(&mut self, child: &Child) {
        self.child_pid = child.id();
    }

    /// Record the TAP this room uses. Does NOT take ownership — see
    /// `set_tap_owned` for that. v0 always uses the shared host TAP
    /// (`tap-fc0`), which is managed by `scripts/setup-tap.sh` and must
    /// outlive any single room.
    fn set_tap(&mut self, tap_name: String) {
        self.tap_name = Some(tap_name);
    }

    /// Mark the recorded TAP as per-room (owned by this guard). Cleanup
    /// will `ip tuntap del` the interface on drop. Not yet wired — per-room
    /// TAPs land with the network rewrite that retires `tap-fc0`.
    #[allow(dead_code, reason = "wired by future per-room-TAP work")]
    pub fn set_tap_owned(&mut self, owned: bool) {
        self.tap_owned = owned;
    }

    /// Prevent cleanup on drop (successful handoff to caller-managed shutdown).
    pub fn dismiss(&mut self) {
        self.dismiss = true;
    }

    /// Suppress all cleanup (for `--keep` debugging).
    pub fn set_suppress_cleanup(&mut self, suppress: bool) {
        self.suppress_cleanup = suppress;
    }

    /// Explicit cleanup before dropping ownership.
    pub fn cleanup(&mut self) {
        if self.suppress_cleanup || self.dismiss {
            return;
        }
        self.cleanup_sync();
    }

    fn cleanup_sync(&mut self) {
        if self.suppress_cleanup {
            return;
        }
        debug!(room_dir = %self.room_dir.display(), "RoomGuard cleanup");
        kill_child_gracefully(self.child_pid, self.cleanup_grace);
        self.child_pid = None;
        if self.tap_owned {
            release_tap(self.tap_name.as_deref());
        }
        if self.socket.exists() {
            let _ = std::fs::remove_file(&self.socket);
        }
        let _ = std::fs::remove_dir_all(&self.room_dir);
    }
}

impl Drop for RoomGuard {
    fn drop(&mut self) {
        if self.dismiss || self.suppress_cleanup {
            debug!("RoomGuard drop skipped (dismissed or suppressed)");
            return;
        }
        debug!(room_dir = %self.room_dir.display(), "RoomGuard drop firing cleanup");
        self.cleanup_sync();
    }
}

/// A booted Firecracker microVM.
pub struct BootedVm {
    guard: RoomGuard,
    child: Child,
}

impl BootedVm {
    /// Terminate the firecracker process and remove room state.
    pub async fn shutdown(mut self) -> Result<(), FirecrackerError> {
        if !self.guard.suppress_cleanup {
            if let Err(e) = self.child.kill().await {
                warn!(error = %e, "failed to kill firecracker child; continuing cleanup");
            }
            self.guard.cleanup();
        }
        self.guard.dismiss();
        Ok(())
    }

    /// Returns true if the firecracker process is still running.
    pub fn is_alive(&mut self) -> Result<bool, FirecrackerError> {
        Ok(self
            .child
            .try_wait()
            .map_err(FirecrackerError::Io)?
            .is_none())
    }

    pub fn guard_mut(&mut self) -> &mut RoomGuard {
        &mut self.guard
    }
}

/// Boot a Firecracker microVM with the given kernel + rootfs.
pub async fn boot(
    kernel: &Path,
    rootfs: &Path,
    network: Option<&NetworkConfig>,
    config: &RoomsConfig,
) -> Result<BootedVm, FirecrackerError> {
    check_kvm()?;
    resolve_firecracker_binary(config)?;

    let room_id = RoomId::new();
    let per_room_dir = room_state_dir(&room_id)?;
    prepare_room_dir(&per_room_dir).await?;

    let socket = per_room_dir.join("api.sock");
    let log_path = per_room_dir.join("firecracker.log");
    let mut guard = RoomGuard::new(per_room_dir.clone(), socket.clone(), config);

    if let Some(net) = network {
        guard.set_tap(net.tap_name.clone());
    }

    let log_handles = open_log_file(&log_path).await?;
    let mut child = spawn_firecracker(&config.firecracker_binary, &socket, log_handles)?;
    guard.set_child(&child);

    wait_for_socket(&socket, config.api_socket_timeout, &mut child).await?;

    let boot_args = build_boot_args(network);
    configure_vm(&socket, kernel, rootfs, network, &boot_args, config).await?;

    info!("microVM booted");
    Ok(BootedVm { guard, child })
}

fn check_kvm() -> Result<(), FirecrackerError> {
    #[cfg(unix)]
    {
        let path = Path::new("/dev/kvm");
        if !path.exists() {
            return Err(FirecrackerError::KvmUnavailable);
        }
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|_| FirecrackerError::KvmUnavailable)?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        Err(FirecrackerError::KvmUnavailable)
    }
}

fn resolve_firecracker_binary(config: &RoomsConfig) -> Result<(), FirecrackerError> {
    if config.firecracker_binary.is_absolute() {
        if config.firecracker_binary.exists() {
            return Ok(());
        }
        return Err(FirecrackerError::BinaryNotFound {
            path: config.firecracker_binary.clone(),
        });
    }
    // Relative name: rely on PATH at spawn time; doctor validates separately.
    Ok(())
}

fn room_state_dir(room_id: &RoomId) -> Result<PathBuf, FirecrackerError> {
    let home = env::var("HOME").map_err(|_| FirecrackerError::HomeUnset)?;
    Ok(PathBuf::from(home)
        .join(".local/state/rooms")
        .join(room_id.0.to_string().to_lowercase()))
}

async fn prepare_room_dir(per_room_dir: &Path) -> Result<(), FirecrackerError> {
    if let Some(parent) = per_room_dir.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(FirecrackerError::Io)?;
    }

    let leaf = per_room_dir.to_path_buf();
    tokio::task::spawn_blocking(move || create_room_dir_0700(&leaf))
        .await
        .map_err(|e| FirecrackerError::Internal(format!("spawn_blocking panicked: {e}")))?
        .map_err(FirecrackerError::Io)
}

fn create_room_dir_0700(path: &Path) -> Result<(), std::io::Error> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().mode(0o700).create(path)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir(path)
    }
}

async fn open_log_file(
    log_path: &Path,
) -> Result<(std::fs::File, std::fs::File), FirecrackerError> {
    let path = log_path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let f = std::fs::File::create(&path)?;
        let f2 = f.try_clone()?;
        Ok((f, f2))
    })
    .await
    .map_err(|e| FirecrackerError::Internal(format!("spawn_blocking panicked: {e}")))?
}

fn spawn_firecracker(
    binary: &Path,
    socket: &Path,
    (log_file, log_stderr): (std::fs::File, std::fs::File),
) -> Result<Child, FirecrackerError> {
    info!(socket = %socket.display(), "spawning firecracker");
    Command::new(binary)
        .arg("--api-sock")
        .arg(socket)
        .stdout(std::process::Stdio::from(log_file))
        .stderr(std::process::Stdio::from(log_stderr))
        .kill_on_drop(true)
        .spawn()
        .map_err(|err| match err.kind() {
            std::io::ErrorKind::NotFound => FirecrackerError::BinaryNotFound {
                path: binary.to_path_buf(),
            },
            _ => FirecrackerError::Io(err),
        })
}

fn build_boot_args(network: Option<&NetworkConfig>) -> String {
    let base = "console=ttyS0 reboot=k panic=1 pci=off random.trust_cpu=on";
    network.map_or_else(
        || base.to_owned(),
        |net| {
            format!(
                "{base} ip={}::{}:{}::eth0:off",
                net.guest_ip, net.gateway_ip, net.netmask
            )
        },
    )
}

async fn configure_vm(
    socket: &Path,
    kernel: &Path,
    rootfs: &Path,
    network: Option<&NetworkConfig>,
    boot_args: &str,
    config: &RoomsConfig,
) -> Result<(), FirecrackerError> {
    transport::api_put(
        socket,
        "/boot-source",
        &serde_json::json!({
            "kernel_image_path": kernel,
            "boot_args": boot_args,
        }),
        config,
    )
    .await?;

    transport::api_put(
        socket,
        "/drives/rootfs",
        &serde_json::json!({
            "drive_id": "rootfs",
            "path_on_host": rootfs,
            "is_root_device": true,
            "is_read_only": false,
        }),
        config,
    )
    .await?;

    transport::api_put(
        socket,
        "/machine-config",
        &serde_json::json!({
            "vcpu_count": 1,
            "mem_size_mib": 256,
        }),
        config,
    )
    .await?;

    if let Some(net) = network {
        transport::api_put(
            socket,
            "/network-interfaces/eth0",
            &serde_json::json!({
                "iface_id": "eth0",
                "host_dev_name": net.tap_name,
            }),
            config,
        )
        .await?;
        info!(tap = %net.tap_name, guest_ip = %net.guest_ip, "network attached");
    }

    transport::api_put(socket, "/entropy", &serde_json::json!({}), config).await?;

    transport::api_put(
        socket,
        "/actions",
        &serde_json::json!({ "action_type": "InstanceStart" }),
        config,
    )
    .await?;

    Ok(())
}

async fn wait_for_socket(
    socket: &Path,
    timeout: Duration,
    child: &mut Child,
) -> Result<(), FirecrackerError> {
    #[cfg(unix)]
    {
        wait_for_socket_unix(socket, timeout, child).await
    }
    #[cfg(not(unix))]
    {
        let _ = (socket, timeout, child);
        std::future::ready(Err(FirecrackerError::KvmUnavailable)).await
    }
}

#[cfg(unix)]
async fn wait_for_socket_unix(
    socket: &Path,
    timeout: Duration,
    child: &mut Child,
) -> Result<(), FirecrackerError> {
    use std::time::Instant;

    use tokio::time::sleep;
    let start = Instant::now();
    let timeout_ms = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX);

    while start.elapsed() < timeout {
        if let Some(status) = child.try_wait().map_err(FirecrackerError::Io)? {
            let stderr_tail = read_log_tail(socket).await;
            return Err(FirecrackerError::ProcessExitedEarly {
                exit_code: status.code().unwrap_or(-1),
                stderr_tail,
            });
        }

        if tokio::net::UnixStream::connect(socket).await.is_ok() {
            debug!("api socket accepting connections");
            return Ok(());
        }

        sleep(Duration::from_millis(50)).await;
    }

    Err(FirecrackerError::ApiSocketNeverAppeared { timeout_ms })
}

#[cfg(unix)]
async fn read_log_tail(socket: &Path) -> String {
    let log_path = socket
        .parent()
        .map_or_else(|| socket.to_path_buf(), |p| p.join("firecracker.log"));

    let content = tokio::fs::read_to_string(&log_path)
        .await
        .unwrap_or_default();
    let tail: String = content
        .chars()
        .rev()
        .take(512)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    tail
}

fn kill_child_gracefully(pid: Option<u32>, grace: Duration) {
    let Some(pid) = pid else { return };

    #[cfg(unix)]
    {
        use std::process::Command;
        use std::thread;
        use std::time::{Duration as StdDuration, Instant};

        let _ = Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .output();

        // Poll-with-early-exit instead of one blocking sleep(grace). Drop runs
        // synchronously from the tokio runtime, so a full 5s sleep stalls the
        // executor even if the process is already gone after the SIGTERM (the
        // common case for firecracker).
        let deadline = Instant::now() + grace.min(StdDuration::from_secs(5));
        let pid_str = pid.to_string();
        let poll = StdDuration::from_millis(100);
        while Instant::now() < deadline {
            let alive = Command::new("kill")
                .args(["-0", &pid_str])
                .output()
                .is_ok_and(|out| out.status.success());
            if !alive {
                return;
            }
            thread::sleep(poll);
        }

        if Command::new("kill")
            .args(["-0", &pid_str])
            .output()
            .is_ok_and(|out| out.status.success())
        {
            let _ = Command::new("kill").args(["-KILL", &pid_str]).output();
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (pid, grace);
    }
}

fn release_tap(tap_name: Option<&str>) {
    let Some(tap) = tap_name else { return };

    #[cfg(unix)]
    {
        use std::process::Command;
        let _ = Command::new("ip")
            .args(["tuntap", "del", "dev", tap, "mode", "tap"])
            .output();
    }
    #[cfg(not(unix))]
    {
        let _ = tap;
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        reason = "test module"
    )]

    #[cfg(unix)]
    mod unix_tests {
        use std::time::Duration;

        use tokio::net::UnixListener;
        use tokio::process::Command;

        use super::super::wait_for_socket;

        #[tokio::test]
        async fn wait_for_socket_requires_listener_not_just_file() {
            let dir = tempfile::tempdir().expect("tempdir");
            let socket_path = dir.path().join("fake.sock");
            tokio::fs::File::create(&socket_path)
                .await
                .expect("create fake socket file");

            let mut child = Command::new("sleep")
                .arg("60")
                .spawn()
                .expect("spawn sleep");
            let err = wait_for_socket(&socket_path, Duration::from_millis(300), &mut child)
                .await
                .expect_err("file without listener should time out");
            assert!(
                matches!(
                    err,
                    crate::error::FirecrackerError::ApiSocketNeverAppeared { .. }
                ),
                "unexpected error: {err}"
            );
            let _ = child.kill().await;

            tokio::fs::remove_file(&socket_path)
                .await
                .expect("remove fake socket file");
            let _listener = UnixListener::bind(&socket_path).expect("bind listener");
            let mut child2 = Command::new("sleep")
                .arg("60")
                .spawn()
                .expect("spawn sleep");
            wait_for_socket(&socket_path, Duration::from_millis(300), &mut child2)
                .await
                .expect("listening socket should be ready");
            let _ = child2.kill().await;
        }
    }

    use super::RoomGuard;
    use crate::config::RoomsConfig;

    #[test]
    fn room_guard_cleans_up_on_drop_when_not_dismissed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().to_path_buf();
        std::fs::write(path.join("marker"), b"x").expect("write marker");

        let config = RoomsConfig::default();
        drop(RoomGuard::new(path.clone(), path.join("api.sock"), &config));

        assert!(!path.exists(), "guard should remove the directory");
    }

    #[test]
    fn room_guard_preserves_dir_when_dismissed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().to_path_buf();
        let config = RoomsConfig::default();
        let mut guard = RoomGuard::new(path.clone(), path.join("api.sock"), &config);
        guard.dismiss();
        drop(guard);

        assert!(path.exists(), "dismissed guard should leave the directory");
    }
}

#[cfg(all(test, feature = "e2e"))]
mod e2e_tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        reason = "test module"
    )]

    use std::path::PathBuf;
    use std::time::Duration;

    use super::boot;
    use crate::config::RoomsConfig;

    fn image_path(name: &str) -> PathBuf {
        let home = std::env::var("HOME").expect("HOME env var must be set");
        PathBuf::from(home).join("rooms/images").join(name)
    }

    #[tokio::test]
    async fn firecracker_boots_and_survives_briefly() {
        let kernel = image_path("vmlinux.bin");
        let rootfs = image_path("rootfs.ext4");
        let config = RoomsConfig::default();

        assert!(kernel.exists(), "kernel missing at {kernel:?}");
        assert!(rootfs.exists(), "rootfs missing at {rootfs:?}");

        let mut vm = boot(&kernel, &rootfs, None, &config)
            .await
            .expect("boot should succeed");

        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(vm.is_alive().expect("is_alive probe"));
        vm.shutdown().await.expect("shutdown should succeed");
    }
}
