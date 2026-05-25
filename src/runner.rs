//! Run commands inside a booted microVM, via SSH.
//!
//! POC: shells out to the `ssh` client. A native russh/openssh-rs client is
//! a productionization concern.

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::time::sleep;
use tracing::{debug, info};

use crate::artifacts::{ResultJson, RunStatus};

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
    // `getattr(sys.stdin, "buffer", sys.stdin).read()` reads bytes on both
    // py2 and py3: on py2 the `buffer` attr doesn't exist so the fallback
    // returns `sys.stdin` itself (whose `.read()` is bytes), on py3 the
    // `buffer` attr is the binary stream backing the text wrapper. Without
    // this, a py3 invocation would decode stdin as text and `struct.pack`
    // would type-error. Today's bionic rootfs only has py2, but this keeps
    // the workaround forward-compatible until rootfs-builder retires it.
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

/// Outcome of a guest command exec, including artifact metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestExecOutcome {
    pub exit_code: i32,
    pub status: RunStatus,
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
}

/// Exec `command` in the guest as root via SSH.
///
/// Captures stdout/stderr under `/workspace/out/logs/` and writes
/// `/workspace/out/result.json` per the runner contract. Guest output is not
/// inherited on the host — use `rooms collect --from` after exec to inspect logs.
///
/// Exit code: parsed from an `EXIT=<n>` marker line printed after the user
/// command runs inside a subshell, so genuine guest exit codes (including 255,
/// and `exit N` / `set -e; false` cases that abort the shell) round-trip
/// without being conflated with SSH transport errors.
///
/// Returns `Err` only when the wrapper never ran to completion (no EXIT=
/// marker in stdout). In that case the spawned `ssh` failed before the
/// wrapper could emit its trailer — network, auth, sshd not listening — and
/// the caller should treat it as a substrate-level transport failure.
///
/// Forwards `ANTHROPIC_API_KEY` from the host process env via SSH's `SendEnv`
/// option. The matching `AcceptEnv ANTHROPIC_API_KEY` lives in the rootfs's
/// `/etc/ssh/sshd_config`, baked by `scripts/bake-rootfs-ssh.sh`.
pub async fn exec_in_guest(
    guest_ip: &str,
    key_path: &Path,
    command: &str,
) -> Result<GuestExecOutcome> {
    let started_at = Utc::now();
    // Run the user command through `bash -c <quoted>` instead of inlining
    // it into the wrapper string. Inlining was vulnerable to shell-meta
    // injection: `--command 'echo hi # note'` would comment out the
    // wrapper's closing `)` and EXIT= trailer on the same line, breaking
    // the marker contract. Single-quoting around the command (with
    // standard `'\''` escaping for embedded singles) makes the whole
    // user input a single argument to bash -c, so no syntax in it can
    // reach the outer wrapper. `bash -c` also gives us subshell
    // isolation, so a user `exit 42` aborts only the inner bash and the
    // wrapper's `echo EXIT=$?` still prints.
    let quoted_command = shell_single_quote(command);
    let remote = format!(
        "mkdir -p /workspace/out/logs && \
         bash -c {quoted_command} > /workspace/out/logs/stdout.log 2> /workspace/out/logs/stderr.log; \
         echo EXIT=$?"
    );
    let output = ssh_command(guest_ip, key_path, &remote)?
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await
        .context("failed to spawn ssh; is openssh-client installed?")?;

    let ended_at = Utc::now();
    // Decide failure mode by looking for the EXIT= marker. If we see it,
    // SSH ran the wrapper to completion and the user command's exit code
    // is in `output.stdout` — even if SSH itself returned non-zero (which
    // it does when, e.g., the user's last command exits non-zero and bash
    // surfaces that). If we don't see it, the wrapper never ran and this
    // is genuine SSH-transport failure.
    let stdout_str = String::from_utf8_lossy(&output.stdout);
    if !stdout_str.lines().any(|l| l.starts_with("EXIT=")) {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "guest exec via SSH failed before wrapper completed (ssh exit {}): {stderr}",
            output.status
        );
    }

    let exit_code = parse_remote_exit_code(&output.stdout)?;
    let status = ResultJson::status_from_exit_code(exit_code);
    let result = ResultJson::from_exec(
        exit_code,
        status,
        started_at,
        ended_at,
        guest_command_argv(command),
    );
    write_guest_result_json(guest_ip, key_path, &result).await?;

    Ok(GuestExecOutcome {
        exit_code,
        status,
        started_at,
        ended_at,
    })
}

/// Ensure the guest artifact dir + empty log files exist.
///
/// `exec_in_guest` creates these as a side effect of running the wrapped
/// command, but on a Ctrl-C that fires before exec actually started they're
/// missing — and `RunnerArtifacts::load` then bails with `MissingRequired`
/// even though `result.json` has been written. Touching empty placeholders
/// keeps the contract intact for cancelled runs.
pub async fn ensure_guest_artifact_skeleton(guest_ip: &str, key_path: &Path) -> Result<()> {
    let remote = "mkdir -p /workspace/out/logs \
         && : > /workspace/out/logs/stdout.log \
         && : > /workspace/out/logs/stderr.log";
    let output = ssh_command(guest_ip, key_path, remote)?
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await
        .context("failed to spawn ssh; is openssh-client installed?")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "creating cancelled-run artifact skeleton failed (exit {}): {stderr}",
            output.status
        );
    }
    Ok(())
}

/// Write `result.json` into the guest artifact directory.
pub async fn write_guest_result_json(
    guest_ip: &str,
    key_path: &Path,
    result: &ResultJson,
) -> Result<()> {
    let json = serde_json::to_string_pretty(result).context("serialize result.json")?;
    let mut child = ssh_command(
        guest_ip,
        key_path,
        "mkdir -p /workspace/out && cat > /workspace/out/result.json",
    )?
    .stdin(Stdio::piped())
    .stdout(Stdio::null())
    .stderr(Stdio::piped())
    .kill_on_drop(true)
    .spawn()
    .context("spawn ssh to write result.json")?;

    let mut stdin = child
        .stdin
        .take()
        .context("result.json ssh has no stdin (unexpected)")?;
    stdin
        .write_all(json.as_bytes())
        .await
        .context("write result.json to ssh stdin")?;
    stdin
        .shutdown()
        .await
        .context("close ssh stdin after result.json")?;
    drop(stdin);

    let output = child
        .wait_with_output()
        .await
        .context("wait on result.json ssh")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "write result.json via SSH failed (exit {}): {stderr}",
            output.status
        );
    }
    Ok(())
}

fn guest_command_argv(command: &str) -> Vec<String> {
    vec!["sh".to_owned(), "-c".to_owned(), command.to_owned()]
}

/// Wrap `s` in single quotes for safe inclusion as a bash argument.
///
/// Uses the standard `'\''` escape: end the current single-quoted string,
/// insert a literal single quote, then start a new single-quoted string.
/// Result is always a single argv token regardless of the input's content.
fn shell_single_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

fn parse_remote_exit_code(stdout: &[u8]) -> Result<i32> {
    let text = String::from_utf8_lossy(stdout);
    // Scan lines (last wins) for the `EXIT=<n>` marker. The wrapper
    // appends it after running the user command in a subshell, so even
    // commands that print to stdout don't mask the marker.
    let marker = text
        .lines()
        .filter_map(|line| line.strip_prefix("EXIT="))
        .next_back()
        .with_context(|| format!("guest stdout missing EXIT= marker; raw stdout: {text:?}"))?;
    marker
        .trim()
        .parse::<i32>()
        .with_context(|| format!("EXIT= marker not numeric: {marker:?}; raw stdout: {text:?}"))
}

fn ssh_command(guest_ip: &str, key_path: &Path, remote: &str) -> Result<Command> {
    let key = key_path.to_str().context("key path not utf-8")?;
    let mut cmd = Command::new("ssh");
    cmd.args([
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
        "-o",
        "SendEnv=ANTHROPIC_API_KEY",
        &format!("root@{guest_ip}"),
        "--",
        remote,
    ]);
    Ok(cmd)
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

    use super::{shell_single_quote, wait_for_ssh};

    #[test]
    fn shell_single_quote_handles_meta_and_embedded_quotes() {
        assert_eq!(shell_single_quote("echo hi"), "'echo hi'");
        // The codex finding: `echo hi # note` previously broke the wrapper
        // because `#` started a comment. With quoting it's just data.
        assert_eq!(shell_single_quote("echo hi # note"), "'echo hi # note'");
        // Embedded single quotes use the standard `'\''` escape.
        assert_eq!(shell_single_quote("echo 'hello'"), r"'echo '\''hello'\'''");
        // Closing-paren can't escape the wrapper either.
        assert_eq!(shell_single_quote("echo ) rm -rf /"), "'echo ) rm -rf /'");
    }

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
