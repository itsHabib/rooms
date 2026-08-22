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
        assert!(agent.contains("verify_protected_state"));
        assert_eq!(agent.matches("credential_state_is_safe || fail").count(), 2);

        let resume = include_str!("../scripts/lib/rooms-resume-agent.sh");
        assert!(resume.contains("fresh_git_identity \"$room_id\""));
        assert!(resume.contains("destination=/home/rooms/.gitconfig"));
        assert!(resume.contains("chmod 0600 \"$pending\""));
        assert!(resume.contains("@rooms.invalid"));
        assert!(!resume.contains("credential.helper"));
        let open = '{';
        let close = '}';
        let library_guard = format!("&& [ \"${open}1:-{close}\" = __rooms_test_library__ ]");
        assert!(resume.contains(&library_guard));
        assert!(resume.contains("interval=0.1"));
        assert!(!resume.contains("interval=2"));
        assert!(resume.contains("ssh-keygen -q -t ed25519 -N '' -f \"$SSH_HOST_KEY\""));
        assert_eq!(resume.matches("-t ed25519").count(), 1);
        assert!(!resume.contains("ssh-keygen -A"));
        assert!(!resume.to_ascii_lowercase().contains("rsa"));
        assert!(!resume.to_ascii_lowercase().contains("ecdsa"));
        assert!(resume.contains("printf 'HostKey %s\\n' \"$SSH_HOST_KEY\""));
        assert!(resume.contains("\"$SSHD_BIN\" -T -f \"$SSHD_CONFIG\""));
        assert!(resume.contains("restore_workload_sudo"));
        assert!(resume.contains("chmod 0440 \"$pending\""));
        assert!(resume.contains("\"$SSHD_BIN\" -f \"$SSHD_CONFIG\""));
        assert!(!resume.contains("rc-service sshd"));
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

    // Git-identity rendering is pure POSIX shell and never touches guest-only
    // paths, so keep these focused resume-agent tests active on every host.
    fn run_resume_agent_shell(body: &str) -> std::process::Output {
        let agent = format!(
            "{}/scripts/lib/rooms-resume-agent.sh",
            env!("CARGO_MANIFEST_DIR")
        );
        std::process::Command::new("/bin/sh")
            .args(["-c", body])
            .env("AGENT", agent)
            .output()
            .expect("run resume-agent shell test")
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
        let output = run_resume_agent_shell(
            r#"
            set -- __rooms_test_library__
            ROOMS_AGENT_LIBRARY_ONLY=1; export ROOMS_AGENT_LIBRARY_ONLY
            . "$AGENT"
            PROVISION_AGENT="$AGENT"
            # The resume agent and provision agent intentionally have different
            # library guards; source the policy helpers from the latter too.
            ROOMS_AGENT_LIBRARY_ONLY=1 . "$(dirname "$PROVISION_AGENT")/rooms-provision-agent.sh"

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
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn resume_git_identity_is_deterministic_distinct_and_credential_free() {
        let output = run_resume_agent_shell(
            r#"
            set -- __rooms_test_library__
            ROOMS_AGENT_LIBRARY_ONLY=1; export ROOMS_AGENT_LIBRARY_ONLY
            . "$AGENT"
            tmp="$(mktemp -d)"
            trap 'rm -rf "$tmp"' EXIT
            first_id=01aaaaaaaaaaaaaaaaaaaaaaaa
            second_id=01bbbbbbbbbbbbbbbbbbbbbbbb
            write_git_identity "$first_id" "$tmp/first"
            write_git_identity "$first_id" "$tmp/repeat"
            write_git_identity "$second_id" "$tmp/second"
            printf '[user]\n\tname = rooms %s\n\temail = %s@rooms.invalid\n' \
                "$first_id" "$first_id" >"$tmp/expected"
            cmp -s "$tmp/first" "$tmp/expected"
            cmp -s "$tmp/first" "$tmp/repeat"
            ! cmp -s "$tmp/first" "$tmp/second"
            [ "$(git config --file "$tmp/first" --get user.name)" = "rooms $first_id" ]
            [ "$(git config --file "$tmp/first" --get user.email)" = "$first_id@rooms.invalid" ]
            ! grep -qi credential "$tmp/first"
            "#,
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn resume_generates_one_fresh_ed25519_key_and_pins_sshd_to_it() {
        let output = run_resume_agent_shell(
            r#"
            set -- __rooms_test_library__
            ROOMS_AGENT_LIBRARY_ONLY=1; export ROOMS_AGENT_LIBRARY_ONLY
            . "$AGENT"
            tmp="$(mktemp -d)"
            trap 'rm -rf "$tmp"' EXIT
            SSH_HOST_KEY_DIR="$tmp/ssh"
            SSH_HOST_KEY="$SSH_HOST_KEY_DIR/ssh_host_ed25519_key"
            SSHD_SOURCE_CONFIG="$tmp/sshd_config.source"
            SSHD_CONFIG="$tmp/sshd_config.runtime"
            SSHD_RUNTIME_DIR="$tmp/run-sshd"
            ROOT_UID="$(id -u)"
            ROOT_GID="$(id -g)"
            # Production validates Linux ownership with GNU/BusyBox stat. This
            # cross-platform unit owns only config/key semantics; the Linux
            # guest test exercises the real runtime-directory helper.
            prepare_sshd_runtime_dir() { mkdir -p "$SSHD_RUNTIME_DIR"; }
            mkdir "$SSH_HOST_KEY_DIR"
            printf old >"$SSH_HOST_KEY_DIR/ssh_host_rsa_key"
            printf sentinel >"$tmp/sentinel"
            ln -s "$tmp/sentinel" "$SSH_HOST_KEY_DIR/ssh_host_ecdsa_key"
            cat >"$SSHD_SOURCE_CONFIG" <<'EOF'
HostKey /etc/ssh/ssh_host_rsa_key
Include /etc/ssh/sshd_config.d/*.conf
HostKey=/tmp/equal-alternate
HostCertificate = /tmp/equal-certificate
HostKeyAgent=/tmp/equal-agent
Include=/tmp/equal-include.conf
PermitRootLogin no
Match User nobody
    X11Forwarding no
EOF

            fresh_ssh_host_key
            first="$(cat "$SSH_HOST_KEY.pub")"
            [ "$(find "$SSH_HOST_KEY_DIR" -mindepth 1 -maxdepth 1 -print | wc -l)" -eq 2 ]
            [ -f "$SSH_HOST_KEY" ] && [ ! -L "$SSH_HOST_KEY" ]
            [ -f "$SSH_HOST_KEY.pub" ] && [ ! -L "$SSH_HOST_KEY.pub" ]
            [ "$(cat "$tmp/sentinel")" = sentinel ]
            ssh_host_key_pair_matches "$SSH_HOST_KEY" "$SSH_HOST_KEY.pub"
            pin_sshd_to_fresh_host_key
            pinned_sshd_config_is_safe "$SSHD_CONFIG" "$SSH_HOST_KEY"
            [ "$(grep -ci '^[[:space:]]*HostKey[[:space:]]' "$SSHD_CONFIG")" -eq 1 ]
            ! grep -Eqi '^[[:space:]]*(HostKey|HostCertificate|HostKeyAgent|Include)[[:space:]]*=' \
                "$SSHD_CONFIG"
            ! grep -qi '^[[:space:]]*Include[[:space:]]' "$SSHD_CONFIG"
            printf 'hostkey %s\n' "$SSH_HOST_KEY" \
                | effective_sshd_host_keys_are_safe "$SSH_HOST_KEY"
            ! printf 'hostkey %s\nhostkey /tmp/alternate\n' "$SSH_HOST_KEY" \
                | effective_sshd_host_keys_are_safe "$SSH_HOST_KEY"

            fresh_ssh_host_key
            second="$(cat "$SSH_HOST_KEY.pub")"
            [ "$first" != "$second" ]
            mkdir "$SSH_HOST_KEY_DIR/ssh_host_bad_key"
            if (fresh_ssh_host_key) >/dev/null 2>&1; then
                exit 1
            fi
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
    fn resume_prepares_an_exact_owned_sshd_runtime_directory() {
        let output = run_resume_agent_shell(
            r#"
            set -- __rooms_test_library__
            ROOMS_AGENT_LIBRARY_ONLY=1; export ROOMS_AGENT_LIBRARY_ONLY
            . "$AGENT"
            tmp="$(mktemp -d)"
            trap 'rm -rf "$tmp"' EXIT
            SSHD_RUNTIME_DIR="$tmp/run-sshd"
            ROOT_UID="$(id -u)"
            ROOT_GID="$(id -g)"
            prepare_sshd_runtime_dir
            [ ! -L "$SSHD_RUNTIME_DIR" ] && [ -d "$SSHD_RUNTIME_DIR" ]
            [ "$(stat -c '%u:%g' "$SSHD_RUNTIME_DIR")" = "$ROOT_UID:$ROOT_GID" ]
            [ "$(stat -c '%a' "$SSHD_RUNTIME_DIR")" = 755 ]
            "#,
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn resume_repository_git_identity_overrides_only_author_and_rejects_symlink() {
        let output = run_resume_agent_shell(
            r#"
            set -- __rooms_test_library__
            ROOMS_AGENT_LIBRARY_ONLY=1; export ROOMS_AGENT_LIBRARY_ONLY
            . "$AGENT"
            tmp="$(mktemp -d)"
            trap 'rm -rf "$tmp"' EXIT
            room_id=01aaaaaaaaaaaaaaaaaaaaaaaa

            git init -q "$tmp/repo"
            git -C "$tmp/repo" config user.name inherited
            git -C "$tmp/repo" config user.email inherited@example.com
            git -C "$tmp/repo" config rooms.keep unchanged
            update_repository_git_identity "$room_id" "$tmp/repo"
            [ "$(git -C "$tmp/repo" config user.name)" = "rooms $room_id" ]
            [ "$(git -C "$tmp/repo" config user.email)" = "$room_id@rooms.invalid" ]
            [ "$(git -C "$tmp/repo" config rooms.keep)" = unchanged ]

            git init -q "$tmp/hostile"
            cp "$tmp/hostile/.git/config" "$tmp/sentinel"
            rm "$tmp/hostile/.git/config"
            ln -s "$tmp/sentinel" "$tmp/hostile/.git/config"
            cp "$tmp/sentinel" "$tmp/before"
            if (update_repository_git_identity "$room_id" "$tmp/hostile") >/dev/null 2>&1; then
                exit 1
            fi
            cmp -s "$tmp/sentinel" "$tmp/before"
            "#,
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn resume_library_hook_requires_both_private_argument_and_environment() {
        let output = run_resume_agent_shell(
            r#"
            if ROOMS_AGENT_LIBRARY_ONLY=1 /bin/sh "$AGENT" invalid >/dev/null 2>&1; then
                exit 1
            fi
            if /bin/sh "$AGENT" __rooms_test_library__ >/dev/null 2>&1; then
                exit 1
            fi
            "#,
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn resume_git_identity_rejects_shell_metacharacters_before_writing() {
        let output = run_resume_agent_shell(
            r#"
            set -- __rooms_test_library__
            ROOMS_AGENT_LIBRARY_ONLY=1; export ROOMS_AGENT_LIBRARY_ONLY
            . "$AGENT"
            tmp="$(mktemp -d)"
            trap 'rm -rf "$tmp"' EXIT
            sentinel="$tmp/pwned"
            bad="01abc; touch $sentinel"
            if (write_git_identity "$bad" "$tmp/config") >/dev/null 2>&1; then
                exit 1
            fi
            [ ! -e "$tmp/config" ]
            [ ! -e "$sentinel" ]
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
