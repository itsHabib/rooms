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
    /// Path to the jailer binary (default: `"jailer"` on PATH).
    pub jailer_binary: PathBuf,
    /// Base directory for jailer chroot jails (default: `<state_base>/jailer`).
    pub jailer_chroot_base: Option<PathBuf>,
    /// Base directory for all room state (default: `$HOME/.local/state/rooms`).
    /// Overriding it (e.g. to a tempdir) redirects every room path — the seam
    /// that makes the registry hermetically testable.
    pub state_base: Option<PathBuf>,
    /// Minimum supported Firecracker semver major.minor.
    pub min_firecracker_version: (u32, u32),
    /// Minimum rootfs image size in bytes.
    pub min_rootfs_bytes: u64,
    /// Host-global pool ceiling: the most rooms allowed to hold slots at once.
    /// `slots/` is host-global, so this — not a per-caller flag — is the source
    /// of truth; a `--max-pool` / `ROOMS_MAX_POOL` override can only lower it.
    pub max_pool: u8,
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
            jailer_binary: PathBuf::from("jailer"),
            jailer_chroot_base: None,
            state_base: None,
            min_firecracker_version: (1, 7),
            min_rootfs_bytes: 64 * 1024 * 1024,
            max_pool: crate::slot::DEFAULT_MAX_POOL,
        }
    }
}

impl RoomsConfig {
    /// The base directory holding every room's state. Honors `state_base`,
    /// else `$HOME/.local/state/rooms`. `None` only when neither is set (HOME
    /// unset); callers map that to their own layer's error.
    pub fn resolved_state_base(&self) -> Option<PathBuf> {
        if let Some(base) = &self.state_base {
            return Some(base.clone());
        }
        std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state/rooms"))
    }

    /// Per-room state dir: `<state_base>/<id>` — holds `firecracker.log` and
    /// `room.json`.
    pub fn room_dir(&self, id: &str) -> Option<PathBuf> {
        Some(self.resolved_state_base()?.join(id))
    }

    /// Pool slot directory: `<state_base>/slots` — one `O_EXCL` file per claimed
    /// slot. `None` only when the base can't resolve (HOME unset).
    pub fn slots_dir(&self) -> Option<PathBuf> {
        Some(self.resolved_state_base()?.join(crate::slot::SLOTS_DIR))
    }

    /// The pool ceiling for one invocation. The host cap ([`Self::max_pool`]) is
    /// the source of truth; a per-invocation `flag` (`--max-pool` /
    /// `ROOMS_MAX_POOL`) can only lower it, never raise it. The result is
    /// clamped to the addressing ceiling ([`crate::slot::MAX_SLOT`]) so a
    /// misconfigured host cap can never drive a claim off the /24 carve.
    #[must_use]
    pub fn effective_max_pool(&self, flag: Option<u8>) -> u8 {
        flag.unwrap_or(self.max_pool)
            .min(self.max_pool)
            .min(crate::slot::MAX_SLOT)
    }

    /// Jailer chroot base: the `jailer_chroot_base` override, else
    /// `<state_base>/jailer`.
    pub fn chroot_base(&self) -> Option<PathBuf> {
        if let Some(base) = &self.jailer_chroot_base {
            return Some(base.clone());
        }
        Some(self.resolved_state_base()?.join("jailer"))
    }

    // The three below mirror the jail layout staged in `firecracker` — kept in
    // lockstep by `config_paths_match_jail_layout` so the registry and the
    // booter resolve identical paths.

    /// Jail instance dir: `<chroot_base>/firecracker/<id>`.
    pub fn jail_instance_dir(&self, id: &str) -> Option<PathBuf> {
        Some(self.chroot_base()?.join("firecracker").join(id))
    }

    /// Jail root (the chroot): `<jail_instance_dir>/root`.
    pub fn jail_root_dir(&self, id: &str) -> Option<PathBuf> {
        Some(self.jail_instance_dir(id)?.join("root"))
    }

    /// Firecracker API socket: `<jail_root>/api.sock`.
    pub fn jail_socket(&self, id: &str) -> Option<PathBuf> {
        Some(self.jail_root_dir(id)?.join("api.sock"))
    }

    /// API call timeout for a specific endpoint.
    #[must_use]
    pub fn timeout_for_endpoint(&self, endpoint: &str) -> Duration {
        // Firecracker endpoint paths don't embed the action type
        // (`InstanceStart` lives in the JSON body, not the URL), so the
        // only call that ever hits `/actions` is the boot trigger.
        if endpoint == "/actions" {
            self.instance_start_timeout
        } else {
            self.api_timeout
        }
    }
}

#[cfg(test)]
mod tests {
    use super::RoomsConfig;
    use std::path::PathBuf;

    fn with_base(base: &str) -> RoomsConfig {
        RoomsConfig {
            state_base: Some(PathBuf::from(base)),
            ..RoomsConfig::default()
        }
    }

    #[test]
    fn paths_derive_from_state_base() {
        let c = with_base("/s");
        let id = "01abcdefghijklmnopqrstuvwx";
        assert_eq!(c.resolved_state_base(), Some(PathBuf::from("/s")));
        assert_eq!(c.room_dir(id), Some(PathBuf::from(format!("/s/{id}"))));
        assert_eq!(c.chroot_base(), Some(PathBuf::from("/s/jailer")));
    }

    #[test]
    fn config_paths_match_jail_layout() {
        // Pins the registry's view of a room's dirs to the layout `firecracker`
        // stages; a drift here would point gc at the wrong tree.
        let c = with_base("/s");
        let id = "01abcdefghijklmnopqrstuvwx";
        assert_eq!(
            c.jail_instance_dir(id),
            Some(PathBuf::from(format!("/s/jailer/firecracker/{id}")))
        );
        assert_eq!(
            c.jail_root_dir(id),
            Some(PathBuf::from(format!("/s/jailer/firecracker/{id}/root")))
        );
        assert_eq!(
            c.jail_socket(id),
            Some(PathBuf::from(format!(
                "/s/jailer/firecracker/{id}/root/api.sock"
            )))
        );
    }

    #[test]
    fn effective_max_pool_defaults_to_the_host_cap() {
        // No flag → the host cap (default 8) is walked as-is.
        let c = RoomsConfig::default();
        assert_eq!(c.max_pool, crate::slot::DEFAULT_MAX_POOL);
        assert_eq!(
            c.effective_max_pool(None),
            crate::slot::DEFAULT_MAX_POOL,
            "with no override the effective cap is the host cap"
        );
    }

    #[test]
    fn max_pool_flag_only_lowers_never_raises() {
        // Host cap 8: a lower flag wins; a higher flag clamps back down to it —
        // the cap is a host fact, a per-caller flag can't raise it.
        let c = with_base("/s");
        assert_eq!(c.max_pool, 8);
        assert_eq!(
            c.effective_max_pool(Some(3)),
            3,
            "a lower flag lowers the cap"
        );
        assert_eq!(
            c.effective_max_pool(Some(20)),
            8,
            "a flag above the host cap clamps down to it"
        );
    }

    #[test]
    fn effective_max_pool_clamps_to_the_addressing_ceiling() {
        // A host cap misconfigured past the /24 carve can never drive a claim
        // off the addressing ceiling — flag or no flag.
        let c = RoomsConfig {
            max_pool: 200,
            ..RoomsConfig::default()
        };
        assert_eq!(c.effective_max_pool(None), crate::slot::MAX_SLOT);
        assert_eq!(c.effective_max_pool(Some(u8::MAX)), crate::slot::MAX_SLOT);
    }

    #[test]
    fn jailer_chroot_base_override_wins() {
        let c = RoomsConfig {
            state_base: Some(PathBuf::from("/s")),
            jailer_chroot_base: Some(PathBuf::from("/custom/jail")),
            ..RoomsConfig::default()
        };
        assert_eq!(c.chroot_base(), Some(PathBuf::from("/custom/jail")));
        // room_dir still tracks state_base, independent of the chroot override.
        assert_eq!(c.room_dir("x"), Some(PathBuf::from("/s/x")));
    }
}
