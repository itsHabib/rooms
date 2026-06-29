//! Firecracker process + API control.

#![allow(
    clippy::missing_const_for_fn,
    reason = "many helpers include cfg-gated non-const bodies"
)]

use std::env;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::Utc;
use tokio::process::{Child, Command};
use tracing::{debug, info, warn};
use ulid::Ulid;

use crate::config::RoomsConfig;
use crate::error::FirecrackerError;
use crate::{room, transport};

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

    /// Construct a guard over an already-dead room's paths, for orphan reaping
    /// (`rooms gc`). No child pid — the process is *confirmed dead* before gc
    /// reaps it, so there's nothing to kill (which also sidesteps signalling a
    /// reused pid) — and no tap ownership (v0's `tap-fc0` is shared). `cleanup`
    /// then only unmounts the jail binds, removes the socket, and removes both
    /// dirs: the exact teardown the live drop runs.
    pub fn for_orphan(
        room_dir: PathBuf,
        socket: PathBuf,
        jail_instance_dir: PathBuf,
        config: &RoomsConfig,
    ) -> Self {
        let mut guard = Self::new(room_dir, socket, config);
        guard.set_jail_instance_dir(jail_instance_dir);
        guard
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
        let jail_torn_down = self
            .jail_instance_dir
            .take()
            .is_none_or(|jail_dir| teardown_jail_sync(&jail_dir));
        // Keep the room dir (gc's only handle on this room) if the jail tree
        // didn't fully tear down: a stuck bind-mount leaves the jail dir behind,
        // and deleting the room dir here would orphan that mount invisibly (the
        // registry scans the per-room state dir, not the chroot subtree). With
        // the room dir preserved and the process dead, the room re-classifies as
        // orphaned-dead, so a later `rooms gc` retries the unmount and reaps it.
        if jail_torn_down {
            // Surface a failed room-dir removal: the jail is clean but the room
            // dir lingers, so the room re-lists as `unknown` (fc gone, meta
            // intact) and never becomes reapable — at least make it diagnosable.
            if let Err(e) = std::fs::remove_dir_all(&self.room_dir) {
                warn!(room_dir = %self.room_dir.display(), error = %e, "failed to remove room dir after cleanup; it may re-list as unknown");
            }
            return;
        }
        warn!(
            room_dir = %self.room_dir.display(),
            "jail teardown incomplete (stranded mount?); keeping room dir so `rooms gc` can reap it"
        );
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
    readonly_rootfs: bool,
    descriptor: &room::RoomDescriptor,
) -> Result<BootedVm, FirecrackerError> {
    ensure_root()?;
    check_kvm()?;
    let firecracker_binary = resolve_firecracker_binary(config)?;
    let jailer_binary = resolve_jailer_binary(config)?;
    let (fc_uid, fc_gid) = tokio::task::spawn_blocking(lookup_firecracker_ids)
        .await
        .map_err(|e| FirecrackerError::Internal(format!("spawn_blocking panicked: {e}")))??;

    let room_id = RoomId::new();
    let room_id_str = room_id.as_str();
    let per_room_dir = config
        .room_dir(&room_id_str)
        .ok_or(FirecrackerError::HomeUnset)?;
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
    write_room_meta(&per_room_dir, &room_id_str, descriptor, child.id());

    wait_for_socket(
        &socket,
        config.api_socket_timeout,
        &mut child,
        Some(&log_path),
    )
    .await?;

    let boot_args = build_boot_args(network, readonly_rootfs);
    let rootfs_drive = rootfs_drive_payload(&launch.rootfs_path_in_jail, readonly_rootfs);
    configure_vm(
        &socket,
        &launch.kernel_path_in_jail,
        &rootfs_drive,
        network,
        &boot_args,
        config,
    )
    .await?;

    info!("microVM booted");
    Ok(BootedVm { guard, child })
}

/// Jailer must run as root: it chroots, bind-mounts the kernel/rootfs into the
/// jail, and drops to the firecracker uid. Fail early and clearly when not
/// root rather than cryptically at the first `mount --bind`.
#[cfg(unix)]
fn ensure_root() -> Result<(), FirecrackerError> {
    let output = std::process::Command::new("id")
        .arg("-u")
        .output()
        .map_err(FirecrackerError::Io)?;
    if String::from_utf8_lossy(&output.stdout).trim() == "0" {
        return Ok(());
    }
    Err(FirecrackerError::RootRequired)
}

#[cfg(not(unix))]
fn ensure_root() -> Result<(), FirecrackerError> {
    Err(FirecrackerError::RootRequired)
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
    config.chroot_base().ok_or(FirecrackerError::HomeUnset)
}

/// Persist the room's metadata (`room.json`) as early as the pid is known.
///
/// Best-effort: a write failure only hides the room from `rooms ls`, never
/// aborts an otherwise-successful boot. The recorded pid is the jailer child,
/// which `exec`s firecracker in place (no `--daemonize` in the launch plan), so
/// it stays the firecracker pid for the room's life — `room::probe` liveness
/// depends on that. If a `--daemonize` jailer is ever adopted, capture the
/// firecracker pid the jailer writes instead.
fn write_room_meta(room_dir: &Path, id: &str, descriptor: &room::RoomDescriptor, pid: Option<u32>) {
    let meta = room::RoomMeta::new(
        id.to_owned(),
        descriptor.command.clone(),
        pid,
        descriptor.keep,
        Utc::now(),
    );
    if let Err(e) = room::write_atomic(room_dir, &meta) {
        warn!(error = %e, "failed to write room.json; room will be invisible to `rooms ls`");
    }
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
    if let Err(e) = bind_mount(rootfs, &jail_rootfs) {
        // Roll back the kernel bind + the partial jail tree so a failed boot
        // doesn't strand an active mount and directory behind it.
        unmount_quiet(&jail_kernel);
        let _ = std::fs::remove_dir_all(jail_instance_dir(chroot_base, room_id));
        return Err(e);
    }
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

#[cfg(unix)]
fn unmount(target: &Path) -> Result<(), String> {
    let output = std::process::Command::new("umount")
        .arg(target)
        .output()
        .map_err(|e| e.to_string())?;
    if output.status.success() {
        return Ok(());
    }
    Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
}

#[cfg(unix)]
fn unmount_quiet(target: &Path) {
    let _ = unmount(target);
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

/// Unmount a room's jail binds and remove its instance dir. Returns `true` only
/// when the jail fully tore down — i.e. the instance dir is gone. A still-active
/// bind mount fails the `remove_dir_all` (EBUSY) and leaves the dir behind, so
/// the dir-gone check is the authoritative signal; the `unmount` exit codes are
/// advisory (a "not mounted" error on a plain file is harmless).
#[cfg(unix)]
fn teardown_jail_sync(instance_dir: &Path) -> bool {
    let jail_root = instance_dir.join("root");
    for name in [JAIL_KERNEL, JAIL_ROOTFS] {
        let target = jail_root.join(name);
        if target.exists() {
            if let Err(e) = unmount(&target) {
                tracing::warn!(target = %target.display(), error = %e, "umount reported an error; relying on dir removal");
            }
        }
    }
    if let Err(e) = std::fs::remove_dir_all(instance_dir) {
        tracing::warn!(dir = %instance_dir.display(), error = %e, "failed to remove jail instance dir (active mount?)");
    }
    !instance_dir.exists()
}

#[cfg(not(unix))]
const fn teardown_jail_sync(_instance_dir: &Path) -> bool {
    true
}

/// Tear down an orphaned room — one whose live `RoomGuard` is gone.
///
/// A crash, a killed launcher, or a `--keep` room whose firecracker later died.
/// Reconstructs a guard over the room's paths and runs the same cleanup the live
/// drop uses; the caller (gc) guarantees the firecracker process is already
/// dead. Returns an error if either dir survives, so `rooms gc` reports an
/// honest outcome rather than a silent leak.
pub fn reap_orphan(
    room_dir: &Path,
    jail_instance_dir: &Path,
    socket: &Path,
    config: &RoomsConfig,
) -> Result<(), FirecrackerError> {
    let mut guard = RoomGuard::for_orphan(
        room_dir.to_path_buf(),
        socket.to_path_buf(),
        jail_instance_dir.to_path_buf(),
        config,
    );
    guard.cleanup();
    guard.dismiss(); // cleanup already ran; don't let Drop run it again.
                     // cleanup keeps the room dir when the jail tree didn't fully tear down, so a
                     // surviving jail dir is the primary signal of an incomplete reap (a stranded
                     // bind-mount). Report it honestly; the room stays listed for a retry.
    if jail_instance_dir.exists() {
        return Err(FirecrackerError::Internal(format!(
            "jail instance dir survived reap (stranded mount?): {}",
            jail_instance_dir.display()
        )));
    }
    if room_dir.exists() {
        return Err(FirecrackerError::Internal(format!(
            "room dir survived reap: {}",
            room_dir.display()
        )));
    }
    Ok(())
}

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

const OVERLAY_INIT: &str = "/sbin/overlay-init";

fn build_boot_args(network: Option<&NetworkConfig>, readonly_rootfs: bool) -> String {
    // `init=/sbin/overlay-init` only ships in images built by
    // build-rootfs-alpine.sh; force it (and the read-only drive) only when the
    // caller opts in, so a plain `rooms run --command` against any image still
    // boots rather than panicking on a missing init.
    let mut base = String::from("console=ttyS0 reboot=k panic=1 pci=off random.trust_cpu=on");
    if readonly_rootfs {
        base.push_str(" init=");
        base.push_str(OVERLAY_INIT);
    }
    let Some(net) = network else {
        return base;
    };
    format!(
        "{base} ip={}::{}:{}::eth0:off",
        net.guest_ip, net.gateway_ip, net.netmask
    )
}

fn rootfs_drive_payload(rootfs: &Path, readonly_rootfs: bool) -> serde_json::Value {
    serde_json::json!({
        "drive_id": "rootfs",
        "path_on_host": rootfs,
        "is_root_device": true,
        "is_read_only": readonly_rootfs,
    })
}

async fn configure_vm(
    socket: &Path,
    kernel: &Path,
    rootfs_drive: &serde_json::Value,
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

    transport::api_put(socket, "/drives/rootfs", rootfs_drive, config).await?;

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
        build_boot_args, build_jailer_launch_plan, parse_getent_passwd, rootfs_drive_payload,
        JailLayout, JailerLaunchInput, NetworkConfig, RoomGuard, FIRECRACKER_USER,
    };
    use crate::config::RoomsConfig;
    use std::path::{Path, PathBuf};

    #[test]
    fn build_boot_args_overlay_init_only_when_readonly() {
        let on = build_boot_args(None, true);
        assert!(
            on.contains("init=/sbin/overlay-init"),
            "readonly boot args must hand off to overlay-init: {on}"
        );
        assert!(
            !on.contains(" ip="),
            "no network suffix without config: {on}"
        );

        let off = build_boot_args(None, false);
        assert!(
            !off.contains("init="),
            "non-readonly boot must not force an init= (any image boots): {off}"
        );
    }

    #[test]
    fn build_boot_args_includes_overlay_init_with_network() {
        let net = NetworkConfig {
            tap_name: "tap-fc0".to_owned(),
            guest_ip: "172.16.0.2".to_owned(),
            gateway_ip: "172.16.0.1".to_owned(),
            netmask: "255.255.255.0".to_owned(),
        };
        let args = build_boot_args(Some(&net), true);
        assert!(
            args.contains("init=/sbin/overlay-init"),
            "boot args must hand off to overlay-init: {args}"
        );
        assert!(
            args.contains("ip=172.16.0.2::172.16.0.1:255.255.255.0::eth0:off"),
            "network suffix must follow overlay init: {args}"
        );
    }

    #[test]
    fn rootfs_drive_payload_read_only_tracks_flag() {
        let ro = rootfs_drive_payload(Path::new("/tmp/rootfs.ext4"), true);
        assert_eq!(ro.get("drive_id"), Some(&serde_json::json!("rootfs")));
        assert_eq!(ro.get("is_root_device"), Some(&serde_json::json!(true)));
        assert_eq!(
            ro.get("is_read_only"),
            Some(&serde_json::json!(true)),
            "readonly path must mount the drive read-only"
        );
        assert_eq!(
            ro.get("path_on_host"),
            Some(&serde_json::json!("/tmp/rootfs.ext4"))
        );

        let rw = rootfs_drive_payload(Path::new("/tmp/rootfs.ext4"), false);
        assert_eq!(
            rw.get("is_read_only"),
            Some(&serde_json::json!(false)),
            "non-readonly path must keep the drive writable"
        );
    }

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

    /// Regression for the adversarial finding: when the jail tree can't fully
    /// tear down (a stuck bind-mount blocking `remove_dir_all`), `cleanup` must
    /// PRESERVE the room dir — gc's only handle — so the stranded mount stays
    /// reapable instead of being orphaned invisibly. We inject the failure by
    /// making the jail dir's parent read-only so the final rmdir can't unlink it
    /// (no root or real mount required).
    #[cfg(unix)]
    #[test]
    fn cleanup_keeps_room_dir_when_jail_teardown_fails() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().expect("tempdir");
        let room_dir = tmp.path().join("room");
        std::fs::create_dir_all(&room_dir).expect("room dir");
        std::fs::write(room_dir.join("room.json"), b"{}").expect("marker");

        let fc_parent = tmp.path().join("chroot").join("firecracker");
        let instance = fc_parent.join("01abcdefghijklmnopqrstuvwx");
        std::fs::create_dir_all(instance.join("root")).expect("jail tree");
        std::fs::write(instance.join("root").join("kernel"), b"k").expect("kernel");
        std::fs::write(instance.join("root").join("rootfs"), b"r").expect("rootfs");
        std::fs::set_permissions(&fc_parent, std::fs::Permissions::from_mode(0o500))
            .expect("lock parent");

        let config = RoomsConfig::default();
        let mut guard = RoomGuard::for_orphan(
            room_dir.clone(),
            room_dir.join("api.sock"),
            instance.clone(),
            &config,
        );
        guard.cleanup();
        guard.dismiss(); // don't let drop re-run cleanup

        let injected = instance.exists();
        // Restore perms so the tempdir can be cleaned up regardless of outcome.
        std::fs::set_permissions(&fc_parent, std::fs::Permissions::from_mode(0o700))
            .expect("restore perms");

        if !injected {
            // Running as root: the read-only parent didn't block removal, so the
            // failure couldn't be injected. The preserve-on-failure path is
            // covered on non-root runners (CI + the rooms-host).
            return;
        }
        assert!(
            room_dir.exists(),
            "room dir (gc's handle) must be preserved when the jail tree leaks"
        );
    }

    /// A failed room-dir removal on the jail-succeeded path must not panic and
    /// must leave the dir (so it's at least diagnosable, per the cleanup warn).
    /// We inject the removal failure with a read-only parent; no jail dir is set,
    /// so the jail teardown "succeeds" and cleanup proceeds to the room-dir rm.
    #[cfg(unix)]
    #[test]
    fn cleanup_survives_room_dir_removal_failure() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().expect("tempdir");
        let parent = tmp.path().join("locked");
        let room_dir = parent.join("room");
        std::fs::create_dir_all(&room_dir).expect("room dir");
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o500))
            .expect("lock parent");

        let config = RoomsConfig::default();
        let mut guard = RoomGuard::new(room_dir.clone(), room_dir.join("api.sock"), &config);
        guard.cleanup(); // no jail dir → jail "torn down" → attempts room-dir rm
        guard.dismiss();

        let injected = room_dir.exists();
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700))
            .expect("restore perms");

        if injected {
            assert!(
                room_dir.exists(),
                "a failed room-dir removal must leave the dir, not panic"
            );
        }
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

        let mut vm = boot(
            &kernel,
            &rootfs,
            None,
            &config,
            false,
            &crate::room::RoomDescriptor::default(),
        )
        .await
        .expect("boot should succeed");

        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(vm.is_alive().expect("is_alive probe"));
        vm.shutdown().await.expect("shutdown should succeed");
    }
}
