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

/// Dedicated unprivileged user Firecracker runs as inside the jailer.
pub const FIRECRACKER_USER: &str = "firecracker";

/// Unix socket file name inside the jail root (host path is under the chroot tree).
const JAIL_API_SOCK: &str = "api.sock";

/// Bind-mount target names inside the jail root for kernel and rootfs.
const JAIL_KERNEL: &str = "kernel";
const JAIL_ROOTFS: &str = "rootfs";

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

    fn as_str(&self) -> String {
        self.0.to_string().to_lowercase()
    }
}

/// RAII guard that cleans up room resources on drop or explicit shutdown.
#[derive(Debug)]
pub struct RoomGuard {
    room_dir: PathBuf,
    socket: PathBuf,
    child_pid: Option<u32>,
    tap_name: Option<String>,
    tap_owned: bool,
    suppress_cleanup: bool,
    dismiss: bool,
    cleanup_grace: Duration,
    jail_instance_dir: Option<PathBuf>,
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
            jail_instance_dir: None,
        }
    }

    fn set_jail_instance_dir(&mut self, path: PathBuf) {
        self.jail_instance_dir = Some(path);
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
        if let Some(jail_dir) = self.jail_instance_dir.take() {
            teardown_jail_sync(&jail_dir);
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
#[derive(Debug)]
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

/// Resolved jailer invocation plan (pure data for tests and spawn).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JailerLaunchPlan {
    pub jailer_binary: PathBuf,
    pub jailer_args: Vec<String>,
    pub firecracker_args: Vec<String>,
    pub host_socket: PathBuf,
    pub kernel_path_in_jail: PathBuf,
    pub rootfs_path_in_jail: PathBuf,
}

/// Boot a Firecracker microVM with the given kernel + rootfs.
pub async fn boot(
    kernel: &Path,
    rootfs: &Path,
    network: Option<&NetworkConfig>,
    config: &RoomsConfig,
) -> Result<BootedVm, FirecrackerError> {
    check_kvm()?;
    let firecracker_binary = resolve_firecracker_binary(config)?;
    let jailer_binary = resolve_jailer_binary(config)?;
    let (fc_uid, fc_gid) = lookup_firecracker_ids()?;

    let room_id = RoomId::new();
    let room_id_str = room_id.as_str();
    let per_room_dir = room_state_dir(&room_id)?;
    prepare_room_dir(&per_room_dir).await?;

    let chroot_base = jailer_chroot_base(config)?;
    let jail_layout =
        prepare_jail_layout(&chroot_base, &room_id_str, kernel, rootfs, fc_uid, fc_gid).await?;

    let socket = jail_layout.host_socket.clone();
    let log_path = per_room_dir.join("firecracker.log");
    let mut guard = RoomGuard::new(per_room_dir.clone(), socket.clone(), config);
    guard.set_jail_instance_dir(jail_layout.instance_dir.clone());

    if let Some(net) = network {
        guard.set_tap(net.tap_name.clone());
    }

    let launch = build_jailer_launch_plan(&JailerLaunchInput {
        jailer_binary: &jailer_binary,
        firecracker_binary: &firecracker_binary,
        chroot_base: &chroot_base,
        room_id: &room_id_str,
        fc_uid,
        fc_gid,
        layout: &jail_layout,
    });

    let log_handles = open_log_file(&log_path).await?;
    let mut child = spawn_jailer(&launch, log_handles)?;
    guard.set_child(&child);

    wait_for_socket(
        &socket,
        config.api_socket_timeout,
        &mut child,
        Some(&log_path),
    )
    .await?;

    let boot_args = build_boot_args(network);
    configure_vm(
        &socket,
        &launch.kernel_path_in_jail,
        &launch.rootfs_path_in_jail,
        network,
        &boot_args,
        config,
    )
    .await?;

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

fn resolve_firecracker_binary(config: &RoomsConfig) -> Result<PathBuf, FirecrackerError> {
    resolve_binary_on_path(&config.firecracker_binary, |path| {
        FirecrackerError::BinaryNotFound {
            path: path.to_path_buf(),
        }
    })
}

fn resolve_jailer_binary(config: &RoomsConfig) -> Result<PathBuf, FirecrackerError> {
    resolve_binary_on_path(&config.jailer_binary, |path| {
        FirecrackerError::JailerNotFound {
            path: path.to_path_buf(),
        }
    })
}

fn resolve_binary_on_path<F>(binary: &Path, not_found: F) -> Result<PathBuf, FirecrackerError>
where
    F: FnOnce(&Path) -> FirecrackerError,
{
    if binary.is_absolute() || binary.components().count() > 1 {
        if binary.exists() {
            return Ok(binary.to_path_buf());
        }
        return Err(not_found(binary));
    }
    let path_var = env::var_os("PATH").ok_or_else(|| FirecrackerError::JailPrepareFailed {
        reason: "PATH unset".to_owned(),
    })?;
    env::split_paths(&path_var)
        .map(|dir| dir.join(binary))
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| not_found(binary))
}

fn jailer_chroot_base(config: &RoomsConfig) -> Result<PathBuf, FirecrackerError> {
    if let Some(base) = &config.jailer_chroot_base {
        return Ok(base.clone());
    }
    let home = env::var("HOME").map_err(|_| FirecrackerError::HomeUnset)?;
    Ok(PathBuf::from(home)
        .join(".local/state/rooms")
        .join("jailer"))
}

#[derive(Debug, Clone)]
pub(crate) struct JailLayout {
    instance_dir: PathBuf,
    host_socket: PathBuf,
}

fn jail_instance_dir(chroot_base: &Path, room_id: &str) -> PathBuf {
    chroot_base.join("firecracker").join(room_id)
}

fn jail_root_dir(chroot_base: &Path, room_id: &str) -> PathBuf {
    jail_instance_dir(chroot_base, room_id).join("root")
}

/// Inputs for [`build_jailer_launch_plan`].
pub(crate) struct JailerLaunchInput<'a> {
    pub jailer_binary: &'a Path,
    pub firecracker_binary: &'a Path,
    pub chroot_base: &'a Path,
    pub room_id: &'a str,
    pub fc_uid: u32,
    pub fc_gid: u32,
    pub layout: &'a JailLayout,
}

fn build_jailer_launch_plan(input: &JailerLaunchInput<'_>) -> JailerLaunchPlan {
    let JailerLaunchInput {
        jailer_binary,
        firecracker_binary,
        chroot_base,
        room_id,
        fc_uid,
        fc_gid,
        layout,
    } = input;
    let jailer_args = vec![
        "--id".to_owned(),
        (*room_id).to_owned(),
        "--uid".to_owned(),
        fc_uid.to_string(),
        "--gid".to_owned(),
        fc_gid.to_string(),
        "--exec-file".to_owned(),
        firecracker_binary.to_string_lossy().into_owned(),
        "--chroot-base-dir".to_owned(),
        chroot_base.to_string_lossy().into_owned(),
    ];
    let firecracker_args = vec!["--api-sock".to_owned(), JAIL_API_SOCK.to_owned()];
    let kernel_path_in_jail = PathBuf::from(format!("/{JAIL_KERNEL}"));
    let rootfs_path_in_jail = PathBuf::from(format!("/{JAIL_ROOTFS}"));

    JailerLaunchPlan {
        jailer_binary: jailer_binary.to_path_buf(),
        jailer_args,
        firecracker_args,
        host_socket: layout.host_socket.clone(),
        kernel_path_in_jail,
        rootfs_path_in_jail,
    }
}

#[cfg(unix)]
fn lookup_firecracker_ids() -> Result<(u32, u32), FirecrackerError> {
    use std::process::Command;

    let output = Command::new("getent")
        .args(["passwd", FIRECRACKER_USER])
        .output()
        .map_err(FirecrackerError::Io)?;
    if !output.status.success() {
        return Err(FirecrackerError::FirecrackerUserMissing {
            user: FIRECRACKER_USER.to_owned(),
        });
    }
    parse_getent_passwd(&String::from_utf8_lossy(&output.stdout)).ok_or_else(|| {
        FirecrackerError::FirecrackerUserMissing {
            user: FIRECRACKER_USER.to_owned(),
        }
    })
}

#[cfg(not(unix))]
fn lookup_firecracker_ids() -> Result<(u32, u32), FirecrackerError> {
    Err(FirecrackerError::FirecrackerUserMissing {
        user: FIRECRACKER_USER.to_owned(),
    })
}

/// Parse `getent passwd` output into `(uid, gid)`.
pub fn parse_getent_passwd(line: &str) -> Option<(u32, u32)> {
    let line = line.lines().next()?.trim();
    if line.is_empty() {
        return None;
    }
    let mut fields = line.split(':');
    let _name = fields.next()?;
    let _passwd = fields.next()?;
    let uid: u32 = fields.next()?.parse().ok()?;
    let gid: u32 = fields.next()?.parse().ok()?;
    Some((uid, gid))
}

async fn prepare_jail_layout(
    chroot_base: &Path,
    room_id: &str,
    kernel: &Path,
    rootfs: &Path,
    fc_uid: u32,
    fc_gid: u32,
) -> Result<JailLayout, FirecrackerError> {
    let kernel = kernel
        .canonicalize()
        .map_err(|e| FirecrackerError::JailPrepareFailed {
            reason: format!("kernel path {}: {e}", kernel.display()),
        })?;
    let rootfs = rootfs
        .canonicalize()
        .map_err(|e| FirecrackerError::JailPrepareFailed {
            reason: format!("rootfs path {}: {e}", rootfs.display()),
        })?;
    let instance_dir = jail_instance_dir(chroot_base, room_id);
    let jail_root = jail_root_dir(chroot_base, room_id);
    let host_socket = jail_root.join(JAIL_API_SOCK);

    let chroot_base = chroot_base.to_path_buf();
    let room_id = room_id.to_owned();
    tokio::task::spawn_blocking(move || {
        stage_jail_sync(&chroot_base, &room_id, &kernel, &rootfs, fc_uid, fc_gid)
    })
    .await
    .map_err(|e| FirecrackerError::Internal(format!("spawn_blocking panicked: {e}")))?
    .map(|()| JailLayout {
        instance_dir,
        host_socket,
    })
}

#[cfg(unix)]
fn stage_jail_sync(
    chroot_base: &Path,
    room_id: &str,
    kernel: &Path,
    rootfs: &Path,
    _fc_uid: u32,
    _fc_gid: u32,
) -> Result<(), FirecrackerError> {
    let jail_root = jail_root_dir(chroot_base, room_id);
    std::fs::create_dir_all(&jail_root).map_err(|e| FirecrackerError::JailPrepareFailed {
        reason: format!("create jail root {}: {e}", jail_root.display()),
    })?;

    let jail_kernel = jail_root.join(JAIL_KERNEL);
    let jail_rootfs = jail_root.join(JAIL_ROOTFS);
    for target in [&jail_kernel, &jail_rootfs] {
        if !target.exists() {
            std::fs::File::create(target).map_err(|e| FirecrackerError::JailPrepareFailed {
                reason: format!("create mount target {}: {e}", target.display()),
            })?;
        }
    }

    bind_mount(kernel, &jail_kernel)?;
    bind_mount(rootfs, &jail_rootfs)?;
    Ok(())
}

#[cfg(unix)]
fn bind_mount(source: &Path, target: &Path) -> Result<(), FirecrackerError> {
    use std::process::Command;

    let output = Command::new("mount")
        .args([
            "--bind",
            &source.to_string_lossy(),
            &target.to_string_lossy(),
        ])
        .output()
        .map_err(|e| FirecrackerError::JailPrepareFailed {
            reason: format!(
                "mount --bind {} -> {}: {e}",
                source.display(),
                target.display()
            ),
        })?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(FirecrackerError::JailPrepareFailed {
        reason: format!(
            "mount --bind {} -> {} failed (need root?): {stderr}",
            source.display(),
            target.display()
        ),
    })
}

#[cfg(not(unix))]
fn stage_jail_sync(
    _chroot_base: &Path,
    _room_id: &str,
    _kernel: &Path,
    _rootfs: &Path,
    _fc_uid: u32,
    _fc_gid: u32,
) -> Result<(), FirecrackerError> {
    Err(FirecrackerError::KvmUnavailable)
}

#[cfg(unix)]
fn teardown_jail_sync(instance_dir: &Path) {
    use std::process::Command;

    let jail_root = instance_dir.join("root");
    for name in [JAIL_KERNEL, JAIL_ROOTFS] {
        let target = jail_root.join(name);
        if target.exists() {
            let _ = Command::new("umount").arg(&target).output();
        }
    }
    let _ = std::fs::remove_dir_all(instance_dir);
}

#[cfg(not(unix))]
fn teardown_jail_sync(_instance_dir: &Path) {}

fn spawn_jailer(
    plan: &JailerLaunchPlan,
    (log_file, log_stderr): (std::fs::File, std::fs::File),
) -> Result<Child, FirecrackerError> {
    info!(socket = %plan.host_socket.display(), "spawning firecracker via jailer");
    Command::new(&plan.jailer_binary)
        .args(&plan.jailer_args)
        .arg("--")
        .args(&plan.firecracker_args)
        .stdout(std::process::Stdio::from(log_file))
        .stderr(std::process::Stdio::from(log_stderr))
        .kill_on_drop(true)
        .spawn()
        .map_err(|err| match err.kind() {
            std::io::ErrorKind::NotFound => FirecrackerError::JailerNotFound {
                path: plan.jailer_binary.clone(),
            },
            _ => FirecrackerError::Io(err),
        })
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
    log_path: Option<&Path>,
) -> Result<(), FirecrackerError> {
    #[cfg(unix)]
    {
        wait_for_socket_unix(socket, timeout, child, log_path).await
    }
    #[cfg(not(unix))]
    {
        let _ = (socket, timeout, child, log_path);
        std::future::ready(Err(FirecrackerError::KvmUnavailable)).await
    }
}

#[cfg(unix)]
async fn wait_for_socket_unix(
    socket: &Path,
    timeout: Duration,
    child: &mut Child,
    log_path: Option<&Path>,
) -> Result<(), FirecrackerError> {
    use std::time::Instant;

    use tokio::time::sleep;
    let start = Instant::now();
    let timeout_ms = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX);

    while start.elapsed() < timeout {
        if let Some(status) = child.try_wait().map_err(FirecrackerError::Io)? {
            let stderr_tail = read_log_tail(log_path, socket).await;
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
async fn read_log_tail(log_path: Option<&Path>, socket: &Path) -> String {
    let log_path = log_path.map_or_else(
        || {
            socket
                .parent()
                .map_or_else(|| socket.to_path_buf(), |p| p.join("firecracker.log"))
        },
        Path::to_path_buf,
    );

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
            let err = wait_for_socket(&socket_path, Duration::from_millis(300), &mut child, None)
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
            wait_for_socket(&socket_path, Duration::from_millis(300), &mut child2, None)
                .await
                .expect("listening socket should be ready");
            let _ = child2.kill().await;
        }
    }

    use super::{
        build_jailer_launch_plan, parse_getent_passwd, JailLayout, JailerLaunchInput, RoomGuard,
        FIRECRACKER_USER,
    };
    use crate::config::RoomsConfig;
    use std::path::{Path, PathBuf};

    #[test]
    fn parse_getent_passwd_extracts_uid_gid() {
        assert_eq!(
            parse_getent_passwd(
                "firecracker:x:995:995:Firecracker microVM:/nonexistent:/usr/sbin/nologin\n"
            ),
            Some((995, 995))
        );
        assert_eq!(parse_getent_passwd(""), None);
    }

    #[test]
    fn jailer_launch_plan_assembles_expected_argv() {
        let room_id = "01abc123def456";
        let chroot_base = PathBuf::from("/tmp/rooms-jailer");
        let jail_root = chroot_base.join("firecracker").join(room_id).join("root");
        let layout = JailLayout {
            instance_dir: chroot_base.join("firecracker").join(room_id),
            host_socket: jail_root.join("api.sock"),
        };
        let plan = build_jailer_launch_plan(&JailerLaunchInput {
            jailer_binary: Path::new("/usr/local/bin/jailer"),
            firecracker_binary: Path::new("/usr/local/bin/firecracker"),
            chroot_base: &chroot_base,
            room_id,
            fc_uid: 995,
            fc_gid: 995,
            layout: &layout,
        });

        assert_eq!(plan.jailer_binary, PathBuf::from("/usr/local/bin/jailer"));
        assert_eq!(
            plan.jailer_args,
            vec![
                "--id".to_owned(),
                room_id.to_owned(),
                "--uid".to_owned(),
                "995".to_owned(),
                "--gid".to_owned(),
                "995".to_owned(),
                "--exec-file".to_owned(),
                "/usr/local/bin/firecracker".to_owned(),
                "--chroot-base-dir".to_owned(),
                "/tmp/rooms-jailer".to_owned(),
            ]
        );
        assert_eq!(
            plan.firecracker_args,
            vec!["--api-sock".to_owned(), "api.sock".to_owned()]
        );
        assert_eq!(plan.host_socket, jail_root.join("api.sock"));
        assert_eq!(plan.kernel_path_in_jail, PathBuf::from("/kernel"));
        assert_eq!(plan.rootfs_path_in_jail, PathBuf::from("/rootfs"));
        assert_eq!(FIRECRACKER_USER, "firecracker");
    }

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

    mod room_guard_properties {
        use proptest::prelude::*;

        use super::RoomGuard;
        use crate::config::RoomsConfig;

        #[derive(Debug, Clone)]
        enum GuardAction {
            Dismiss,
            SetSuppressCleanup(bool),
            SetTapOwned(bool),
        }

        prop_compose! {
            fn arb_guard_action()(tag in 0u8..3, flag in any::<bool>()) -> GuardAction {
                match tag {
                    0 => GuardAction::Dismiss,
                    1 => GuardAction::SetSuppressCleanup(flag),
                    _ => GuardAction::SetTapOwned(flag),
                }
            }
        }

        fn apply_action(guard: &mut RoomGuard, action: &GuardAction) {
            match action {
                GuardAction::Dismiss => guard.dismiss(),
                GuardAction::SetSuppressCleanup(suppress) => {
                    guard.set_suppress_cleanup(*suppress);
                }
                GuardAction::SetTapOwned(owned) => guard.set_tap_owned(*owned),
            }
        }

        fn should_preserve_on_drop(actions: &[GuardAction]) -> bool {
            let mut dismissed = false;
            let mut suppress_cleanup = false;
            for action in actions {
                match action {
                    GuardAction::Dismiss => dismissed = true,
                    GuardAction::SetSuppressCleanup(suppress) => suppress_cleanup = *suppress,
                    GuardAction::SetTapOwned(_) => {}
                }
            }
            dismissed || suppress_cleanup
        }

        proptest! {
            #[test]
            fn drop_respects_dismiss_and_suppress_flags(
                actions in proptest::collection::vec(arb_guard_action(), 0..24),
            ) {
                let dir = tempfile::tempdir().expect("tempdir");
                let path = dir.path().to_path_buf();
                std::fs::write(path.join("marker"), b"x").expect("write marker");

                let config = RoomsConfig::default();
                let mut guard = RoomGuard::new(path.clone(), path.join("api.sock"), &config);
                for action in &actions {
                    apply_action(&mut guard, action);
                }
                drop(guard);

                if should_preserve_on_drop(&actions) {
                    prop_assert!(path.exists(), "dismiss/suppress should skip cleanup");
                } else {
                    prop_assert!(!path.exists(), "guard should remove the directory");
                }
            }
        }
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
