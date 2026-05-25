//! Rootfs image validation helpers.

#![allow(
    clippy::missing_const_for_fn,
    reason = "unmount_overlay is cfg-gated and not const on unix"
)]

use std::io::Read;
use std::path::{Path, PathBuf};

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
