//! Host environment checks for `rooms doctor`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::config::RoomsConfig;
#[cfg(unix)]
use crate::firecracker::{parse_getent_passwd, FIRECRACKER_USER};
use crate::rootfs::{kernel_sibling, validate_kernel, validate_rootfs};

/// Embedded checksum pins (same source as `scripts/checksums.txt`).
const CHECKSUMS_TXT: &str = include_str!("../scripts/checksums.txt");

/// Artifact names in `checksums.txt` checked against on-disk installs.
const DRIFT_ARTIFACTS: &[&str] = &[
    "firecracker-v1.10.1-x86_64",
    "jailer-v1.10.1-x86_64",
    "vmlinux-6.1.155.bin",
    "bionic.rootfs.ext4",
];

/// Schema version for `--json` output (ED-4: forward-compatible).
pub const DOCTOR_SCHEMA_VERSION: u32 = 1;

/// Prefix doctor stamps on a passing-but-non-fatal check's message.
///
/// A gate lets these through (logged); the human `rooms doctor` output renders
/// them `WARN`. The single source both surfaces key on — see
/// [`CheckResult::is_warning`].
pub const WARN_PREFIX: &str = "warn:";

/// The rooms-owned FORWARD sub-chain.
pub const ROOMS_FWD_CHAIN: &str = "ROOMS_FWD";

/// The allocator supernet every slot's /30 is carved from — the value doctor
/// checks the chain is scoped to, not mere chain existence.
pub const ROOMS_SUPERNET: &str = "172.16.0.0/24";

/// Marker comment `setup-tap.sh --host` stamps into `ROOMS_FWD`, embedding the
/// chain version + supernet so doctor keys on a version/supernet match rather
/// than existence alone.
pub const ROOMS_FWD_MARKER: &str = "rooms:fwd:v1:172.16.0.0/24";

/// Whether the host's `ROOMS_FWD` chain could be read, and if so whether it
/// matches the allocator supernet marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoomsFwdStatus {
    /// Chain present and the version/supernet marker matches.
    Installed,
    /// Chain absent, or present without the current marker (version/supernet
    /// drift). Boot must not proceed.
    Missing,
    /// Could not read the chain (no `iptables`, or insufficient privilege) — a
    /// fail-open state: the caller defers to its own root/privilege checks.
    Unprobeable,
}

/// Boot-time degraded-mode precheck: the host firewall chain must be installed
/// before a slot is claimed.
///
/// `Err(remediation)` only when the chain is *confirmed* missing; an unprobeable
/// host (non-root, no `iptables`) returns `Ok(())` so the boot's own root/KVM
/// checks surface those errors.
pub fn ensure_rooms_fwd_installed() -> Result<(), String> {
    match rooms_fwd_status() {
        RoomsFwdStatus::Installed | RoomsFwdStatus::Unprobeable => Ok(()),
        RoomsFwdStatus::Missing => Err(format!(
            "{ROOMS_FWD_CHAIN} chain not installed (or supernet drift); run `sudo bash scripts/setup-tap.sh --host` before booting a room"
        )),
    }
}

/// Probe the `ROOMS_FWD` chain, distinguishing a confirmed-missing chain from an
/// unreadable one (permission / no iptables) so callers never conflate "not set
/// up" with "run me as root".
#[must_use]
#[cfg_attr(
    not(unix),
    allow(
        clippy::missing_const_for_fn,
        reason = "non-unix body is const-trivial; the unix body shells out to iptables"
    )
)]
pub fn rooms_fwd_status() -> RoomsFwdStatus {
    #[cfg(unix)]
    {
        let output = Command::new("iptables")
            .args(["-S", ROOMS_FWD_CHAIN])
            .output();
        match output {
            Ok(out) if out.status.success() => {
                classify_rooms_fwd_dump(&String::from_utf8_lossy(&out.stdout))
            }
            Ok(out) => {
                // A missing chain exits non-zero with "No chain/target/match by
                // that name"; a privilege failure ("Permission denied ...") is
                // unprobeable, not a confirmed-missing chain.
                let stderr = String::from_utf8_lossy(&out.stderr);
                if stderr.contains("No chain") {
                    RoomsFwdStatus::Missing
                } else {
                    RoomsFwdStatus::Unprobeable
                }
            }
            // iptables not installed / not on PATH.
            Err(_) => RoomsFwdStatus::Unprobeable,
        }
    }
    #[cfg(not(unix))]
    {
        RoomsFwdStatus::Unprobeable
    }
}

/// Classify an `iptables -S ROOMS_FWD` dump.
///
/// `Installed` iff it carries the current version/supernet marker (chain exists
/// but without it → drift → `Missing`). Pure, so it's unit-testable without
/// iptables.
#[must_use]
pub fn classify_rooms_fwd_dump(dump: &str) -> RoomsFwdStatus {
    if dump.contains(ROOMS_FWD_MARKER) {
        RoomsFwdStatus::Installed
    } else {
        RoomsFwdStatus::Missing
    }
}

/// Outcome of a single doctor check.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckResult {
    pub name: String,
    pub ok: bool,
    pub message: String,
}

impl CheckResult {
    /// A *warning* — surfaced but non-fatal — is a check that passed yet whose
    /// message carries the [`WARN_PREFIX`]. A preflight gate lets warnings
    /// through (logged); only a not-`ok` check is a hard failure.
    #[must_use]
    pub fn is_warning(&self) -> bool {
        self.ok && self.message.starts_with(WARN_PREFIX)
    }
}

/// Full doctor report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoctorReport {
    pub schema_version: u32,
    pub checks: Vec<CheckResult>,
}

impl DoctorReport {
    #[must_use]
    pub fn all_ok(&self) -> bool {
        self.checks.iter().all(|c| c.ok)
    }
}

/// Run all host environment checks.
pub fn run_doctor(config: &RoomsConfig, image: Option<&Path>) -> DoctorReport {
    let checks = vec![
        check_kvm(),
        check_firecracker_version(config),
        check_jailer(config),
        check_firecracker_user(),
        check_jailer_file_access(image),
        check_tun_device(),
        check_rooms_fwd(),
        check_slots_dir(config),
        check_orphaned_taps(config),
        check_kernel(image, config),
        check_rootfs(image, config),
        check_anthropic_api_key(),
        check_nested_virt(),
        check_sha_drift(config, image),
    ];

    DoctorReport {
        schema_version: DOCTOR_SCHEMA_VERSION,
        checks,
    }
}

fn check_kvm() -> CheckResult {
    let name = "kvm".to_owned();
    #[cfg(unix)]
    {
        let path = Path::new("/dev/kvm");
        if !path.exists() {
            return CheckResult {
                name,
                ok: false,
                message: "/dev/kvm does not exist; enable KVM or nested virt".to_owned(),
            };
        }
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
        {
            Ok(_) => CheckResult {
                name,
                ok: true,
                message: "/dev/kvm present and rw-accessible".to_owned(),
            },
            Err(e) => CheckResult {
                name,
                ok: false,
                message: format!("/dev/kvm permission denied: {e}"),
            },
        }
    }
    #[cfg(not(unix))]
    {
        CheckResult {
            name,
            ok: false,
            message: "KVM checks require a Unix host".to_owned(),
        }
    }
}

fn check_firecracker_version(config: &RoomsConfig) -> CheckResult {
    let name = "firecracker".to_owned();
    let output = Command::new(&config.firecracker_binary)
        .arg("--version")
        .output();

    match output {
        Ok(out) if out.status.success() => {
            let version_str = String::from_utf8_lossy(&out.stdout).trim().to_owned();
            match parse_firecracker_version(&version_str) {
                Some((major, minor))
                    if version_meets_min(major, minor, config.min_firecracker_version) =>
                {
                    CheckResult {
                        name,
                        ok: true,
                        message: format!(
                            "firecracker {major}.{minor} (>= {}.{})",
                            config.min_firecracker_version.0, config.min_firecracker_version.1
                        ),
                    }
                }
                Some((major, minor)) => CheckResult {
                    name,
                    ok: false,
                    message: format!(
                        "firecracker {major}.{minor} is below minimum {}.{}",
                        config.min_firecracker_version.0, config.min_firecracker_version.1
                    ),
                },
                None => CheckResult {
                    name,
                    ok: false,
                    message: format!("could not parse firecracker version from: {version_str}"),
                },
            }
        }
        Ok(out) => CheckResult {
            name,
            ok: false,
            message: format!(
                "firecracker --version failed (exit {}): {}",
                out.status,
                String::from_utf8_lossy(&out.stderr)
            ),
        },
        Err(e) => CheckResult {
            name,
            ok: false,
            message: format!(
                "firecracker binary not found at {}: {e}",
                config.firecracker_binary.display()
            ),
        },
    }
}

fn check_jailer(config: &RoomsConfig) -> CheckResult {
    let name = "jailer".to_owned();
    let Some(path) = resolve_in_path(&config.jailer_binary) else {
        return CheckResult {
            name,
            ok: false,
            message: format!(
                "jailer not found on PATH (looked for {}); run scripts/setup-rooms-host.sh",
                config.jailer_binary.display()
            ),
        };
    };

    CheckResult {
        name,
        ok: true,
        message: format!("jailer present at {}", path.display()),
    }
}

fn check_firecracker_user() -> CheckResult {
    let name = "firecracker_user".to_owned();
    #[cfg(unix)]
    {
        let output = Command::new("getent")
            .args(["passwd", FIRECRACKER_USER])
            .output();
        match output {
            Ok(out) if out.status.success() => {
                let line = String::from_utf8_lossy(&out.stdout);
                match parse_getent_passwd(&line) {
                    Some((uid, gid)) => CheckResult {
                        name,
                        ok: true,
                        message: format!(
                            "{FIRECRACKER_USER} system user exists (uid {uid}, gid {gid})"
                        ),
                    },
                    None => CheckResult {
                        name,
                        ok: false,
                        message: format!(
                            "getent returned unexpected passwd line for {FIRECRACKER_USER}"
                        ),
                    },
                }
            }
            Ok(_) => CheckResult {
                name,
                ok: false,
                message: format!(
                    "system user {FIRECRACKER_USER} missing; run scripts/setup-rooms-host.sh"
                ),
            },
            Err(e) => CheckResult {
                name,
                ok: false,
                message: format!("could not run getent passwd {FIRECRACKER_USER}: {e}"),
            },
        }
    }
    #[cfg(not(unix))]
    {
        CheckResult {
            name,
            ok: false,
            message: "firecracker user checks require a Unix host".to_owned(),
        }
    }
}

// `image` is only consulted by the unix jail-access checks in this fn body.
#[cfg_attr(not(unix), allow(unused_variables, reason = "image used only on unix"))]
fn check_jailer_file_access(image: Option<&Path>) -> CheckResult {
    let name = "jailer_file_access".to_owned();
    #[cfg(unix)]
    {
        let Some(uid) = firecracker_uid() else {
            return CheckResult {
                name,
                ok: false,
                message: format!("cannot verify file access: {FIRECRACKER_USER} user missing"),
            };
        };

        let kernel = resolve_kernel_path(image);
        let rootfs = image.map_or_else(default_rootfs_path, Path::to_path_buf);

        let mut failures = Vec::new();
        if let Some(path) = kernel {
            if !path.exists() {
                failures.push(format!("kernel missing at {}", path.display()));
            } else if !path_readable_by_uid(&path, uid) {
                failures.push(format!(
                    "{FIRECRACKER_USER} cannot read kernel at {} (check group/other permissions)",
                    path.display()
                ));
            }
        } else {
            failures.push(
                "no kernel path configured; pass --image or set $HOME/rooms/images/vmlinux.bin"
                    .to_owned(),
            );
        }

        if !rootfs.exists() {
            failures.push(format!("rootfs missing at {}", rootfs.display()));
        } else if !path_readable_by_uid(&rootfs, uid) {
            failures.push(format!(
                "{FIRECRACKER_USER} cannot read rootfs at {} (check group/other permissions)",
                rootfs.display()
            ));
        }

        if failures.is_empty() {
            return CheckResult {
                name,
                ok: true,
                message: format!("{FIRECRACKER_USER} can read kernel and rootfs images"),
            };
        }

        CheckResult {
            name,
            ok: false,
            message: failures.join("; "),
        }
    }
    #[cfg(not(unix))]
    {
        CheckResult {
            name,
            ok: false,
            message: "jailer file access checks require a Unix host".to_owned(),
        }
    }
}

#[cfg(unix)]
fn firecracker_uid() -> Option<u32> {
    let output = Command::new("getent")
        .args(["passwd", FIRECRACKER_USER])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_getent_passwd(&String::from_utf8_lossy(&output.stdout)).map(|(uid, _gid)| uid)
}

#[cfg(unix)]
fn path_readable_by_uid(path: &Path, uid: u32) -> bool {
    use std::os::unix::fs::MetadataExt;

    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    let mode = meta.mode();
    if meta.uid() == uid {
        return mode & 0o400 != 0;
    }
    if meta.gid() == current_primary_gid(uid) {
        return mode & 0o040 != 0;
    }
    mode & 0o004 != 0
}

#[cfg(unix)]
fn current_primary_gid(uid: u32) -> u32 {
    let output = Command::new("getent")
        .args(["passwd", &uid.to_string()])
        .output();
    let Ok(out) = output else {
        return u32::MAX;
    };
    if !out.status.success() {
        return u32::MAX;
    }
    parse_getent_passwd(&String::from_utf8_lossy(&out.stdout)).map_or(u32::MAX, |(_uid, gid)| gid)
}

/// The `/dev/net/tun` device firecracker opens at boot to create each slot's
/// tap. The per-slot model has no persistent `tap-fc0` to probe — tap creation
/// happens in the boot path — so the standing precondition is just that the
/// firecracker user can open `/dev/net/tun`.
fn check_tun_device() -> CheckResult {
    let name = "tun_device".to_owned();
    #[cfg(unix)]
    {
        let Some(uid) = firecracker_uid() else {
            return CheckResult {
                name,
                ok: false,
                message: format!("cannot verify TAP access: {FIRECRACKER_USER} user missing"),
            };
        };
        let tun = Path::new("/dev/net/tun");
        if !tun.exists() {
            return CheckResult {
                name,
                ok: false,
                message: "/dev/net/tun missing; load the tun kernel module (sudo modprobe tun)"
                    .to_owned(),
            };
        }
        if !path_readable_by_uid(tun, uid) {
            return CheckResult {
                name,
                ok: false,
                message: format!(
                    "{FIRECRACKER_USER} cannot open /dev/net/tun; check its permissions"
                ),
            };
        }
        CheckResult {
            name,
            ok: true,
            message: format!(
                "/dev/net/tun present and openable by {FIRECRACKER_USER} (per-slot taps created at boot)"
            ),
        }
    }
    #[cfg(not(unix))]
    {
        CheckResult {
            name,
            ok: false,
            message: "TAP checks require a Unix host".to_owned(),
        }
    }
}

fn parse_firecracker_version(output: &str) -> Option<(u32, u32)> {
    for token in output.split_whitespace() {
        let trimmed = token.trim_start_matches(['v', 'V']);
        if !trimmed.contains('.') {
            continue;
        }
        let mut parts = trimmed.split('.');
        let major: u32 = parts.next()?.parse().ok()?;
        let minor: u32 = parts.next()?.parse().ok()?;
        return Some((major, minor));
    }
    None
}

const fn version_meets_min(major: u32, minor: u32, min: (u32, u32)) -> bool {
    major > min.0 || (major == min.0 && minor >= min.1)
}

fn check_kernel(image: Option<&Path>, _config: &RoomsConfig) -> CheckResult {
    let name = "kernel".to_owned();
    let Some(path) = resolve_kernel_path(image) else {
        return CheckResult {
            name,
            ok: false,
            message:
                "no kernel path configured; pass --image or set $HOME/rooms/images/vmlinux.bin"
                    .to_owned(),
        };
    };

    match validate_kernel(&path) {
        Ok(()) => CheckResult {
            name,
            ok: true,
            message: format!("kernel valid at {}", path.display()),
        },
        Err(e) => CheckResult {
            name,
            ok: false,
            message: e.to_string(),
        },
    }
}

fn check_rootfs(image: Option<&Path>, config: &RoomsConfig) -> CheckResult {
    let name = "rootfs".to_owned();
    let path = image.map_or_else(default_rootfs_path, Path::to_path_buf);

    match validate_rootfs(&path, config.min_rootfs_bytes) {
        Ok(()) => CheckResult {
            name,
            ok: true,
            message: format!("rootfs valid at {}", path.display()),
        },
        Err(e) => CheckResult {
            name,
            ok: false,
            message: e.to_string(),
        },
    }
}

fn check_anthropic_api_key() -> CheckResult {
    anthropic_api_key_result(std::env::var("ANTHROPIC_API_KEY").ok().as_deref())
}

/// Decide the Anthropic-key check from the resolved env value — policy split
/// from the env read so it's unit-testable without mutating process env.
///
/// The base substrate (boot / network / `--command` exec) runs without a key;
/// only the cursor runner path needs one. So an unset, empty, or whitespace-only
/// key is a [`WARN_PREFIX`] warning, not a failure — else the preflight gate
/// would abort substrate-only e2e on a host that merely lacks the key.
fn anthropic_api_key_result(value: Option<&str>) -> CheckResult {
    let name = "anthropic_api_key".to_owned();
    match value {
        Some(v) if !v.trim().is_empty() => CheckResult {
            name,
            ok: true,
            message: "ANTHROPIC_API_KEY is set".to_owned(),
        },
        _ => CheckResult {
            name,
            ok: true,
            message: format!(
                "{WARN_PREFIX} ANTHROPIC_API_KEY unset — required for --runner cursor, not for --command / e2e"
            ),
        },
    }
}

fn check_rooms_fwd() -> CheckResult {
    let name = "rooms_fwd".to_owned();
    match rooms_fwd_status() {
        RoomsFwdStatus::Installed => CheckResult {
            name,
            ok: true,
            message: format!("{ROOMS_FWD_CHAIN} installed and scoped to {ROOMS_SUPERNET}"),
        },
        RoomsFwdStatus::Missing => CheckResult {
            name,
            ok: false,
            message: format!(
                "{ROOMS_FWD_CHAIN} not installed or supernet drift; run `sudo bash scripts/setup-tap.sh --host`"
            ),
        },
        // Non-root / no iptables: warn (ok) rather than fail — a non-privileged
        // `rooms doctor` can't read the FORWARD table.
        RoomsFwdStatus::Unprobeable => CheckResult {
            name,
            ok: true,
            message: format!(
                "{WARN_PREFIX} could not read {ROOMS_FWD_CHAIN} (need root?); re-run `sudo rooms doctor` to verify the chain"
            ),
        },
    }
}

fn check_slots_dir(config: &RoomsConfig) -> CheckResult {
    let name = "slots_dir".to_owned();
    let Some(slots) = config.slots_dir() else {
        return CheckResult {
            name,
            ok: false,
            message: "HOME unset; cannot locate the slots dir".to_owned(),
        };
    };
    // `O_EXCL` slot claims need a local, writable filesystem. Probe writability
    // at the nearest existing ancestor — the slots dir is created lazily on
    // first claim, so its absence is fine as long as the parent accepts a file.
    let probe = nearest_existing_dir(&slots);
    match probe {
        Some(dir) if dir_is_writable(&dir) => CheckResult {
            name,
            ok: true,
            message: format!(
                "slots dir {} is writable (ensure the state base is a local fs, not NFS)",
                slots.display()
            ),
        },
        Some(dir) => CheckResult {
            name,
            ok: false,
            message: format!(
                "slots dir {} not writable ({} rejected a probe file); O_EXCL slot claims will fail",
                slots.display(),
                dir.display()
            ),
        },
        None => CheckResult {
            name,
            ok: false,
            message: format!(
                "no existing ancestor of {} to write slot claims into",
                slots.display()
            ),
        },
    }
}

fn check_orphaned_taps(config: &RoomsConfig) -> CheckResult {
    let name = "orphaned_taps".to_owned();
    let orphans = orphaned_pool_taps(config);
    if orphans.is_empty() {
        return CheckResult {
            name,
            ok: true,
            message: "no orphaned pool taps".to_owned(),
        };
    }
    let list = orphans
        .iter()
        .map(|k| format!("tap-fc{k}"))
        .collect::<Vec<_>>()
        .join(", ");
    CheckResult {
        name,
        ok: true,
        message: format!(
            "{WARN_PREFIX} orphaned tap(s) with no live slot: {list}; `rooms gc` sweeps them"
        ),
    }
}

/// Pool taps (`tap-fc<k>`, k≥1) present on the host with no claimed slot file.
///
/// The true orphans a boot-path or reconcile crash can strand (a slot file
/// removed, then its tap-delete failed). Empty on a non-unix host or when `ip
/// link show` can't be read. Shared by `doctor` (which flags them) and
/// `registry::gc` (which sweeps them), so the two never disagree.
#[must_use]
#[cfg_attr(
    not(unix),
    allow(
        clippy::missing_const_for_fn,
        reason = "non-unix body is const-trivial; the unix body shells out to ip"
    )
)]
pub fn orphaned_pool_taps(config: &RoomsConfig) -> Vec<u8> {
    #[cfg(unix)]
    {
        let claimed = claimed_slot_indices(config);
        let Ok(out) = Command::new("ip").args(["-o", "link", "show"]).output() else {
            return Vec::new();
        };
        if !out.status.success() {
            return Vec::new();
        }
        orphaned_tap_indices(&String::from_utf8_lossy(&out.stdout), &claimed)
    }
    #[cfg(not(unix))]
    {
        let _ = config;
        Vec::new()
    }
}

/// The set of currently-claimed pool slot indices, read from the slots dir. An
/// unresolvable / unreadable dir yields an empty set (best-effort).
#[cfg(unix)]
fn claimed_slot_indices(config: &RoomsConfig) -> std::collections::HashSet<u8> {
    let mut set = std::collections::HashSet::new();
    let Some(slots) = config.slots_dir() else {
        return set;
    };
    let Ok(read_dir) = std::fs::read_dir(&slots) else {
        return set;
    };
    for dirent in read_dir.flatten() {
        if let Some(name) = dirent.file_name().to_str() {
            if let Ok(index) = name.parse::<u8>() {
                if name == index.to_string() {
                    set.insert(index);
                }
            }
        }
    }
    set
}

/// The nearest existing directory at or above `path`.
fn nearest_existing_dir(path: &Path) -> Option<PathBuf> {
    let mut cur = Some(path);
    while let Some(p) = cur {
        if p.is_dir() {
            return Some(p.to_path_buf());
        }
        cur = p.parent();
    }
    None
}

/// Whether `dir` accepts a probe file — the writability signal for slot claims.
///
/// The probe file is created then removed; a rename-free `O_EXCL` create is the
/// operation slot claims actually use.
fn dir_is_writable(dir: &Path) -> bool {
    let probe = dir.join(".rooms-doctor-write-probe");
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
    {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // A stray probe from a prior run — still proves writability.
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// Parse `ip -o link show` output for `tap-fc<k>` devices (k ≥ 1) whose slot
/// index is not in `claimed`, sorted ascending. Pure, so it's unit-testable
/// without touching the host's links.
#[must_use]
pub fn orphaned_tap_indices<S: std::hash::BuildHasher>(
    ip_link_output: &str,
    claimed: &std::collections::HashSet<u8, S>,
) -> Vec<u8> {
    let mut orphans: Vec<u8> = ip_link_output
        .lines()
        .filter_map(parse_tap_index)
        .filter(|k| !claimed.contains(k))
        .collect();
    orphans.sort_unstable();
    orphans.dedup();
    orphans
}

/// Extract a `tap-fc<k>` (k ≥ 1) slot index from one `ip -o link show` line.
/// The device name is the second whitespace-colon token: `N: tap-fc3: <...>`.
fn parse_tap_index(line: &str) -> Option<u8> {
    let dev = line.split_whitespace().nth(1)?.trim_end_matches(':');
    // `ip -o` sometimes suffixes `@if<n>` for linked devices; strip it.
    let dev = dev.split('@').next()?;
    let suffix = dev.strip_prefix("tap-fc")?;
    let index: u8 = suffix.parse().ok()?;
    // Slot 0 is the reserved legacy shared tap — never an orphan candidate.
    (index >= 1).then_some(index)
}

fn check_nested_virt() -> CheckResult {
    let name = "nested_virt".to_owned();

    // Try kvm-ok first. Trust the exit status — the string match was a
    // double-positive that would have flipped "nested virtualisation not
    // enabled" stderr into a "ok" result.
    if let Ok(out) = Command::new("kvm-ok").output() {
        if out.status.success() {
            return CheckResult {
                name,
                ok: true,
                message: "nested virtualization appears enabled (kvm-ok)".to_owned(),
            };
        }
    }

    // Fall back to sysfs knobs.
    for path in [
        "/sys/module/kvm_intel/parameters/nested",
        "/sys/module/kvm_amd/parameters/nested",
    ] {
        if let Ok(content) = std::fs::read_to_string(path) {
            let val = content.trim();
            let enabled = val == "Y" || val == "1" || val == "on";
            return CheckResult {
                name,
                ok: enabled,
                message: if enabled {
                    format!("nested virt enabled ({path}={val})")
                } else {
                    format!("nested virt disabled ({path}={val})")
                },
            };
        }
    }

    CheckResult {
        name,
        ok: false,
        message: "could not determine nested virt status (kvm-ok and sysfs probes failed)"
            .to_owned(),
    }
}

fn resolve_kernel_path(image: Option<&Path>) -> Option<PathBuf> {
    if let Some(img) = image {
        return kernel_sibling(img);
    }
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join("rooms/images/vmlinux.bin"))
}

fn default_rootfs_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| String::from("/tmp"));
    PathBuf::from(home).join("rooms/images/rootfs.ext4")
}

fn parse_checksums(content: &str) -> HashMap<String, String> {
    let mut pins = HashMap::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let Some(digest) = parts.next() else {
            continue;
        };
        let Some(name) = parts.next() else {
            continue;
        };
        if digest.len() == 64 && digest.chars().all(|c| c.is_ascii_hexdigit()) {
            pins.insert(name.to_owned(), digest.to_ascii_lowercase());
        }
    }
    pins
}

fn file_sha256(path: &Path) -> Result<String, String> {
    let output = Command::new("sha256sum")
        .arg(path)
        .output()
        .map_err(|e| format!("sha256sum failed for {}: {e}", path.display()))?;
    if !output.status.success() {
        return Err(format!(
            "sha256sum exited {} for {}",
            output.status,
            path.display()
        ));
    }
    let line = String::from_utf8_lossy(&output.stdout);
    line.split_whitespace()
        .next()
        .map(str::to_ascii_lowercase)
        .ok_or_else(|| format!("sha256sum produced no digest for {}", path.display()))
}

fn drift_target_path(
    artifact: &str,
    config: &RoomsConfig,
    image: Option<&Path>,
) -> Option<PathBuf> {
    match artifact {
        "firecracker-v1.10.1-x86_64" => resolve_in_path(&config.firecracker_binary),
        // jailer installs alongside firecracker (setup-rooms-host.sh) and is a
        // security-boundary binary, so cover its pin too. It is not in
        // RoomsConfig, so resolve the conventional name on PATH.
        "jailer-v1.10.1-x86_64" => resolve_in_path(Path::new("jailer")),
        "vmlinux-6.1.155.bin" => resolve_kernel_path(image),
        // The bionic pin only applies to the quickstart download at its default
        // path — never to an arbitrary --image, which may be a built agent
        // rootfs that legitimately differs from the bionic digest.
        "bionic.rootfs.ext4" => Some(default_rootfs_path()),
        _ => None,
    }
}

/// Resolve a binary to a concrete path. Absolute or directory-qualified paths
/// are used as-is; a bare name is searched on `PATH`, so an installed
/// `firecracker` in `/usr/local/bin` is hashed for drift rather than silently
/// skipped because the bare name does not exist relative to the cwd.
fn resolve_in_path(binary: &Path) -> Option<PathBuf> {
    if binary.is_absolute() || binary.components().count() > 1 {
        return binary.exists().then(|| binary.to_path_buf());
    }
    let path_var = std::env::var_os("PATH")?;
    std::env::split_paths(&path_var)
        .map(|dir| dir.join(binary))
        .find(|candidate| candidate.is_file())
}

fn check_sha_drift(config: &RoomsConfig, image: Option<&Path>) -> CheckResult {
    check_sha_drift_with(config, image, drift_target_path)
}

fn check_sha_drift_with<F>(
    config: &RoomsConfig,
    image: Option<&Path>,
    resolve_target: F,
) -> CheckResult
where
    F: Fn(&str, &RoomsConfig, Option<&Path>) -> Option<PathBuf>,
{
    let name = "sha_drift".to_owned();
    let pins = parse_checksums(CHECKSUMS_TXT);
    let mut warnings = Vec::new();
    let mut checked = 0u32;

    for artifact in DRIFT_ARTIFACTS {
        let Some(expected) = pins.get(*artifact) else {
            warnings.push(format!(
                "no checksum pin for {artifact} in embedded checksums"
            ));
            continue;
        };
        let Some(path) = resolve_target(artifact, config, image) else {
            continue;
        };
        if !path.exists() {
            continue;
        }
        checked += 1;
        match file_sha256(&path) {
            Ok(actual) if actual == *expected => {}
            Ok(actual) => warnings.push(format!(
                "sha256 drift: {artifact} at {} (expected {expected}, got {actual})",
                path.display()
            )),
            Err(e) => warnings.push(format!(
                "sha256 check failed: {artifact} at {} ({e})",
                path.display()
            )),
        }
    }

    if warnings.is_empty() {
        return CheckResult {
            name,
            ok: true,
            message: if checked == 0 {
                "no pinned artifacts present to verify".to_owned()
            } else {
                format!("{checked} pinned artifact(s) match checksums.txt")
            },
        };
    }

    CheckResult {
        name,
        ok: true,
        message: format!("{WARN_PREFIX} {}", warnings.join("; ")),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, reason = "test module")]

    use super::{
        anthropic_api_key_result, check_sha_drift, check_sha_drift_with, default_rootfs_path,
        drift_target_path, parse_checksums, parse_firecracker_version, version_meets_min,
        RoomsConfig,
    };
    use std::path::PathBuf;

    #[test]
    fn classify_rooms_fwd_dump_keys_on_the_supernet_marker() {
        use super::{classify_rooms_fwd_dump, RoomsFwdStatus, ROOMS_FWD_MARKER};
        let installed = format!(
            "-N ROOMS_FWD\n-A ROOMS_FWD -s 172.16.0.0/24 -m comment --comment \"{ROOMS_FWD_MARKER}\" -j DROP\n"
        );
        assert_eq!(
            classify_rooms_fwd_dump(&installed),
            RoomsFwdStatus::Installed
        );
        // A chain that exists but lacks the current marker is drift → Missing,
        // never a false Installed.
        assert_eq!(
            classify_rooms_fwd_dump("-N ROOMS_FWD\n-A ROOMS_FWD -j DROP\n"),
            RoomsFwdStatus::Missing
        );
        // An older-version marker must not satisfy the current supernet check.
        assert_eq!(
            classify_rooms_fwd_dump(
                "-A ROOMS_FWD -m comment --comment \"rooms:fwd:v0:10.0.0.0/8\" -j DROP\n"
            ),
            RoomsFwdStatus::Missing
        );
    }

    #[test]
    fn orphaned_tap_indices_flags_only_unclaimed_pool_taps() {
        use super::orphaned_tap_indices;
        use std::collections::HashSet;
        let dump = "\
1: lo: <LOOPBACK,UP> mtu 65536 qdisc noqueue state UNKNOWN\n\
2: eth0: <BROADCAST,MULTICAST,UP> mtu 1500 qdisc fq state UP\n\
3: tap-fc0: <BROADCAST,MULTICAST,UP> mtu 1500 qdisc pfifo_fast state DOWN\n\
4: tap-fc1: <BROADCAST,MULTICAST,UP> mtu 1500 qdisc pfifo_fast state DOWN\n\
5: tap-fc2: <BROADCAST,MULTICAST,UP> mtu 1500 qdisc pfifo_fast state DOWN\n\
6: tap-fc7@if9: <BROADCAST,MULTICAST,UP> mtu 1500 qdisc pfifo_fast state DOWN\n";
        // Slot 1 is claimed; taps 0 (legacy, never an orphan), 2, and 7 are not.
        let claimed: HashSet<u8> = std::iter::once(1u8).collect();
        assert_eq!(orphaned_tap_indices(dump, &claimed), vec![2, 7]);
        // With every pool tap claimed, none are orphaned.
        let all: HashSet<u8> = [1u8, 2, 7].into_iter().collect();
        assert!(orphaned_tap_indices(dump, &all).is_empty());
    }

    #[test]
    fn parses_firecracker_version_string() {
        assert_eq!(
            parse_firecracker_version("Firecracker v1.7.0"),
            Some((1, 7))
        );
        assert_eq!(parse_firecracker_version("v2.1.3"), Some((2, 1)));
    }

    #[test]
    fn suffix_attached_to_patch_still_parses_major_minor() {
        assert_eq!(
            parse_firecracker_version("Firecracker v1.10.1-dirty"),
            Some((1, 10))
        );
        assert_eq!(parse_firecracker_version("v2.0.5_custom"), Some((2, 0)));
    }

    #[test]
    fn version_meets_minimum() {
        assert!(version_meets_min(1, 7, (1, 7)));
        assert!(version_meets_min(2, 0, (1, 7)));
        assert!(!version_meets_min(1, 6, (1, 7)));
    }

    #[test]
    fn parses_checksums_skips_comments_and_blanks() {
        let digest = "a".repeat(64);
        let input = format!("# comment\n\n{digest}  artifact-a\n");
        let pins = parse_checksums(&input);
        assert_eq!(pins.get("artifact-a"), Some(&digest));
    }

    #[test]
    fn sha_drift_reports_ok_when_no_artifacts_present() {
        let config = RoomsConfig {
            firecracker_binary: PathBuf::from("/nonexistent/firecracker"),
            ..RoomsConfig::default()
        };
        let result = check_sha_drift_with(&config, None, |_, _, _| None);
        assert!(result.ok, "missing artifacts should warn-only, not fail");
        assert!(
            result.message.contains("no pinned artifacts present"),
            "unexpected message: {}",
            result.message
        );
    }

    #[test]
    fn sha_drift_warns_on_mismatch_not_fail() {
        let dir = tempfile::tempdir().expect("tempdir");
        let image = dir.path().join("rootfs.ext4");
        std::fs::write(&image, b"stub-rootfs").expect("write rootfs stub");
        let kernel_path = dir.path().join("vmlinux.bin");
        std::fs::write(&kernel_path, b"not-the-real-kernel").expect("write kernel stub");

        let config = RoomsConfig::default();
        let result = check_sha_drift(&config, Some(&image));

        assert!(
            result.ok,
            "sha drift must warn, not fail doctor: {}",
            result.message
        );
        assert!(
            result.message.contains("warn:") && result.message.contains("drift"),
            "expected warn-level drift message, got: {}",
            result.message
        );
    }

    #[test]
    fn bionic_drift_ignores_image_override() {
        // `--image` may point at a built agent rootfs that legitimately differs
        // from the bionic pin; the bionic drift target must stay the default
        // quickstart path regardless, so it never spuriously warns.
        let config = RoomsConfig::default();
        let custom = PathBuf::from("/custom/agent-alpine.ext4");
        let target = drift_target_path("bionic.rootfs.ext4", &config, Some(custom.as_path()));
        assert_ne!(
            target.as_deref(),
            Some(custom.as_path()),
            "bionic pin must not follow --image"
        );
        assert_eq!(
            target,
            Some(default_rootfs_path()),
            "bionic pin must resolve to the default quickstart rootfs path"
        );
    }

    #[test]
    fn anthropic_api_key_unset_empty_or_blank_warns_not_fails() {
        // The base substrate runs without a key, so an unset, empty, or
        // whitespace-only key is a WARN (ok=true + `warn:` prefix), never a hard
        // FAIL that would abort the preflight gate on substrate-only e2e.
        for value in [None, Some(""), Some("   ")] {
            let result = anthropic_api_key_result(value);
            assert!(result.ok, "unset/blank key must not fail: {value:?}");
            assert!(
                result.is_warning(),
                "unset/blank key must be a WARN, got: {}",
                result.message
            );
        }
    }

    #[test]
    fn anthropic_api_key_set_is_a_plain_pass() {
        let result = anthropic_api_key_result(Some("sk-ant-example"));
        assert!(result.ok);
        assert!(
            !result.is_warning(),
            "a set key is a pass, not a warn: {}",
            result.message
        );
    }

    mod version_parser_properties {
        use proptest::prelude::*;

        use super::parse_firecracker_version;

        proptest! {
            #[test]
            fn firecracker_banner_round_trips(
                major in 0u32..=99,
                minor in 0u32..=99,
                patch in 0u32..=999,
            ) {
                let output = format!("Firecracker v{major}.{minor}.{patch}");
                prop_assert_eq!(parse_firecracker_version(&output), Some((major, minor)));
            }

            #[test]
            fn v_prefix_round_trips(
                major in 0u32..=99,
                minor in 0u32..=99,
                patch in 0u32..=99,
            ) {
                let output = format!("v{major}.{minor}.{patch}");
                prop_assert_eq!(parse_firecracker_version(&output), Some((major, minor)));
            }

            #[test]
            fn trailing_junk_still_parses_version(
                major in 1u32..=20,
                minor in 0u32..=20,
                patch in 0u32..=20,
                junk in "\\s+[A-Za-z0-9._-]+",
            ) {
                let output = format!("Firecracker v{major}.{minor}.{patch}{junk}");
                prop_assert_eq!(parse_firecracker_version(&output), Some((major, minor)));
            }

            #[test]
            fn tokens_without_dots_return_none(words in proptest::collection::vec("[^\\.\\s]+", 1..6)) {
                let output = words.join(" ");
                prop_assert_eq!(parse_firecracker_version(&output), None);
            }
        }

        #[test]
        fn adversarial_inputs_return_none() {
            for input in [
                "",
                "Firecracker",
                "no version token here",
                "v.",
                "v1.",
                "v.not_a_number.2",
            ] {
                assert_eq!(parse_firecracker_version(input), None, "input: {input:?}");
            }
        }
    }
}
