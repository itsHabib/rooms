//! Host environment checks for `rooms doctor`.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Serialize;

use crate::config::RoomsConfig;
use crate::rootfs::{kernel_sibling, validate_kernel, validate_rootfs};

/// Schema version for `--json` output (ED-4: forward-compatible).
pub const DOCTOR_SCHEMA_VERSION: u32 = 1;

/// Outcome of a single doctor check.
#[derive(Debug, Clone, Serialize)]
pub struct CheckResult {
    pub name: String,
    pub ok: bool,
    pub message: String,
}

/// Full doctor report.
#[derive(Debug, Clone, Serialize)]
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
        check_kernel(image, config),
        check_rootfs(image, config),
        check_anthropic_api_key(),
        check_tap_roundtrip(),
        check_nested_virt(),
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
    let name = "anthropic_api_key".to_owned();
    match std::env::var("ANTHROPIC_API_KEY") {
        Ok(v) if !v.is_empty() => CheckResult {
            name,
            ok: true,
            message: "ANTHROPIC_API_KEY is set".to_owned(),
        },
        _ => CheckResult {
            name,
            ok: false,
            message: "ANTHROPIC_API_KEY not set in environment".to_owned(),
        },
    }
}

fn check_tap_roundtrip() -> CheckResult {
    let name = "tap".to_owned();
    #[cfg(unix)]
    {
        let tap = "rooms-doctor-probe";
        let add = Command::new("ip")
            .args(["tuntap", "add", "dev", tap, "mode", "tap"])
            .output();
        match add {
            Ok(out) if out.status.success() => {
                let del = Command::new("ip")
                    .args(["tuntap", "del", "dev", tap, "mode", "tap"])
                    .output();
                match del {
                    Ok(d) if d.status.success() => CheckResult {
                        name,
                        ok: true,
                        message: "ip tuntap add/del round-trip succeeded".to_owned(),
                    },
                    Ok(d) => CheckResult {
                        name,
                        ok: false,
                        message: format!(
                            "ip tuntap del failed (exit {}): {}",
                            d.status,
                            String::from_utf8_lossy(&d.stderr)
                        ),
                    },
                    Err(e) => CheckResult {
                        name,
                        ok: false,
                        message: format!("ip tuntap del failed: {e}"),
                    },
                }
            }
            Ok(out) => CheckResult {
                name,
                ok: false,
                message: format!(
                    "ip tuntap add failed (exit {}): {}",
                    out.status,
                    String::from_utf8_lossy(&out.stderr)
                ),
            },
            Err(e) => CheckResult {
                name,
                ok: false,
                message: format!("ip command not available: {e}"),
            },
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

fn check_nested_virt() -> CheckResult {
    let name = "nested_virt".to_owned();

    // Try kvm-ok first.
    if let Ok(out) = Command::new("kvm-ok").output() {
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        let combined = format!("{stdout}{stderr}");
        if out.status.success() || combined.contains("enabled") {
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, reason = "test module")]

    use super::{parse_firecracker_version, version_meets_min};

    #[test]
    fn parses_firecracker_version_string() {
        assert_eq!(
            parse_firecracker_version("Firecracker v1.7.0"),
            Some((1, 7))
        );
        assert_eq!(parse_firecracker_version("v2.1.3"), Some((2, 1)));
    }

    #[test]
    fn version_meets_minimum() {
        assert!(version_meets_min(1, 7, (1, 7)));
        assert!(version_meets_min(2, 0, (1, 7)));
        assert!(!version_meets_min(1, 6, (1, 7)));
    }
}
