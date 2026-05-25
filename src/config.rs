//! Configurable timeouts and paths for the rooms substrate.

use std::path::PathBuf;
use std::time::Duration;

/// Runtime configuration for Firecracker control and guest reachability probes.
#[derive(Debug, Clone)]
pub struct RoomsConfig {
    /// Default timeout for Firecracker REST API calls (except `InstanceStart`).
    pub api_timeout: Duration,
    /// Timeout for the `InstanceStart` action.
    pub instance_start_timeout: Duration,
    /// How long to wait for the Firecracker API socket to accept connections.
    pub api_socket_timeout: Duration,
    /// How long to wait for the guest to become reachable (SSH).
    pub guest_reach_timeout: Duration,
    /// Poll interval while waiting for guest reachability.
    pub guest_reach_poll_interval: Duration,
    /// Grace period between SIGTERM and SIGKILL during cleanup.
    pub cleanup_grace: Duration,
    /// Path to the firecracker binary (default: `"firecracker"` on PATH).
    pub firecracker_binary: PathBuf,
    /// Minimum supported Firecracker semver major.minor.
    pub min_firecracker_version: (u32, u32),
    /// Minimum rootfs image size in bytes.
    pub min_rootfs_bytes: u64,
}

impl Default for RoomsConfig {
    fn default() -> Self {
        Self {
            api_timeout: Duration::from_secs(30),
            #[allow(
                clippy::duration_suboptimal_units,
                reason = "from_mins requires Rust 1.83; no MSRV pinned yet"
            )]
            instance_start_timeout: Duration::from_secs(60),
            api_socket_timeout: Duration::from_secs(30),
            #[allow(
                clippy::duration_suboptimal_units,
                reason = "from_mins requires Rust 1.83; no MSRV pinned yet"
            )]
            guest_reach_timeout: Duration::from_secs(120),
            guest_reach_poll_interval: Duration::from_secs(2),
            cleanup_grace: Duration::from_secs(5),
            firecracker_binary: PathBuf::from("firecracker"),
            min_firecracker_version: (1, 7),
            min_rootfs_bytes: 64 * 1024 * 1024,
        }
    }
}

impl RoomsConfig {
    /// API call timeout for a specific endpoint.
    #[must_use]
    pub fn timeout_for_endpoint(&self, endpoint: &str) -> Duration {
        if endpoint.contains("InstanceStart") || endpoint == "/actions" {
            self.instance_start_timeout
        } else {
            self.api_timeout
        }
    }
}
