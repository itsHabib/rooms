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

/// Validate that `path` is a bootable Firecracker guest kernel for this host.
///
/// An uncompressed ELF vmlinux on `x86_64`, or a Linux ARM64 boot `Image`
/// (magic `ARM\x64` at byte offset 56) on `aarch64`.
/// The guest kernel boots under the host's own Firecracker, so its format must
/// match the host arch: the check is gated on the build's `target_arch` (which
/// equals the host arch), the same way the shell-side `is_guest_kernel` gates
/// on `uname -m`. Accepting the wrong-arch format here would defer a guaranteed
/// boot failure past validation — worse than rejecting it up front on this
/// isolation-boundary path.
pub fn validate_kernel(path: &Path) -> Result<(), RootfsError> {
    if !path.exists() {
        return Err(RootfsError::KernelNotFound {
            path: path.to_path_buf(),
        });
    }

    let mut header = [0_u8; 60];
    let mut file = std::fs::File::open(path)?;
    match file.read_exact(&mut header) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
            return Err(RootfsError::KernelBadFormat {
                path: path.to_path_buf(),
            });
        }
        Err(e) => return Err(e.into()),
    }

    if kernel_matches_host_arch(&header) {
        return Ok(());
    }
    Err(RootfsError::KernelBadFormat {
        path: path.to_path_buf(),
    })
}

/// Whether the 60-byte header is the bootable kernel format for this build's
/// target architecture. `x86_64` requires an ELF vmlinux; `aarch64` requires
/// an ARM64 boot `Image` (magic at offset 56). On any other target arch rooms
/// does not run Firecracker, so accept either rather than hard-fail a format
/// check that has no host to boot against.
fn kernel_matches_host_arch(header: &[u8; 60]) -> bool {
    let elf = header[..4] == [0x7F, b'E', b'L', b'F'];
    let arm64_image = header[56..60] == *b"ARM\x64";
    #[cfg(target_arch = "x86_64")]
    {
        let _ = arm64_image;
        elf
    }
    #[cfg(target_arch = "aarch64")]
    {
        let _ = elf;
        arm64_image
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        elf || arm64_image
    }
}

/// Fail-closed admission for a snapshot-capable base image.
///
/// The immutable lower layer must contain the overlay entry point and must not
/// contain a baked SSH host private key. The latter cannot be repaired by
/// deleting a key after boot: its bytes may already have reached guest memory.
pub fn validate_snapshot_base_image(path: &Path) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    crate::inode_seal::require(path, "snapshot base image").map_err(|error| error.to_string())?;

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
        .map_err(|e| debugfs_spawn_error(&e))?;
    if !output.status.success() {
        return Err(format!(
            "inspect snapshot base image {} ({request}): {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn debugfs_spawn_error(error: &std::io::Error) -> String {
    if error.kind() == std::io::ErrorKind::NotFound {
        return "snapshot base admission requires debugfs; install e2fsprogs (for example: apt install e2fsprogs)".to_owned();
    }
    format!("inspect snapshot base image with debugfs: {error}")
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
    #![allow(
        clippy::expect_used,
        clippy::indexing_slicing,
        reason = "test helper: a missing shell is an immediate test failure"
    )]

    #[cfg(target_os = "linux")]
    use super::validate_snapshot_base_image;
    use super::{baked_host_private_key, debugfs_spawn_error, validate_kernel};
    use crate::error::RootfsError;

    fn write_kernel(bytes: &[u8]) -> tempfile::NamedTempFile {
        use std::io::Write as _;
        let mut f = tempfile::NamedTempFile::new().expect("tempfile");
        f.write_all(bytes).expect("write");
        f.flush().expect("flush");
        f
    }

    /// An ELF vmlinux header (magic at offset 0).
    fn elf_kernel() -> Vec<u8> {
        let mut elf = vec![0_u8; 64];
        elf[..4].copy_from_slice(&[0x7F, b'E', b'L', b'F']);
        elf
    }

    /// An ARM64 Linux boot `Image` header (`ARMd` magic at offset 56).
    fn arm64_kernel() -> Vec<u8> {
        let mut image = vec![0_u8; 64];
        image[56..60].copy_from_slice(b"ARM\x64");
        image
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn validate_kernel_accepts_elf_and_rejects_arm64_on_x86() {
        let f = write_kernel(&elf_kernel());
        assert!(
            validate_kernel(f.path()).is_ok(),
            "ELF vmlinux must pass on x86_64"
        );
        // The wrong-arch kernel must be rejected at validation, not deferred to
        // a boot failure.
        let f = write_kernel(&arm64_kernel());
        assert!(
            matches!(
                validate_kernel(f.path()),
                Err(RootfsError::KernelBadFormat { .. })
            ),
            "an ARM64 Image must be rejected on an x86_64 host"
        );
    }

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn validate_kernel_accepts_arm64_and_rejects_elf_on_aarch64() {
        let f = write_kernel(&arm64_kernel());
        assert!(
            validate_kernel(f.path()).is_ok(),
            "ARM64 Image must pass on aarch64"
        );
        let f = write_kernel(&elf_kernel());
        assert!(
            matches!(
                validate_kernel(f.path()),
                Err(RootfsError::KernelBadFormat { .. })
            ),
            "an ELF vmlinux must be rejected on an aarch64 host"
        );
    }

    #[test]
    fn validate_kernel_rejects_neither_magic_and_short_headers() {
        // Neither magic → KernelBadFormat (on every arch).
        let f = write_kernel(&[0_u8; 64]);
        assert!(
            matches!(
                validate_kernel(f.path()),
                Err(RootfsError::KernelBadFormat { .. })
            ),
            "a buffer that is neither ELF nor ARM64 Image must be rejected"
        );
        // Too short to carry the offset-56 magic → KernelBadFormat, not a panic.
        let f = write_kernel(&[0x7F, b'E']);
        assert!(
            matches!(
                validate_kernel(f.path()),
                Err(RootfsError::KernelBadFormat { .. })
            ),
            "a sub-header file must be rejected as bad format"
        );
    }

    #[test]
    fn validate_kernel_missing_file_is_not_found() {
        let missing = std::path::Path::new("/nonexistent/rooms/vmlinux.bin");
        assert!(matches!(
            validate_kernel(missing),
            Err(RootfsError::KernelNotFound { .. })
        ));
    }

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
    fn missing_debugfs_has_actionable_remediation() {
        let error = std::io::Error::from(std::io::ErrorKind::NotFound);
        let message = debugfs_spawn_error(&error);
        assert!(message.contains("requires debugfs"), "{message}");
        assert!(message.contains("install e2fsprogs"), "{message}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn snapshot_base_admission_refuses_a_mutable_inode_before_inspection() {
        let image = tempfile::NamedTempFile::new().expect("mutable image tempfile");
        let error =
            validate_snapshot_base_image(image.path()).expect_err("mutable inode must fail");
        assert!(error.contains("snapshot base image"), "{error}");
        assert!(error.contains("is not kernel-immutable"), "{error}");
    }

    #[test]
    fn snapshot_rootfs_scripts_encode_the_neutral_base_shape() {
        let build = include_str!("../scripts/build-rootfs-alpine.sh");
        assert!(build.contains("rm -f /etc/ssh/ssh_host_*"));
        assert!(!build.contains("\nssh-keygen -A\n"));
        assert!(build.contains("rc-update add rooms-provision boot"));
        assert!(build.contains("chattr +i -- \"$OUT\""));
        assert!(build.contains("lsattr -d -- \"$OUT\""));

        let overlay = include_str!("../scripts/lib/overlay-init.sh");
        assert!(overlay.contains("rooms.base=1"));
        assert!(overlay.contains("runlevels/default/sshd"));
        assert!(overlay.contains("runlevels/boot/rooms-secrets"));

        let agent = include_str!("../scripts/lib/rooms-provision-agent.sh");
        assert!(agent.contains("env -i HOME=/home/rooms"));
        assert!(agent.contains("dd bs=1 count=\"$length\""));
        assert!(!agent.contains("head -c \"$length\""));
        assert!(agent.contains("ipv6_is_disabled"));
        assert!(agent.contains("no_post_warm_processes"));
        assert!(agent.contains("VSOCK-CONNECT:2:5002"));
        assert!(agent.contains("install -d -m 0711 -o root -g root \"$PROVISION_DIR\""));
        assert!(!agent.contains("install -d -m 0700 \"$PROVISION_DIR\""));
        assert!(agent.contains("revoke_workload_sudo"));
        assert!(agent.contains("rooms_sudo_is_revoked"));
        assert!(agent.contains("/usr/bin/sudo -U rooms -ll"));
        for executable in [
            "/usr/bin/ssh-keygen",
            "/usr/bin/sudo",
            "/usr/sbin/sshd",
            "/usr/sbin/visudo",
        ] {
            assert!(agent.contains(executable), "unprotected: {executable}");
        }
        assert!(agent.contains("verify_protected_state"));
        assert_eq!(agent.matches("credential_state_is_safe || fail").count(), 2);

        let resume = include_str!("../scripts/lib/rooms-resume-agent.sh");
        assert!(resume.contains("exec /sbin/rooms-resume-apply --loop"));
        assert!(!resume.contains("socat"));
        assert!(!resume.contains("EXEC:"));

        let apply = include_str!("../scripts/lib/rooms-resume-apply.c");
        assert!(apply.contains("#define ENTROPY_LEN 64U"));
        assert!(apply.contains("#define MAX_SECRETS_LEN"));
        assert!(apply.contains("clock_settime(CLOCK_REALTIME"));
        assert!(apply.contains("open(\"/dev/urandom\", O_WRONLY"));
        assert!(apply.contains("ioctl(fd, RNDRESEEDCRNG, 0)"));
        assert!(apply.contains("socket(AF_VSOCK"));
        assert!(apply.contains("SOCK_NONBLOCK"));
        assert!(apply.contains("connect_resume_stream"));
        assert!(apply.contains("MSG_NOSIGNAL"));
        assert!(apply.contains("RESUME_HANDSHAKE_MAX_ATTEMPTS"));
        let preface_write = apply
            .find("send_resume_preface(fd)")
            .expect("preface write before transport attachment");
        let transport_attach = apply
            .find("dup2(fd, STDIN_FILENO)")
            .expect("resume transport attachment");
        assert!(preface_write < transport_attach);
        assert_native_resume_hygiene_order(apply);
        assert!(apply.contains("ROOMS-RESUME/2"));
        assert!(apply.contains("\"-t\", \"ed25519\""));
        assert!(!apply.contains("ssh-keygen -A"));
        assert!(!apply.to_ascii_lowercase().contains("ssh_host_rsa"));
        assert!(!apply.to_ascii_lowercase().contains("ssh_host_ecdsa"));
        assert!(apply.contains("effective_sshd_config_is_safe"));
        assert!(apply.contains("\"/usr/sbin/visudo\", \"-cf\""));
        assert!(apply.contains("tcp22_is_listening"));
        assert!(apply.contains("sshd_owns_reachable_listener"));
        assert!(apply.contains("/run/rooms/secrets.env"));
        assert!(!apply.contains("/run/rooms-secrets/env"));
        assert!(!apply.contains("/usr/bin/git"));
        assert!(!apply.contains("/bin/sh"));
        assert!(apply.contains("\"/usr/bin/sudo\", \"-n\", \"/bin/true\""));

        assert!(build.contains("-static-pie"));
        assert!(build.contains("-fno-stack-protector"));
        assert!(!build.contains("-fstack-protector-strong"));
        assert!(build.contains(".rooms-resume-build-deps"));
        assert!(build.contains("gcc musl-dev linux-headers"));
        assert!(build.contains("apk del .rooms-resume-build-deps"));
        assert!(build.contains("native post-resume helper rejected the final built image policy"));
        assert_eq!(
            build
                .matches("HostKey /etc/ssh/ssh_host_ed25519_key")
                .count(),
            1
        );
        assert!(!build.contains("HostCertificate"));
        assert!(!build.contains("HostKeyAgent"));
        assert!(build.contains("command_args=\"loop\""));
    }

    // The provisioning agent runs on the Linux rooms-host and inside the Alpine
    // guest, so these tests exercise it against the host's own /bin/sh and
    // coreutils. That is only a faithful stand-in on Linux because the script
    // reads procfs. Its frame reader deliberately uses byte-at-a-time `dd` so a
    // pipe or socket read cannot swallow bytes from the adjacent frame.
    #[cfg(target_os = "linux")]
    fn run_agent_shell(body: &str) -> std::process::Output {
        let agent = format!(
            "{}/scripts/lib/rooms-provision-agent.sh",
            env!("CARGO_MANIFEST_DIR")
        );
        std::process::Command::new("/bin/sh")
            .args(["-c", body])
            .env("AGENT", agent)
            .output()
            .expect("run provisioning-agent shell test")
    }

    #[cfg(unix)]
    fn compile_resume_helper() -> (tempfile::TempDir, std::path::PathBuf) {
        let directory = tempfile::tempdir().expect("resume-helper build directory");
        let binary = directory.path().join("rooms-resume-apply-test");
        let source = format!(
            "{}/scripts/lib/rooms-resume-apply.c",
            env!("CARGO_MANIFEST_DIR")
        );
        let output = std::process::Command::new("cc")
            .args([
                "-std=c11",
                "-Wall",
                "-Wextra",
                "-Werror",
                "-fno-stack-protector",
                "-DROOMS_RESUME_TEST",
                "-o",
            ])
            .arg(&binary)
            .arg(source)
            .output()
            .expect("compile native resume helper");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        (directory, binary)
    }

    fn canonical_resume_sshd_config() -> String {
        let build = include_str!("../scripts/build-rootfs-alpine.sh");
        let marker = "cat >\"$MNT/etc/ssh/sshd_config.rooms-resume\" <<'EOF'\n";
        let (_, after_marker) = build
            .split_once(marker)
            .expect("canonical resume sshd heredoc");
        let (config, _) = after_marker
            .split_once("\nEOF\n")
            .expect("canonical resume sshd heredoc terminator");
        format!("{config}\n")
    }

    fn assert_native_resume_hygiene_order(apply: &str) {
        let resume_session = apply
            .split_once("static int resume_session")
            .expect("native resume transaction")
            .1
            .split_once("static int prewarm")
            .expect("native resume transaction boundary")
            .0;
        let entropy_parse = resume_session
            .find("parse_entropy_frame")
            .expect("entropy-first parse");
        let forced_reseed = resume_session
            .find("reseed(sensitive_payload.entropy)")
            .expect("forced CRNG reseed");
        let metadata_parse = resume_session
            .find("parse_payload(&sensitive_payload)")
            .expect("post-reseed metadata parse");
        assert!(entropy_parse < forced_reseed && forced_reseed < metadata_parse);

        let apply_payload = apply
            .split_once("static int apply_payload")
            .expect("native hygiene application")
            .1
            .split_once("static int resume_session")
            .expect("native hygiene application boundary")
            .0;
        let positions = [
            "stage_identity",
            "generate_host_key",
            "restore_sudo",
            "READY clock",
            "parse_clock",
            "step_clock",
            "APPLIED clock",
            "parse_clock_continue",
            "launch_sshd",
        ]
        .map(|needle| apply_payload.find(needle).expect(needle));
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[cfg(unix)]
    fn git_in_repo(
        repo: &std::path::Path,
        home: &std::path::Path,
        arguments: &[&str],
    ) -> std::process::Output {
        std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(arguments)
            .env("HOME", home)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("GIT_AUTHOR_NAME")
            .env_remove("GIT_AUTHOR_EMAIL")
            .env_remove("GIT_COMMITTER_NAME")
            .env_remove("GIT_COMMITTER_EMAIL")
            .output()
            .expect("run git in staged repository")
    }

    #[cfg(unix)]
    fn prepare_hostile_worktree_config(root: &std::path::Path) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt as _;

        for directory in ["run", "home/rooms", "workspace"] {
            let path = root.join(directory);
            std::fs::create_dir_all(&path).expect("create staging directory");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .expect("protect staging directory");
        }
        let repo = root.join("workspace/repo");
        assert!(std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(&repo)
            .status()
            .expect("initialize repository")
            .success());
        for directory in [&repo, &repo.join(".git")] {
            std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o755))
                .expect("protect repository directory");
        }
        let home = root.join("home/rooms");
        for arguments in [
            &["config", "extensions.worktreeConfig", "true"][..],
            &["config", "rooms.keep", "unchanged"][..],
            &["config", "--worktree", "user.name", "warm hostile"][..],
            &["config", "--worktree", "user.email", "warm@evil.invalid"][..],
        ] {
            assert!(git_in_repo(&repo, &home, arguments).status.success());
        }
        for config in [repo.join(".git/config"), repo.join(".git/config.worktree")] {
            std::fs::set_permissions(config, std::fs::Permissions::from_mode(0o600))
                .expect("protect repository config");
        }
        repo
    }

    #[cfg(unix)]
    fn assert_staged_identity_is_effective(root: &std::path::Path, room_id: &str) {
        use std::os::unix::fs::PermissionsExt as _;

        let repo = root.join("workspace/repo");
        let home = root.join("home/rooms");
        let expected_name = format!("rooms {room_id}");
        let expected_email = format!("{room_id}@rooms.invalid");
        let name = git_in_repo(&repo, &home, &["config", "--get", "user.name"]);
        assert_eq!(String::from_utf8_lossy(&name.stdout).trim(), expected_name);
        let email = git_in_repo(&repo, &home, &["config", "--get", "user.email"]);
        assert_eq!(
            String::from_utf8_lossy(&email.stdout).trim(),
            expected_email
        );
        assert_eq!(
            git_in_repo(&repo, &home, &["config", "--get", "rooms.keep"]).stdout,
            b"unchanged\n"
        );

        let commit = git_in_repo(
            &repo,
            &home,
            &[
                "commit",
                "--allow-empty",
                "--no-gpg-sign",
                "-m",
                "identity proof",
            ],
        );
        assert!(
            commit.status.success(),
            "{}",
            String::from_utf8_lossy(&commit.stderr)
        );
        let ident = git_in_repo(
            &repo,
            &home,
            &["show", "-s", "--format=%an <%ae>|%cn <%ce>", "HEAD"],
        );
        assert_eq!(
            String::from_utf8_lossy(&ident.stdout).trim(),
            format!("{expected_name} <{expected_email}>|{expected_name} <{expected_email}>")
        );
        assert_eq!(
            std::fs::read_to_string(repo.join(".git/rooms-identity"))
                .expect("read repository identity include"),
            format!("[user]\n\tname = {expected_name}\n\temail = {expected_email}\n")
        );
        assert_eq!(
            std::fs::read_to_string(root.join("run/rooms/identity")).expect("read room identity"),
            format!("{room_id}\n")
        );
        assert_eq!(
            std::fs::read_to_string(root.join("run/rooms/secrets.env"))
                .expect("read staged secrets"),
            "TOKEN=test\n"
        );
        for (path, mode) in [("run/rooms/secrets.env", 0o600), ("run/rooms", 0o1770)] {
            assert_eq!(
                std::fs::metadata(root.join(path))
                    .expect("staged path metadata")
                    .permissions()
                    .mode()
                    & 0o7777,
                mode
            );
        }
    }

    #[cfg(unix)]
    fn assert_native_stage_rejects_config_symlink(binary: &std::path::Path, room_id: &str) {
        use std::os::unix::fs::{symlink, PermissionsExt as _};

        let hostile = tempfile::tempdir().expect("hostile staging root");
        for directory in ["run", "home/rooms", "workspace/repo/.git"] {
            let path = hostile.path().join(directory);
            std::fs::create_dir_all(&path).expect("create hostile directory");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .expect("protect hostile directory");
        }
        let hostile_config = hostile.path().join("workspace/repo/.git/config");
        let sentinel = hostile.path().join("sentinel");
        std::fs::write(&sentinel, "unchanged\n").expect("write sentinel");
        symlink(&sentinel, &hostile_config).expect("install hostile config symlink");
        let output = std::process::Command::new(binary)
            .args([
                "--test-stage",
                hostile.path().to_str().expect("hostile path"),
                room_id,
            ])
            .output()
            .expect("reject hostile native resume path");
        assert!(!output.status.success());
        assert_eq!(
            std::fs::read_to_string(sentinel).expect("read unchanged sentinel"),
            "unchanged\n"
        );
    }

    #[cfg(unix)]
    fn run_resume_parser(input: &[u8]) -> std::process::Output {
        use std::io::Write as _;
        use std::process::Stdio;

        let (_directory, binary) = compile_resume_helper();
        let mut child = std::process::Command::new(binary)
            .arg("--test-parse")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start native resume parser");
        child
            .stdin
            .take()
            .expect("parser stdin")
            .write_all(input)
            .expect("write parser input");
        child.wait_with_output().expect("wait for resume parser")
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn frame_reader_is_exact_and_preserves_the_next_header() {
        let output = run_agent_shell(
            r#"
            ROOMS_AGENT_LIBRARY_ONLY=1; export ROOMS_AGENT_LIBRARY_ONLY
            . "$AGENT"
            tmp="$(mktemp)"
            printf 'BUNDLE 6\nabc123WARM 5\ntrue\nEND 0\n' |
                {
                    read_frame BUNDLE "$tmp"
                    [ "$(cat "$tmp")" = abc123 ]
                    read_frame WARM "$tmp"
                    [ "$(cat "$tmp")" = true ]
                    IFS=' ' read -r kind length
                    [ "$kind" = END ] && [ "$length" = 0 ]
                }
            status=$?
            rm -f "$tmp"
            exit "$status"
            "#,
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn provision_directory_requires_owner_only_write_and_world_traversal() {
        let output = run_agent_shell(
            r#"
            ROOMS_AGENT_LIBRARY_ONLY=1; export ROOMS_AGENT_LIBRARY_ONLY
            . "$AGENT"
            tmp="$(mktemp -d)"
            trap 'rm -rf "$tmp"' EXIT
            directory="$tmp/provision"
            mkdir "$directory"
            uid="$(id -u)"
            gid="$(id -g)"

            chmod 0711 "$directory"
            provision_dir_is_safe "$directory" "$uid" "$gid"
            chmod 0700 "$directory"
            ! provision_dir_is_safe "$directory" "$uid" "$gid"
            chmod 0711 "$directory"
            mv "$directory" "$tmp/real"
            ln -s "$tmp/real" "$directory"
            ! provision_dir_is_safe "$directory" "$uid" "$gid"

            sudoers="$tmp/sudoers"
            printf '%s\n' "$SUDOERS_GRANT" >"$sudoers"
            chmod 0440 "$sudoers"
            sudoers_grant_is_exact "$sudoers" "$uid" "$gid"
            chmod 0400 "$sudoers"
            ! sudoers_grant_is_exact "$sudoers" "$uid" "$gid"
            "#,
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn neutral_sudo_policy_rejects_every_matching_rooms_rule() {
        let output = run_agent_shell(
            r#"
            ROOMS_AGENT_LIBRARY_ONLY=1; export ROOMS_AGENT_LIBRARY_ONLY
            . "$AGENT"
            sudo_policy_lists_no_rooms_rules \
                'User rooms is not allowed to run sudo on rooms-agent.'
            ! sudo_policy_lists_no_rooms_rules \
                'User rooms may run the following commands on rooms-agent:'
            ! sudo_policy_lists_no_rooms_rules 'Sudoers entry: /etc/sudoers.d/rooms'
            ! sudo_policy_lists_no_rooms_rules "$(printf '%s\n%s' \
                'User rooms is not allowed to run sudo on rooms-agent.' \
                'Sudoers entry: /etc/sudoers.d/rooms')"
            "#,
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn root_owned_0711_payload_is_readable_only_by_its_named_unprivileged_owner() {
        let output = run_agent_shell(
            r#"
            ROOMS_AGENT_LIBRARY_ONLY=1; export ROOMS_AGENT_LIBRARY_ONLY
            . "$AGENT"
            [ "$(id -u)" -eq 0 ] || exit 0
            id nobody >/dev/null 2>&1 || exit 0

            tmp="$(mktemp -d)"
            trap 'rm -rf "$tmp"' EXIT
            chmod 0755 "$tmp"
            directory="$tmp/provision"
            install -d -m 0711 -o root -g root "$directory"
            printf payload >"$directory/repo.bundle"
            nobody_uid="$(id -u nobody)"
            nobody_gid="$(id -g nobody)"
            chown "$nobody_uid:$nobody_gid" "$directory/repo.bundle"
            chmod 0600 "$directory/repo.bundle"

            su nobody -s /bin/sh -c "[ \"\$(cat '$directory/repo.bundle')\" = payload ]"
            chmod 0700 "$directory"
            ! su nobody -s /bin/sh -c "cat '$directory/repo.bundle' >/dev/null" 2>/dev/null
            "#,
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn neutral_credential_seal_rejects_files_directives_and_authenticated_urls() {
        #[cfg(not(target_os = "linux"))]
        return;
        #[cfg(target_os = "linux")]
        let output = run_agent_shell(
            r#"
            ROOMS_AGENT_LIBRARY_ONLY=1; export ROOMS_AGENT_LIBRARY_ONLY
            . "$AGENT"

            tmp="$(mktemp -d)"
            trap 'rm -rf "$tmp"' EXIT
            ROOMS_HOME="$tmp/home"
            ROOMS_REPO="$tmp/repo"
            SYSTEM_GIT_CONFIG="$tmp/system.gitconfig"
            mkdir -p "$ROOMS_HOME/.ssh" "$ROOMS_HOME/.config/git" "$ROOMS_HOME/.config/gh"
            : >"$ROOMS_HOME/.ssh/authorized_keys"
            git init -q "$ROOMS_REPO"
            git config --file "$ROOMS_HOME/.gitconfig" user.name safe-warm
            git config --file "$ROOMS_HOME/.gitconfig" rooms.keep unchanged
            git config --file "$ROOMS_HOME/.gitconfig" \
                url.https://github.com/.insteadOf gh:
            credential_state_is_safe

            : >"$ROOMS_HOME/.git-credentials"
            ! credential_state_is_safe >/dev/null 2>&1
            rm -f "$ROOMS_HOME/.git-credentials"

            : >"$ROOMS_HOME/.config/git/credentials"
            ! credential_state_is_safe >/dev/null 2>&1
            rm -f "$ROOMS_HOME/.config/git/credentials"

            printf private >"$ROOMS_HOME/.ssh/id_ed25519"
            ! credential_state_is_safe >/dev/null 2>&1
            rm -f "$ROOMS_HOME/.ssh/id_ed25519"

            git config --file "$ROOMS_HOME/.gitconfig" credential.helper store
            ! credential_state_is_safe >/dev/null 2>&1
            git config --file "$ROOMS_HOME/.gitconfig" --unset-all credential.helper

            git config --file "$ROOMS_HOME/.gitconfig" \
                http.https://example.com.extraHeader 'Authorization: bearer x'
            ! credential_state_is_safe >/dev/null 2>&1
            git config --file "$ROOMS_HOME/.gitconfig" \
                --unset-all http.https://example.com.extraHeader

            git config --file "$ROOMS_HOME/.gitconfig" include.path "$tmp/included"
            ! credential_state_is_safe >/dev/null 2>&1
            git config --file "$ROOMS_HOME/.gitconfig" --unset-all include.path

            git config --file "$ROOMS_HOME/.gitconfig" \
                url.https://user:password@example.com/.insteadOf private:
            ! credential_state_is_safe >/dev/null 2>&1
            git config --file "$ROOMS_HOME/.gitconfig" \
                --unset-all url.https://user:password@example.com/.insteadOf

            git -C "$ROOMS_REPO" config credential.helper '!echo leaked'
            ! credential_state_is_safe >/dev/null 2>&1
            git -C "$ROOMS_REPO" config --unset-all credential.helper

            credential_state_is_safe
            [ "$(git config --file "$ROOMS_HOME/.gitconfig" rooms.keep)" = unchanged ]
            "#,
        );
        #[cfg(target_os = "linux")]
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[cfg(unix)]
    #[test]
    fn native_resume_parser_accepts_exact_frames_without_consuming_the_next_header() {
        const CHALLENGE: &str = "01cccccccccccccccccccccccc";
        const CONTINUE_CHALLENGE: &str = "01eeeeeeeeeeeeeeeeeeeeeeee";
        let mut input = b"ENTROPY 64\n".to_vec();
        input
            .extend_from_slice(b"0123456789012345678901234567890123456789012345678901234567890123");
        input.extend_from_slice(format!(
            "IDENTITY 01aaaaaaaaaaaaaaaaaaaaaaaa\nSECRETS 11\nTOKEN=test\nEND 0\nCLOCK 1700000000 {CHALLENGE}\nCONTINUE clock {CONTINUE_CHALLENGE}\n"
        ).as_bytes());
        let output = run_resume_parser(&input);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.starts_with("ROOMS-RESUME/2\nREADY clock\n"),
            "{stdout}"
        );
        assert!(
            stdout.contains(&format!("APPLIED clock {CHALLENGE}\n")),
            "{stdout}"
        );
        assert!(
            stdout.contains(&format!("STEP clock {CONTINUE_CHALLENGE}\n")),
            "{stdout}"
        );
        assert!(
            stdout.contains(&format!(
                "PARSED 01aaaaaaaaaaaaaaaaaaaaaaaa 1700000000 11 {CHALLENGE}"
            )),
            "{stdout}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn native_resume_parser_rejects_unbounded_or_malformed_control_data() {
        let cases: &[&[u8]] = &[
            b"ENTROPY 65\n",
            b"ENTROPY 64\nshort",
            b"ENTROPY 64\n0123456789012345678901234567890123456789012345678901234567890123IDENTITY 01abc;touchxxxxxxxxxxxxxxxx\nSECRETS 0\nEND 0\n",
            b"ENTROPY 64\n0123456789012345678901234567890123456789012345678901234567890123IDENTITY 01aaaaaaaaaaaaaaaaaaaaaaaa extra\n",
            b"ENTROPY 64\n0123456789012345678901234567890123456789012345678901234567890123IDENTITY 01aaaaaaaaaaaaaaaaaaaaaaaa\nCLOCK 1700000000\n",
            b"ENTROPY 64\n0123456789012345678901234567890123456789012345678901234567890123IDENTITY 01aaaaaaaaaaaaaaaaaaaaaaaa\nSECRETS 0\nEND 0\nCLOCK -1 01cccccccccccccccccccccccc\n",
            b"ENTROPY 64\n0123456789012345678901234567890123456789012345678901234567890123IDENTITY 01aaaaaaaaaaaaaaaaaaaaaaaa\nSECRETS 1048577\n",
            b"ENTROPY 64\n0123456789012345678901234567890123456789012345678901234567890123IDENTITY 01aaaaaaaaaaaaaaaaaaaaaaaa\nSECRETS 0\nEND 0\nCLOCK 9223372036854775808 01cccccccccccccccccccccccc\n",
            b"ENTROPY 64\n0123456789012345678901234567890123456789012345678901234567890123IDENTITY 01aaaaaaaaaaaaaaaaaaaaaaaa\nSECRETS 0\nEND 0\nCLOCK 1700000000 short\n",
            b"ENTROPY 64\n0123456789012345678901234567890123456789012345678901234567890123IDENTITY 01aaaaaaaaaaaaaaaaaaaaaaaa\nSECRETS 0\nEND 0\nCLOCK 1700000000 01cccccccccccccccccccccccc extra\n",
            b"ENTROPY 64\n0123456789012345678901234567890123456789012345678901234567890123IDENTITY 01aaaaaaaaaaaaaaaaaaaaaaaa\nSECRETS 0\nEND 0\nCLOCK 1700000000 01cccccccccccccccccccccccc\nCONTINUE clock short\n",
        ];
        for input in cases {
            let output = run_resume_parser(input);
            assert!(
                !output.status.success(),
                "malformed input unexpectedly passed: {:?}",
                String::from_utf8_lossy(input)
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn native_resume_clock_application_does_not_release_ingress_without_continuation() {
        const CHALLENGE: &str = "01cccccccccccccccccccccccc";
        let mut input = b"ENTROPY 64\n".to_vec();
        input
            .extend_from_slice(b"0123456789012345678901234567890123456789012345678901234567890123");
        input.extend_from_slice(
            format!(
                "IDENTITY 01aaaaaaaaaaaaaaaaaaaaaaaa\nSECRETS 0\nEND 0\nCLOCK 1700000000 {CHALLENGE}\n"
            )
            .as_bytes(),
        );
        let output = run_resume_parser(&input);
        assert!(!output.status.success());
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains(&format!("APPLIED clock {CHALLENGE}\n")),
            "{stdout}"
        );
        assert!(!stdout.contains("STEP clock "), "{stdout}");
        assert!(!stdout.contains("PARSED "), "{stdout}");
    }

    #[cfg(unix)]
    #[test]
    fn native_resume_failure_emits_one_bounded_protocol_error() {
        let (_build, binary) = compile_resume_helper();
        let output = std::process::Command::new(binary)
            .arg("--test-error")
            .output()
            .expect("emit native resume protocol error");
        assert!(!output.status.success());

        let stdout = String::from_utf8(output.stdout).expect("protocol error is utf-8");
        assert!(stdout.ends_with('\n'), "{stdout:?}");
        assert_eq!(stdout.lines().count(), 1, "{stdout:?}");
        let line = stdout.trim_end_matches('\n');
        assert!(line.starts_with("ERR hostkeys "), "{line:?}");
        assert!(line.len() <= 64, "protocol error is {} bytes", line.len());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn native_resume_preface_handles_a_closed_candidate_without_sigpipe() {
        let (_build, binary) = compile_resume_helper();
        let output = std::process::Command::new(binary)
            .arg("--test-preface-closed-peer")
            .output()
            .expect("write preface to closed candidate");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[cfg(unix)]
    #[test]
    fn native_sshd_pid_poll_is_quiet_until_terminal_failure() {
        let (_build, binary) = compile_resume_helper();
        let root = tempfile::tempdir().expect("missing sshd pid root");
        let root = root.path().to_str().expect("utf-8 temp root");

        let probe = std::process::Command::new(&binary)
            .args(["--test-sshd-pid-absent", root])
            .output()
            .expect("probe absent sshd pid");
        assert!(probe.status.success());
        assert!(probe.stdout.is_empty(), "{:?}", probe.stdout);

        let terminal = std::process::Command::new(binary)
            .args(["--test-sshd-terminal", root])
            .output()
            .expect("emit terminal sshd launch failure");
        assert!(!terminal.status.success());
        let stdout = String::from_utf8(terminal.stdout).expect("terminal error is utf-8");
        assert_eq!(stdout.lines().count(), 1, "{stdout:?}");
        assert!(
            stdout.starts_with("ERR sshd launched sshd did not own"),
            "{stdout:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn native_resume_requires_the_exact_safe_reachable_sshd_policy() {
        use std::os::unix::fs::PermissionsExt as _;

        let (_build, binary) = compile_resume_helper();
        let root = tempfile::tempdir().expect("native sshd-config root");
        let ssh_dir = root.path().join("etc/ssh");
        std::fs::create_dir_all(&ssh_dir).expect("create ssh config directory");
        let config = ssh_dir.join("sshd_config.rooms-resume");
        let run = |content: &str| {
            std::fs::write(&config, content).expect("write native sshd config fixture");
            std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o644))
                .expect("protect native sshd config fixture");
            std::process::Command::new(&binary)
                .args([
                    "--test-config",
                    root.path().to_str().expect("utf-8 config root"),
                ])
                .output()
                .expect("validate native sshd config fixture")
        };
        let safe = canonical_resume_sshd_config();
        assert!(run(&safe).status.success());
        let unsafe_configs = [
            safe.replace("ListenAddress 0.0.0.0", "ListenAddress 127.0.0.1"),
            safe.replace("PermitRootLogin no", "PermitRootLogin yes"),
            safe.replace("PasswordAuthentication no", "PasswordAuthentication yes"),
            safe.replace("AllowTcpForwarding no", "AllowTcpForwarding yes"),
            safe.replace(
                "HostKey /etc/ssh/ssh_host_ed25519_key\n",
                "HostKey /etc/ssh/ssh_host_ed25519_key\nHostKey /tmp/alternate\n",
            ),
            safe.replace("AllowUsers rooms\n", ""),
        ];
        for unsafe_config in unsafe_configs {
            assert!(!run(&unsafe_config).status.success(), "{unsafe_config}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn native_resume_stages_exact_identity_and_rejects_symlink_destinations() {
        let (_build, binary) = compile_resume_helper();
        let root = tempfile::tempdir().expect("native resume staging root");
        prepare_hostile_worktree_config(root.path());

        let room_id = "01aaaaaaaaaaaaaaaaaaaaaaaa";
        let output = std::process::Command::new(&binary)
            .args([
                "--test-stage",
                root.path().to_str().expect("utf-8 temp path"),
                room_id,
            ])
            .output()
            .expect("stage native resume metadata");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_staged_identity_is_effective(root.path(), room_id);
        assert_native_stage_rejects_config_symlink(&binary, room_id);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn process_baseline_rejects_a_background_shell_descendant() {
        let output = run_agent_shell(
            r#"
            ROOMS_AGENT_LIBRARY_ONLY=1; export ROOMS_AGENT_LIBRARY_ONLY
            . "$AGENT"
            baseline="$(mktemp)"
            capture_process_baseline "$baseline"
            /bin/sh -c 'while :; do sleep 1; done' </dev/null >/dev/null 2>&1 &
            child=$!
            accepted=0
            no_post_warm_processes "$baseline" || accepted=$?
            kill "$child" 2>/dev/null || true
            wait "$child" 2>/dev/null || true
            rm -f "$baseline"
            [ "$accepted" -ne 0 ]
            "#,
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
