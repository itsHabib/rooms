//! Kernel-enforced immutability for snapshot inputs.
//!
//! Firecracker demand-pages a restored rootfs and memory file after the
//! `/snapshot/load` request returns. Path identity checks therefore cannot
//! make those bytes safe: the backing inode itself must reject writes for the
//! complete lifetime of every restored VM.

#![allow(
    clippy::redundant_pub_crate,
    reason = "the crate-private API is shared by sibling snapshot and restore modules"
)]

use std::path::Path;

#[cfg(target_os = "linux")]
use rustix::fs::{ioctl_getflags, ioctl_setflags, open, IFlags, Mode, OFlags};

#[cfg(any(target_os = "linux", test))]
trait InodeFlagOps {
    fn immutable_flag(&self) -> u32;
    fn get_flags(&mut self) -> anyhow::Result<u32>;
    fn set_flags(&mut self, flags: u32) -> anyhow::Result<()>;
    fn sync_all(&mut self) -> anyhow::Result<()>;
}

#[cfg(target_os = "linux")]
struct LinuxInodeFlagOps<'a> {
    file: &'a std::fs::File,
}

#[cfg(target_os = "linux")]
impl InodeFlagOps for LinuxInodeFlagOps<'_> {
    fn immutable_flag(&self) -> u32 {
        IFlags::IMMUTABLE.bits()
    }

    fn get_flags(&mut self) -> anyhow::Result<u32> {
        Ok(ioctl_getflags(self.file)?.bits())
    }

    fn set_flags(&mut self, flags: u32) -> anyhow::Result<()> {
        ioctl_setflags(self.file, IFlags::from_bits_retain(flags))?;
        Ok(())
    }

    fn sync_all(&mut self) -> anyhow::Result<()> {
        self.file.sync_all()?;
        Ok(())
    }
}

#[cfg(any(target_os = "linux", test))]
fn inspect_flags(ops: &mut impl InodeFlagOps, path: &Path, label: &str) -> anyhow::Result<u32> {
    ops.get_flags().map_err(|error| {
        anyhow::anyhow!(
            "inspect immutable flag for {label} {}: {error}",
            path.display()
        )
    })
}

#[cfg(any(target_os = "linux", test))]
fn require_with_ops(ops: &mut impl InodeFlagOps, path: &Path, label: &str) -> anyhow::Result<()> {
    let flags = inspect_flags(ops, path, label)?;
    if flags & ops.immutable_flag() == 0 {
        anyhow::bail!(
            "{label} {} is not kernel-immutable; snapshot inputs must be sealed against writes",
            path.display()
        );
    }
    Ok(())
}

/// Refuse a path whose inode is not kernel-immutable.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn require(path: &Path, label: &str) -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    {
        let fd = open(
            path,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
            Mode::empty(),
        )
        .map_err(|error| anyhow::anyhow!("open {label} {}: {error}", path.display()))?;
        let file = std::fs::File::from(fd);
        let metadata = file
            .metadata()
            .map_err(|error| anyhow::anyhow!("inspect {label} {}: {error}", path.display()))?;
        if !metadata.file_type().is_file() && !metadata.file_type().is_dir() {
            anyhow::bail!(
                "{label} {} is not a regular file or directory",
                path.display()
            );
        }
        require_with_ops(&mut LinuxInodeFlagOps { file: &file }, path, label)
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = (path, label);
        anyhow::bail!("snapshot inode sealing is supported only on Linux")
    }
}

/// Open one regular path without following its leaf symlink, require the
/// opened inode to be immutable, and return that same descriptor to the caller.
/// Hashing or parsing through this handle cannot be redirected by a later path
/// rename.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn open_regular(path: &Path, label: &str) -> anyhow::Result<std::fs::File> {
    #[cfg(target_os = "linux")]
    {
        let fd = open(
            path,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
            Mode::empty(),
        )
        .map_err(|error| anyhow::anyhow!("open {label} {}: {error}", path.display()))?;
        let file = std::fs::File::from(fd);
        let metadata = file
            .metadata()
            .map_err(|error| anyhow::anyhow!("inspect {label} {}: {error}", path.display()))?;
        if !metadata.file_type().is_file() || metadata.len() == 0 {
            anyhow::bail!("{label} {} is empty or not a regular file", path.display());
        }
        require_file(&file, path, label)?;
        Ok(file)
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = (path, label);
        anyhow::bail!("snapshot inode sealing is supported only on Linux")
    }
}

/// Re-check immutability on an already-open descriptor.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn require_file(file: &std::fs::File, path: &Path, label: &str) -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    {
        require_with_ops(&mut LinuxInodeFlagOps { file }, path, label)
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = (file, path, label);
        anyhow::bail!("snapshot inode sealing is supported only on Linux")
    }
}

#[cfg(any(target_os = "linux", test))]
fn seal_with_ops(ops: &mut impl InodeFlagOps, path: &Path, label: &str) -> anyhow::Result<()> {
    let flags = inspect_flags(ops, path, label)?;
    if flags & ops.immutable_flag() == 0 {
        ops.set_flags(flags | ops.immutable_flag())
            .map_err(|error| {
                anyhow::anyhow!("seal {label} {} immutable: {error}", path.display())
            })?;
    }
    ops.sync_all()
        .map_err(|error| anyhow::anyhow!("sync sealed {label} {}: {error}", path.display()))?;
    require_with_ops(ops, path, label)
}

/// Idempotently add the immutable inode flag and durably verify it.
pub(crate) fn seal(path: &Path, label: &str) -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    {
        let file = std::fs::File::open(path)
            .map_err(|error| anyhow::anyhow!("open {label} {}: {error}", path.display()))?;
        seal_with_ops(&mut LinuxInodeFlagOps { file: &file }, path, label)
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = (path, label);
        anyhow::bail!("snapshot inode sealing is supported only on Linux")
    }
}

#[cfg(any(target_os = "linux", test))]
fn restore_baseline(
    ops: &mut impl InodeFlagOps,
    baseline: u32,
    directory: &Path,
) -> anyhow::Result<()> {
    // Run every recovery step even if an earlier one fails. In particular,
    // an ioctl may have changed the inode before reporting an error, so a
    // failed clear must not prevent verification or a durability attempt.
    let mut failures = Vec::new();
    if let Err(error) = ops.set_flags(baseline) {
        failures.push(format!("set baseline flags: {error}"));
    }
    match ops.get_flags() {
        Ok(restored) if restored == baseline => {}
        Ok(restored) => failures.push(format!(
            "inode flags are {restored:#x}, expected baseline {baseline:#x}"
        )),
        Err(error) => failures.push(format!("verify baseline flags: {error}")),
    }
    if let Err(error) = ops.sync_all() {
        failures.push(format!("sync restored inode: {error}"));
    }
    if failures.is_empty() {
        return Ok(());
    }
    anyhow::bail!(
        "restore snapshot output mutability at {}: {}",
        directory.display(),
        failures.join("; ")
    )
}

#[cfg(any(target_os = "linux", test))]
fn with_preflight_cleanup(
    primary: anyhow::Error,
    ops: &mut impl InodeFlagOps,
    baseline: u32,
    directory: &Path,
) -> anyhow::Error {
    match restore_baseline(ops, baseline, directory) {
        Ok(()) => primary,
        Err(cleanup) => anyhow::anyhow!("{primary}; cleanup also failed: {cleanup}"),
    }
}

#[cfg(any(target_os = "linux", test))]
fn preflight_output_with_ops(ops: &mut impl InodeFlagOps, directory: &Path) -> anyhow::Result<()> {
    let observed = ops.get_flags().map_err(|error| {
        anyhow::anyhow!(
            "snapshot output filesystem cannot report immutable flags for {}: {error}",
            directory.display()
        )
    })?;
    let immutable = ops.immutable_flag();
    let baseline = observed & !immutable;

    if observed != baseline {
        restore_baseline(ops, baseline, directory).map_err(|error| {
            anyhow::anyhow!(
                "clear interrupted immutable preflight on {}: {error}",
                directory.display()
            )
        })?;
    }

    if let Err(error) = ops.set_flags(baseline | immutable) {
        let primary = anyhow::anyhow!(
            "snapshot output filesystem cannot seal immutable inodes at {}: {error}",
            directory.display()
        );
        return Err(with_preflight_cleanup(primary, ops, baseline, directory));
    }

    let sealed = match ops.get_flags() {
        Ok(flags) => flags,
        Err(error) => {
            let primary = anyhow::anyhow!(
                "verify immutable preflight on {}: {error}",
                directory.display()
            );
            return Err(with_preflight_cleanup(primary, ops, baseline, directory));
        }
    };
    if sealed & immutable == 0 {
        let primary = anyhow::anyhow!(
            "snapshot output filesystem ignored immutable sealing at {}",
            directory.display()
        );
        return Err(with_preflight_cleanup(primary, ops, baseline, directory));
    }

    restore_baseline(ops, baseline, directory)
}

/// Prove the output filesystem and current privilege can set and clear the
/// immutable flag before snapshot creation consumes a live base.
///
/// A `Pending` transaction owns an empty output directory. If a process died
/// after setting the probe flag, recovery re-enters here and safely clears it
/// before repeating the complete set/verify/clear cycle.
pub(crate) fn preflight_output(directory: &Path) -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    {
        let file = std::fs::File::open(directory).map_err(|error| {
            anyhow::anyhow!(
                "open snapshot output {} for immutable preflight: {error}",
                directory.display()
            )
        })?;
        preflight_output_with_ops(&mut LinuxInodeFlagOps { file: &file }, directory)
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = directory;
        anyhow::bail!("snapshot inode sealing is supported only on Linux")
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::panic,
        clippy::unwrap_used,
        reason = "scripted fake failures and assertions are intentional in tests"
    )]

    use super::*;
    use std::collections::VecDeque;

    const IMMUTABLE: u32 = 0x10;
    const BASELINE: u32 = 0x20;

    #[derive(Debug)]
    enum Step {
        Get(anyhow::Result<u32>),
        Set {
            expected: u32,
            result: anyhow::Result<()>,
        },
        Sync(anyhow::Result<()>),
    }

    #[derive(Debug)]
    struct FakeOps {
        steps: VecDeque<Step>,
    }

    impl FakeOps {
        fn new(steps: impl IntoIterator<Item = Step>) -> Self {
            Self {
                steps: steps.into_iter().collect(),
            }
        }

        fn done(self) {
            assert!(self.steps.is_empty(), "unused fake steps: {:?}", self.steps);
        }

        fn next(&mut self, operation: &str) -> Step {
            self.steps
                .pop_front()
                .unwrap_or_else(|| panic!("unexpected {operation}"))
        }
    }

    impl InodeFlagOps for FakeOps {
        fn immutable_flag(&self) -> u32 {
            IMMUTABLE
        }

        fn get_flags(&mut self) -> anyhow::Result<u32> {
            match self.next("get_flags") {
                Step::Get(result) => result,
                step => panic!("expected get_flags, got {step:?}"),
            }
        }

        fn set_flags(&mut self, flags: u32) -> anyhow::Result<()> {
            match self.next("set_flags") {
                Step::Set { expected, result } => {
                    assert_eq!(flags, expected);
                    result
                }
                step => panic!("expected set_flags, got {step:?}"),
            }
        }

        fn sync_all(&mut self) -> anyhow::Result<()> {
            match self.next("sync_all") {
                Step::Sync(result) => result,
                step => panic!("expected sync_all, got {step:?}"),
            }
        }
    }

    fn get(flags: u32) -> Step {
        Step::Get(Ok(flags))
    }

    fn set(flags: u32) -> Step {
        Step::Set {
            expected: flags,
            result: Ok(()),
        }
    }

    fn sync() -> Step {
        Step::Sync(Ok(()))
    }

    #[test]
    fn seal_is_idempotent_but_still_syncs_and_verifies() {
        let mut ops = FakeOps::new([get(BASELINE | IMMUTABLE), sync(), get(BASELINE | IMMUTABLE)]);

        seal_with_ops(&mut ops, Path::new("snapshot.mem"), "snapshot memory").unwrap();

        ops.done();
    }

    #[test]
    fn seal_reports_set_failure_without_claiming_durability() {
        let mut ops = FakeOps::new([
            get(BASELINE),
            Step::Set {
                expected: BASELINE | IMMUTABLE,
                result: Err(anyhow::anyhow!("set failed")),
            },
        ]);

        let error = seal_with_ops(&mut ops, Path::new("rootfs.ext4"), "rootfs").unwrap_err();

        assert!(error
            .to_string()
            .contains("seal rootfs rootfs.ext4 immutable"));
        ops.done();
    }

    #[test]
    fn seal_reports_sync_failure_before_verification() {
        let mut ops = FakeOps::new([
            get(BASELINE),
            set(BASELINE | IMMUTABLE),
            Step::Sync(Err(anyhow::anyhow!("sync failed"))),
        ]);

        let error = seal_with_ops(&mut ops, Path::new("snapshot.mem"), "memory").unwrap_err();

        assert!(error
            .to_string()
            .contains("sync sealed memory snapshot.mem"));
        ops.done();
    }

    #[test]
    fn seal_reports_post_sync_verification_failure() {
        let mut ops = FakeOps::new([
            get(BASELINE),
            set(BASELINE | IMMUTABLE),
            sync(),
            get(BASELINE),
        ]);

        let error = seal_with_ops(&mut ops, Path::new("snapshot.mem"), "memory").unwrap_err();

        assert!(error.to_string().contains("is not kernel-immutable"));
        ops.done();
    }

    #[test]
    fn preflight_recovers_interrupted_flag_before_full_probe() {
        let mut ops = FakeOps::new([
            get(BASELINE | IMMUTABLE),
            set(BASELINE),
            get(BASELINE),
            sync(),
            set(BASELINE | IMMUTABLE),
            get(BASELINE | IMMUTABLE),
            set(BASELINE),
            get(BASELINE),
            sync(),
        ]);

        preflight_output_with_ops(&mut ops, Path::new("pending-snapshot")).unwrap();

        ops.done();
    }

    #[test]
    fn preflight_cleans_up_when_probe_set_partially_fails() {
        let mut ops = FakeOps::new([
            get(BASELINE),
            Step::Set {
                expected: BASELINE | IMMUTABLE,
                result: Err(anyhow::anyhow!("ioctl failed after applying flag")),
            },
            set(BASELINE),
            get(BASELINE),
            sync(),
        ]);

        let error = preflight_output_with_ops(&mut ops, Path::new("pending-snapshot")).unwrap_err();

        assert!(error.to_string().contains("cannot seal immutable inodes"));
        ops.done();
    }

    #[test]
    fn preflight_cleans_up_when_probe_verification_fails() {
        let mut ops = FakeOps::new([
            get(BASELINE),
            set(BASELINE | IMMUTABLE),
            Step::Get(Err(anyhow::anyhow!("get failed"))),
            set(BASELINE),
            get(BASELINE),
            sync(),
        ]);

        let error = preflight_output_with_ops(&mut ops, Path::new("pending-snapshot")).unwrap_err();

        assert!(error.to_string().contains("verify immutable preflight"));
        ops.done();
    }

    #[test]
    fn preflight_reports_cleanup_sync_failure_after_restoring_flags() {
        let mut ops = FakeOps::new([
            get(BASELINE),
            set(BASELINE | IMMUTABLE),
            get(BASELINE | IMMUTABLE),
            set(BASELINE),
            get(BASELINE),
            Step::Sync(Err(anyhow::anyhow!("sync failed"))),
        ]);

        let error = preflight_output_with_ops(&mut ops, Path::new("pending-snapshot")).unwrap_err();

        assert!(error.to_string().contains("sync restored inode"));
        ops.done();
    }

    #[test]
    fn preflight_preserves_primary_and_cleanup_failures() {
        let mut ops = FakeOps::new([
            get(BASELINE),
            set(BASELINE | IMMUTABLE),
            get(BASELINE),
            Step::Set {
                expected: BASELINE,
                result: Err(anyhow::anyhow!("clear failed")),
            },
            get(BASELINE | IMMUTABLE),
            sync(),
        ]);

        let error = preflight_output_with_ops(&mut ops, Path::new("pending-snapshot")).unwrap_err();
        let message = error.to_string();

        assert!(message.contains("ignored immutable sealing"));
        assert!(message.contains("cleanup also failed"));
        assert!(message.contains("clear failed"));
        assert!(message.contains("expected baseline"));
        ops.done();
    }
}
