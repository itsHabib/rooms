//! Run commands inside a booted microVM, via SSH.
//!
//! POC: shells out to the `ssh` client. A native russh/openssh-rs client is
//! a productionization concern.

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::process::Command;
use tokio::time::sleep;
use tracing::{debug, info};

/// Probe the guest's sshd until it accepts a pubkey connection, or `timeout`
/// elapses. Returns the last underlying error on timeout so failure modes
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

/// Exec `command` in the guest as root via SSH. Wires the guest's stdin from
/// /dev/null, and inherits stdout / stderr so guest output flows to the host's
/// fds directly (operators can pipe `rooms run --command '...' | jq`).
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
