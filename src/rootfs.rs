//! Rootfs image validation helpers.

#![allow(
    clippy::missing_const_for_fn,
    reason = "unmount_overlay is cfg-gated and not const on unix"
)]

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::RootfsError;

/// Minimum ext4 superblock size for reading the magic number.
const SUPERBLOCK_OFFSET: u64 = 1024;
const EXT4_MAGIC: u16 = 0xEF53;

/// Validate that `path` exists, is ext4, and meets the minimum size.
pub fn validate_rootfs(path: &Path, min_bytes: u64) -> Result<(), RootfsError> {
    if !path.exists() {
        return Err(RootfsError::NotFound {
            path: path.to_path_buf(),
        });
    }

    let meta = std::fs::metadata(path)?;
    let size = meta.len();
    if size < min_bytes {
        return Err(RootfsError::TooSmall {
            path: path.to_path_buf(),
            size,
            min_bytes,
        });
    }

    let magic = read_ext4_magic(path)?;
    if magic != EXT4_MAGIC {
        return Err(RootfsError::NotExt4 {
            path: path.to_path_buf(),
            magic,
        });
    }

    Ok(())
}

/// Validate that `path` exists and has a valid ELF header.
pub fn validate_kernel(path: &Path) -> Result<(), RootfsError> {
    if !path.exists() {
        return Err(RootfsError::KernelNotFound {
            path: path.to_path_buf(),
        });
    }

    let mut header = [0_u8; 4];
    let mut file = std::fs::File::open(path)?;
    file.read_exact(&mut header)?;
    if header != [0x7F, b'E', b'L', b'F'] {
        return Err(RootfsError::KernelNotElf {
            path: path.to_path_buf(),
        });
    }

    Ok(())
}

/// Fail-closed admission for a snapshot-capable base image.
///
/// The immutable lower layer must contain the overlay entry point and must not
/// contain a baked SSH host private key. The latter cannot be repaired by
/// deleting a key after boot: its bytes may already have reached guest memory.
pub fn validate_snapshot_base_image(path: &Path) -> Result<(), String> {
    let overlay = debugfs(path, "stat /sbin/overlay-init")?;
    if overlay.contains("File not found") || !overlay.contains("Inode:") {
        return Err(format!(
            "snapshot base image {} lacks /sbin/overlay-init",
            path.display()
        ));
    }

    let ssh_dir = debugfs(path, "ls -p /etc/ssh")?;
    if let Some(key) = baked_host_private_key(&ssh_dir) {
        return Err(format!(
            "snapshot base image {} contains baked SSH host private key /etc/ssh/{key}",
            path.display()
        ));
    }
    Ok(())
}

fn debugfs(path: &Path, request: &str) -> Result<String, String> {
    let output = Command::new("debugfs")
        .args(["-R", request])
        .arg(path)
        .output()
        .map_err(|e| format!("inspect snapshot base image with debugfs: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "inspect snapshot base image {} ({request}): {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn baked_host_private_key(listing: &str) -> Option<&str> {
    listing.split(['/', '\n']).map(str::trim).find(|entry| {
        entry.starts_with("ssh_host_")
            && !Path::new(entry)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("pub"))
    })
}

/// Conventional kernel path sibling to a rootfs image.
#[must_use]
pub fn kernel_sibling(rootfs: &Path) -> Option<PathBuf> {
    rootfs.parent().map(|p| p.join("vmlinux.bin"))
}

fn read_ext4_magic(path: &Path) -> Result<u16, RootfsError> {
    use std::io::{Seek, SeekFrom};

    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(SUPERBLOCK_OFFSET + 56))?;
    let mut magic_bytes = [0_u8; 2];
    file.read_exact(&mut magic_bytes)?;
    Ok(u16::from_le_bytes(magic_bytes))
}

/// Best-effort unmount of a per-room overlay mount point.
pub fn unmount_overlay(mount_point: &Path) {
    #[cfg(unix)]
    {
        use std::process::Command;
        if mount_point.exists() {
            let _ = Command::new("umount").arg(mount_point).output();
            let _ = std::fs::remove_dir_all(mount_point);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = mount_point;
    }
}

#[cfg(test)]
mod tests {
    use super::baked_host_private_key;

    #[test]
    fn private_host_keys_are_rejected_but_public_halves_are_not() {
        let public_only = "/12/100644/0/0/ssh_host_ed25519_key.pub/99/\n";
        assert_eq!(baked_host_private_key(public_only), None);

        let with_private = concat!(
            "/12/100644/0/0/ssh_host_ed25519_key.pub/99/\n",
            "/13/100600/0/0/ssh_host_ed25519_key/411/\n"
        );
        assert_eq!(
            baked_host_private_key(with_private),
            Some("ssh_host_ed25519_key")
        );
    }

    #[test]
    fn snapshot_rootfs_scripts_encode_the_neutral_base_shape() {
        let build = include_str!("../scripts/build-rootfs-alpine.sh");
        assert!(build.contains("rm -f /etc/ssh/ssh_host_*"));
        assert!(!build.contains("\nssh-keygen -A\n"));
        assert!(build.contains("rc-update add rooms-provision boot"));

        let overlay = include_str!("../scripts/lib/overlay-init.sh");
        assert!(overlay.contains("rooms.base=1"));
        assert!(overlay.contains("runlevels/default/sshd"));
        assert!(overlay.contains("runlevels/boot/rooms-secrets"));

        let agent = include_str!("../scripts/lib/rooms-provision-agent.sh");
        assert!(agent.contains("env -i HOME=/home/rooms"));
        assert!(agent.contains("ipv6_is_disabled"));
        assert!(agent.contains("retained_processes_are_safe"));
        assert!(agent.contains("VSOCK-CONNECT:2:5002"));
    }
}
