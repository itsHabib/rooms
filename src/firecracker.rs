//! Firecracker process + API control.
//!
//! POC: shells out to `firecracker` and `curl --unix-socket` for API calls.
//! A proper HTTP-over-Unix-socket client lands in task #2 (`harden-firecracker-control`).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::process::{Child, Command};
use tokio::time::sleep;
use tracing::{debug, info};

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

/// A booted Firecracker microVM. Dropping the handle kills the process.
pub struct BootedVm {
    child: Child,
    socket: PathBuf,
}

impl BootedVm {
    /// Best-effort: terminate the firecracker process and remove the API socket.
    pub async fn shutdown(mut self) -> Result<()> {
        // SIGKILL is fine for the POC; SIGTERM-then-SIGKILL with grace is
        // a task #2 concern.
        self.child
            .kill()
            .await
            .context("failed to kill firecracker child")?;
        if self.socket.exists() {
            tokio::fs::remove_file(&self.socket).await.ok();
        }
        Ok(())
    }

    /// Returns true if the firecracker process is still running.
    pub fn is_alive(&mut self) -> Result<bool> {
        Ok(self.child.try_wait().context("try_wait failed")?.is_none())
    }
}

/// Boot a Firecracker microVM with the given kernel + rootfs, optionally
/// attaching a network interface.
///
/// POC: minimal config — 1 vCPU, 256 MiB. Caller is responsible for invoking
/// [`BootedVm::shutdown`] when done.
pub async fn boot(
    kernel: &Path,
    rootfs: &Path,
    network: Option<&NetworkConfig>,
) -> Result<BootedVm> {
    let socket = PathBuf::from(format!("/tmp/fc-{}.sock", std::process::id()));

    // Clean any stale socket from a previous run.
    if socket.exists() {
        let _ = tokio::fs::remove_file(&socket).await;
    }

    info!(socket = %socket.display(), "spawning firecracker");
    let child = Command::new("firecracker")
        .arg("--api-sock")
        .arg(&socket)
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
    Ok(BootedVm { child, socket })
}

async fn wait_for_socket(socket: &Path, timeout: Duration) -> Result<()> {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if socket.exists() {
            debug!("api socket appeared");
            return Ok(());
        }
        sleep(Duration::from_millis(50)).await;
    }
    anyhow::bail!("firecracker api socket did not appear within {timeout:?}");
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
