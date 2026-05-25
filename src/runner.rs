//! Run commands inside a booted microVM, via SSH.

use std::path::Path;
use std::process::Stdio;
use std::time::Instant;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::time::sleep;
use tracing::{debug, info};

use crate::config::RoomsConfig;
use crate::error::{FirecrackerError, RunnerError};

/// Probe the guest's sshd until it accepts a pubkey connection, or timeout elapses.
pub async fn wait_for_ssh(
    guest_ip: &str,
    key_path: &Path,
    config: &RoomsConfig,
) -> Result<(), FirecrackerError> {
    let key = key_path
        .to_str()
        .ok_or(RunnerError::KeyPathNotUtf8)
        .map_err(FirecrackerError::from)?;
    let timeout = config.guest_reach_timeout;
    let poll = config.guest_reach_poll_interval;
    let deadline = Instant::now() + timeout;
    let mut last_err = String::new();

    loop {
        if Instant::now() >= deadline {
            return Err(FirecrackerError::GuestUnreachable {
                reason: format!(
                    "sshd at {guest_ip} did not accept connections within {timeout:?} (last stderr: {last_err})"
                ),
            });
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
            .map_err(|e| RunnerError::SshProbe(e.to_string()))
            .map_err(FirecrackerError::from)?;

        if output.status.success() {
            info!(guest_ip, "sshd accepted pubkey connection");
            return Ok(());
        }

        last_err = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        debug!(guest_ip, stderr = %last_err, "sshd probe failed; retrying");
        sleep(poll).await;
    }
}

/// Seed host entropy into the guest CRNG via SSH.
pub async fn seed_entropy(guest_ip: &str, key_path: &Path) -> Result<(), RunnerError> {
    let key = key_path.to_str().ok_or(RunnerError::KeyPathNotUtf8)?;
    let mut host_random = tokio::fs::File::open("/dev/urandom")
        .await
        .map_err(RunnerError::Io)?;
    let mut seed = vec![0_u8; 512];
    host_random
        .read_exact(&mut seed)
        .await
        .map_err(RunnerError::Io)?;
    drop(host_random);

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
             data = getattr(sys.stdin, \"buffer\", sys.stdin).read()\n\
             buf = struct.pack(\"ii%ds\" % len(data), len(data)*8, len(data), data)\n\
             fcntl.ioctl(open(\"/dev/random\", \"wb\"), 0x40085203, buf)'",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| RunnerError::EntropySeed(e.to_string()))?;

    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| RunnerError::EntropySeed("entropy-seed ssh has no stdin".to_owned()))?;
    stdin.write_all(&seed).await.map_err(RunnerError::Io)?;
    stdin.shutdown().await.map_err(RunnerError::Io)?;
    drop(stdin);

    let output = child.wait_with_output().await.map_err(RunnerError::Io)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(RunnerError::EntropySeed(format!(
            "entropy seed via SSH failed (exit {}): {stderr}",
            output.status
        )));
    }
    info!(guest_ip, "seeded 512 bytes of host entropy into guest CRNG");
    Ok(())
}

/// Exec `command` in the guest as root via SSH.
pub async fn exec_in_guest(
    guest_ip: &str,
    key_path: &Path,
    command: &str,
) -> Result<i32, RunnerError> {
    let status = Command::new("ssh")
        .args([
            "-i",
            key_path.to_str().ok_or(RunnerError::KeyPathNotUtf8)?,
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
        .map_err(|e| RunnerError::Exec(e.to_string()))?;

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
        reason = "test module"
    )]

    use super::wait_for_ssh;
    use crate::config::RoomsConfig;

    #[tokio::test]
    async fn wait_for_ssh_times_out_when_no_sshd() {
        let key_path = std::path::Path::new("/nonexistent/key");
        let config = RoomsConfig {
            guest_reach_timeout: std::time::Duration::from_secs(2),
            guest_reach_poll_interval: std::time::Duration::from_secs(1),
            ..RoomsConfig::default()
        };
        let start = std::time::Instant::now();

        let err = wait_for_ssh("127.0.0.255", key_path, &config)
            .await
            .expect_err("unreachable address should time out");

        assert!(
            matches!(err, crate::error::FirecrackerError::GuestUnreachable { .. }),
            "unexpected error: {err}"
        );
        assert!(
            start.elapsed() >= config.guest_reach_timeout,
            "should wait at least the timeout duration"
        );
    }
}
