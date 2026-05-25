//! Run commands inside a booted microVM, via SSH.
//!
//! POC: shells out to the `ssh` client. A native russh/openssh-rs client is
//! a productionization concern.

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::time::sleep;
use tracing::{debug, info};

/// Probe the guest's sshd until it accepts a pubkey connection, or `timeout` elapses.
///
/// Returns the last underlying error on timeout so failure modes
/// (network down, key not baked, sshd never started) are debuggable from the
/// surface error.
pub async fn wait_for_ssh(guest_ip: &str, key_path: &Path, timeout: Duration) -> Result<()> {
    let key = key_path.to_str().context("key path not utf-8")?;
    let deadline = Instant::now() + timeout;
    let mut last_err = String::new();

    loop {
        if Instant::now() >= deadline {
            anyhow::bail!(
                "sshd at {guest_ip} did not accept connections within {timeout:?} (last stderr: {last_err})"
            );
        }

        let output = Command::new("ssh")
            .args([
                "-i",
                key,
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=2",
                "-o",
                "StrictHostKeyChecking=no",
                "-o",
                "UserKnownHostsFile=/dev/null",
                "-o",
                "LogLevel=ERROR",
                &format!("root@{guest_ip}"),
                "true",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .await
            .context("failed to spawn ssh probe; is openssh-client installed?")?;

        if output.status.success() {
            info!(guest_ip, "sshd accepted pubkey connection");
            return Ok(());
        }

        last_err = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        debug!(guest_ip, stderr = %last_err, "sshd probe failed; retrying");
        sleep(Duration::from_secs(1)).await;
    }
}

/// Seed 512 bytes of host entropy into the guest's kernel CRNG via
/// `RNDADDENTROPY` on `/dev/random`.
///
/// The bundled bionic kernel has no `virtio-rng` driver and ignores
/// `random.trust_cpu` (added in 4.19), so the guest's CRNG never initializes
/// from any internal source. `getrandom()` blocks indefinitely and every TLS
/// handshake hangs silently after TCP connect. Without this seed, `rooms run
/// --command 'curl https://...'` cannot reach any HTTPS endpoint.
///
/// Implementation: read 512 bytes from the host's `/dev/urandom` (the host's
/// CRNG has plenty of entropy), pipe through SSH stdin to a python one-liner
/// that builds a `rand_pool_info` and calls the `RNDADDENTROPY` ioctl. 512
/// bytes credits 4096 bits — well past the 384 bits the kernel needs to
/// transition `crng_init` from `unseeded` to `ready`. Empirically lifts
/// `entropy_avail` from ~30 to ~2200.
///
/// 512 (not 1024) because Python's `fcntl.ioctl` default `buf` size cap is
/// 1024 bytes; the ioctl struct adds 8 bytes of header (`entropy_count` +
/// `buf_size`), so 1024 of payload overshoots and `ioctl` raises `ValueError:
/// ioctl string arg too long`. 512 + 8 = 520 stays safely under.
///
/// Goes away when the productionization rootfs builder ships a kernel with
/// `CONFIG_HW_RANDOM_VIRTIO=y` and our `/entropy` device attaches as
/// `/dev/hwrng`.
pub async fn seed_entropy(guest_ip: &str, key_path: &Path) -> Result<()> {
    let key = key_path.to_str().context("key path not utf-8")?;
    let mut host_random = tokio::fs::File::open("/dev/urandom")
        .await
        .context("open /dev/urandom on host (every Linux has this)")?;
    let mut seed = vec![0_u8; 512];
    host_random
        .read_exact(&mut seed)
        .await
        .context("read 512 bytes from host /dev/urandom")?;
    drop(host_random);

    // The ioctl number 0x40085203 is RNDADDENTROPY on x86_64. The struct
    // packed below is `struct rand_pool_info { int entropy_count; int
    // buf_size; __u32 buf[]; }` from include/uapi/linux/random.h, credit
    // = len*8 bits, size = len bytes, payload = the 512 stdin bytes.
    //
    // The python invocation hard-codes `python` (which on bionic resolves to
    // 2.7) because `sys.stdin.read()` returns bytes on py2; under py3 it would
    // return a text-decoded str and `struct.pack(..., data)` would raise. When
    // the rootfs builder ships a python3-only image, the whole seed_entropy
    // step disappears (the new kernel will have virtio-rng), so this py2 tie
    // is intentional and short-lived.
    let mut child = Command::new("ssh")
        .args([
            "-i",
            key,
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=5",
            "-o",
            "StrictHostKeyChecking=no",
            "-o",
            "UserKnownHostsFile=/dev/null",
            "-o",
            "LogLevel=ERROR",
            &format!("root@{guest_ip}"),
            "--",
            "python -c 'import sys, struct, fcntl\n\
             data = sys.stdin.read()\n\
             buf = struct.pack(\"ii%ds\" % len(data), len(data)*8, len(data), data)\n\
             fcntl.ioctl(open(\"/dev/random\", \"wb\"), 0x40085203, buf)'",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("spawn ssh for entropy seed")?;

    let mut stdin = child
        .stdin
        .take()
        .context("entropy-seed ssh has no stdin (unexpected)")?;
    stdin
        .write_all(&seed)
        .await
        .context("write seed bytes to ssh stdin")?;
    stdin
        .shutdown()
        .await
        .context("close ssh stdin after seed")?;
    drop(stdin);

    let output = child
        .wait_with_output()
        .await
        .context("wait on entropy-seed ssh")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "entropy seed via SSH failed (exit {}): {stderr}",
            output.status
        );
    }
    info!(guest_ip, "seeded 512 bytes of host entropy into guest CRNG");
    Ok(())
}

/// Exec `command` in the guest as root via SSH.
///
/// Wires the guest's stdin from /dev/null, and inherits stdout / stderr so guest
/// output flows to the host's fds directly (operators can pipe
/// `rooms run --command '...' | jq`).
///
/// Returns the guest command's exit code clamped to `0..=255`:
/// - guest exited normally with `ExitStatus.code() == Some(n)` → returns `n`
///   (always in `0..=255` per POSIX). The clamp to `u8` happens in the caller,
///   not here — keep the wider type so the caller can distinguish "ran" from
///   any future "did not run" sentinel.
/// - guest killed by signal: returns `128 + sig.unwrap_or(0)`. Per
///   `ExitStatusExt::signal()`, `sig` is `Option<i32>`, but in practice it's
///   `Some(n)` with `n <= 64` whenever the underlying status was a signal kill,
///   so the result stays in `0..=255`.
///
/// SSH-internal errors (host unreachable, key rejected, ssh-binary missing)
/// surface as `Err` with anyhow context; the bail message names the most likely
/// cause. The SSH-internal exit code 255 is NOT translated — it passes through
/// as `Ok(255)` and is indistinguishable from "remote command exited 255".
/// Documented tradeoff (see Risks).
///
/// Forwards `ANTHROPIC_API_KEY` from the host process env via SSH's `SendEnv`
/// option. The matching `AcceptEnv ANTHROPIC_API_KEY` lives in the rootfs's
/// `/etc/ssh/sshd_config`, baked by `scripts/bake-rootfs-ssh.sh`.
pub async fn exec_in_guest(guest_ip: &str, key_path: &Path, command: &str) -> Result<i32> {
    let status = Command::new("ssh")
        .args([
            "-i",
            key_path.to_str().context("key path not utf-8")?,
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=5",
            "-o",
            "StrictHostKeyChecking=no",
            "-o",
            "UserKnownHostsFile=/dev/null",
            "-o",
            "LogLevel=ERROR",
            "-o",
            "SendEnv=ANTHROPIC_API_KEY",
            &format!("root@{guest_ip}"),
            "--",
            command,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .status()
        .await
        .context("failed to spawn ssh; is openssh-client installed?")?;

    if let Some(code) = status.code() {
        return Ok(code);
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        Ok(128 + status.signal().unwrap_or(0))
    }
    #[cfg(not(unix))]
    {
        Ok(128)
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        reason = "test module: panicky lints are noise in tests"
    )]

    use std::time::Duration;

    use super::wait_for_ssh;

    #[tokio::test]
    async fn wait_for_ssh_times_out_when_no_sshd() {
        let key_path = std::path::Path::new("/nonexistent/key");
        let timeout = Duration::from_secs(2);
        let start = std::time::Instant::now();

        let err = wait_for_ssh("127.0.0.255", key_path, timeout)
            .await
            .expect_err("unreachable address should time out");

        assert!(
            err.to_string().contains("did not accept connections"),
            "unexpected error: {err}"
        );
        assert!(
            start.elapsed() >= timeout,
            "should wait at least the timeout duration"
        );
    }
}
