//! Admit a sealed, host-built Nix closure for a cold room.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::Deserialize;
use sha2::{Digest, Sha256};

fn manifest_file(path: &Path) -> anyhow::Result<File> {
    #[cfg(target_os = "linux")]
    {
        use rustix::fs::{open, Mode, OFlags};
        let descriptor = open(
            path,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
            Mode::empty(),
        )?;
        let file = File::from(descriptor);
        anyhow::ensure!(
            file.metadata()?.is_file(),
            "toolstore manifest is not a regular file"
        );
        Ok(file)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
        anyhow::bail!("toolstores require a Linux host")
    }
}

#[derive(Deserialize)]
struct Manifest {
    schema_version: u32,
    system: String,
    sha256: String,
}

impl Manifest {
    fn validate(&self, system: &str) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.schema_version == 1,
            "unsupported toolstore manifest version"
        );
        anyhow::ensure!(
            self.system == system,
            "toolstore architecture mismatch: expected {system}, found {}",
            self.system
        );
        anyhow::ensure!(
            self.sha256.len() == 64
                && self
                    .sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
            "invalid toolstore SHA-256"
        );
        Ok(())
    }
}

/// An immutable inode held open across admission and jail attachment. A path
/// replacement cannot substitute another disk after its digest was checked.
pub struct Toolstore {
    file: File,
    digest: String,
}

impl Toolstore {
    /// Check the architecture, kernel seal, filesystem magic and complete hash
    /// before a caller claims any room resources.
    pub fn open(directory: &Path) -> anyhow::Result<Self> {
        let manifest_path = directory.join("meta.json");
        let mut bytes = Vec::new();
        manifest_file(&manifest_path)
            .with_context(|| format!("open toolstore manifest {}", manifest_path.display()))?
            .take(1_048_577)
            .read_to_end(&mut bytes)?;
        anyhow::ensure!(bytes.len() <= 1_048_576, "toolstore manifest exceeds 1 MiB");
        let manifest: Manifest =
            serde_json::from_slice(&bytes).context("parse toolstore manifest")?;
        manifest.validate(&format!("{}-linux", std::env::consts::ARCH))?;
        let path = directory.join("toolstore.sqfs");
        let mut file = crate::inode_seal::open_regular(&path, "toolstore")?;
        let mut magic = [0; 4];
        file.read_exact(&mut magic)?;
        anyhow::ensure!(
            &magic == b"hsqs",
            "toolstore is not a little-endian squashfs image"
        );
        let mut hash = Sha256::new();
        hash.update(magic);
        let mut buffer = [0; 16_384];
        loop {
            let count = file.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            // Read bounds count; get follows the crate's indexing restriction.
            hash.update(
                buffer
                    .get(..count)
                    .context("toolstore read exceeded buffer")?,
            );
        }
        anyhow::ensure!(
            format!("{:x}", hash.finalize()) == manifest.sha256,
            "toolstore hash mismatch"
        );
        crate::inode_seal::require_file(&file, &path, "toolstore")?;
        Ok(Self {
            file,
            digest: manifest.sha256,
        })
    }

    /// The digest verified from the held inode, suitable for a run receipt.
    #[must_use]
    pub fn digest(&self) -> &str {
        &self.digest
    }

    /// Linux mount(8) resolves the parent's descriptor rather than reopening
    /// the caller's original path. Keep this object alive until binding ends.
    fn mount_source(&self) -> anyhow::Result<PathBuf> {
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;
            let path = PathBuf::from(format!(
                "/proc/{}/fd/{}",
                std::process::id(),
                self.file.as_raw_fd()
            ));
            crate::inode_seal::require_file(&self.file, &path, "toolstore")?;
            Ok(path)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = &self.file;
            anyhow::bail!("toolstores require a Linux host")
        }
    }

    /// Bind the descriptor without mount(8) resolving it back to a pathname,
    /// then check the mounted inode before Firecracker can open it.
    pub(crate) fn bind_into(&self, target: &Path) -> anyhow::Result<()> {
        let output = std::process::Command::new("mount")
            .args(["--no-canonicalize", "--bind"])
            .arg(self.mount_source()?)
            .arg(target)
            .output()
            .context("bind verified toolstore descriptor")?;
        anyhow::ensure!(
            output.status.success(),
            "bind toolstore failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        self.verify_attachment(target)
    }

    fn verify_attachment(&self, target: &Path) -> anyhow::Result<()> {
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::fs::MetadataExt;
            let mounted = std::fs::metadata(target)?;
            let admitted = self.file.metadata()?;
            anyhow::ensure!(
                mounted.dev() == admitted.dev() && mounted.ino() == admitted.ino(),
                "mounted toolstore differs from verified inode"
            );
            crate::inode_seal::require(target, "attached toolstore")
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (self, target);
            anyhow::bail!("toolstores require Linux")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Manifest;

    #[test]
    fn refuses_incompatible_manifest_before_opening_disk() {
        let mut manifest = Manifest {
            schema_version: 1,
            system: "aarch64-linux".into(),
            sha256: "a".repeat(64),
        };
        assert!(manifest.validate("aarch64-linux").is_ok());
        assert!(manifest.validate("x86_64-linux").is_err());
        manifest.schema_version = 2;
        assert!(manifest.validate("aarch64-linux").is_err());
        manifest.schema_version = 1;
        manifest.sha256 = "g".repeat(64);
        assert!(manifest.validate("aarch64-linux").is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn manifest_refuses_links_directories_and_fifos() -> anyhow::Result<()> {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir()?;
        let regular = dir.path().join("regular");
        std::fs::write(&regular, b"{}")?;
        assert!(super::manifest_file(&regular).is_ok());
        assert!(super::manifest_file(dir.path()).is_err());
        let link = dir.path().join("link");
        symlink(&regular, &link)?;
        assert!(super::manifest_file(&link).is_err());
        let fifo = dir.path().join("fifo");
        rustix::fs::mkfifoat(rustix::fs::CWD, &fifo, rustix::fs::Mode::RUSR)?;
        assert!(super::manifest_file(&fifo).is_err());
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires root with immutable-inode support; run explicitly on the Rooms host"]
    fn attachment_holds_admitted_inode_after_directory_replacement() -> anyhow::Result<()> {
        use anyhow::Context;
        use rustix::fs::{ioctl_getflags, ioctl_setflags, IFlags};
        use sha2::{Digest, Sha256};
        let temporary = tempfile::tempdir()?;
        let directory = temporary.path().join("tools");
        std::fs::create_dir(&directory)?;
        let path = directory.join("toolstore.sqfs");
        let bytes = b"hsqs-original-admitted-bytes";
        std::fs::write(&path, bytes)?;
        let file = std::fs::File::open(&path)?;
        let baseline = ioctl_getflags(&file)?;
        ioctl_setflags(&file, baseline | IFlags::IMMUTABLE)?;
        // Clear the test's seal even if an assertion returns an error.
        let result = (|| -> anyhow::Result<()> {
            let manifest = serde_json::json!({
                "schema_version": 1,
                "system": format!("{}-linux", std::env::consts::ARCH),
                "sha256": format!("{:x}", Sha256::digest(bytes)),
            });
            std::fs::write(directory.join("meta.json"), manifest.to_string())?;
            let admitted = super::Toolstore::open(&directory)?;
            std::fs::rename(&directory, temporary.path().join("moved"))?;
            std::fs::create_dir(&directory)?;
            std::fs::write(&path, b"hsqs-substituted-bytes")?;
            anyhow::ensure!(std::fs::read(admitted.mount_source()?)? == bytes);
            anyhow::ensure!(std::fs::read(&path)? != bytes);
            let error = admitted
                .verify_attachment(&path)
                .err()
                .context("substituted inode must be refused")?;
            anyhow::ensure!(error.to_string().contains("differs from verified inode"));
            let target = temporary.path().join("mount-target");
            std::fs::File::create_new(&target)?;
            let mounted = admitted.bind_into(&target).and_then(|()| {
                anyhow::ensure!(std::fs::read(&target)? == bytes);
                Ok(())
            });
            let unmounted = std::process::Command::new("umount").arg(&target).status()?;
            anyhow::ensure!(unmounted.success(), "test mount cleanup failed");
            mounted
        })();
        ioctl_setflags(&file, baseline)?;
        result
    }
}
