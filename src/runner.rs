//! Run commands inside a booted microVM, via SSH.
//!
//! POC: shells out to the `ssh` client. A native russh/openssh-rs client is
//! a productionization concern.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time::sleep;
use tracing::{debug, info, warn};

use crate::artifacts::{parse_changeset_stream, ResultJson, RunStatus, CHANGESET_JSON};
use crate::config::RoomsConfig;
use crate::error::FirecrackerError;

/// Guest SSH login user. The agent rootfs accepts key-only login only for the
/// unprivileged `rooms` user (uid 1000); `PermitRootLogin no`.
const GUEST_USER: &str = "rooms";

/// A readiness probe performs a complete SSH handshake and pubkey login. Give
/// concurrent guests enough time to finish that work while keeping connection
/// establishment and the protocol handshake bounded.
const SSH_PROBE_CONNECT_TIMEOUT_OPTION: &str = "ConnectTimeout=10";

/// Short connection bound for best-effort post-run operations. Workload setup
/// uses the configured guest-reach budget instead: clone fan-out opens those
/// authenticated connections concurrently, and a five-second banner timeout
/// is not a meaningful readiness boundary on a contended host.
const SSH_AUXILIARY_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Absolute path of the baked cursor runner script inside the guest image.
const CURSOR_RUNNER_JS: &str = "/opt/rooms/cursor-runner/cursor-runner.js";

/// Guest-side stderr marker the wrapper emits when it can't create
/// `/workspace/out/logs` — i.e. the image gives the runner user no writable
/// `/workspace`. Lets the substrate turn that into an actionable error instead of
/// a phantom `EXIT=1` with empty logs.
const WORKSPACE_UNWRITABLE_MARKER: &str = "ROOMS_WORKSPACE_UNWRITABLE";

/// Which runner drives the guest. The substrate stays agent-agnostic — it only
/// selects the guest-side command shape and, for cursor, clones the repo and
/// stages the input dir first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Runner {
    /// Run the operator's literal command (the existing `--command` path).
    Command(String),
    /// Drive the baked `cursor-runner.js` one-shot against `/workspace/repo`.
    Cursor(CursorRequest),
}

impl Runner {
    /// The argv recorded in `result.json`'s `command` field.
    pub fn command_argv(&self) -> Vec<String> {
        match self {
            Self::Command(command) => guest_command_argv(command),
            Self::Cursor(_) => cursor_command_argv(),
        }
    }
}

/// Inputs for a [`Runner::Cursor`] run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorRequest {
    /// Git URL cloned into `/workspace/repo` in the guest.
    pub repo_url: String,
    /// Prompt sent to the agent (contents of the `--task` file).
    pub task_md: String,
    /// Metadata staged at `/workspace/in/meta.json`.
    pub meta: CursorMeta,
    /// When set, after the agent runs the runner commits its changes and pushes
    /// them to this branch on the repo's remote — the room self-persists its work
    /// (mirrors cursor cloud). Requires `GH_TOKEN` in the host env, forwarded into
    /// the guest via SSH `SendEnv`.
    pub push_branch: Option<String>,
}

/// `/workspace/in/meta.json` payload consumed by `cursor-runner.js`.
///
/// The spec's optional `model_params` / `agent_name` are deferred; v0 wires only
/// the base sha and model id. `cursor-runner.js` reads those two fields and
/// treats any others as absent.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CursorMeta {
    pub base_sha: String,
    pub model_id: String,
}

/// A guest readiness signal observed while waiting for the workload channel.
///
/// The two are distinct on purpose: a kernel that booted is not yet a channel
/// a workload can use, and collapsing them is what hides a guest panic behind
/// a "booted" log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuestSignal {
    /// The guest kernel is up: it answered a network probe (ICMP echo), or —
    /// when ICMP never answers — sshd itself proved the kernel alive.
    GuestReady,
    /// sshd accepted a pubkey connection: the workload channel is usable.
    SshReady,
}

/// Host-side route to one guest.
///
/// A restored clone keeps its frozen guest IP inside a private network
/// namespace, while a normal boot remains directly reachable from the host.
/// Keeping both facts together prevents an individual SSH or ping call from
/// accidentally escaping the clone's namespace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuestTarget<'a> {
    guest_ip: &'a str,
    netns: Option<&'a str>,
}

impl<'a> GuestTarget<'a> {
    /// Direct host-to-guest access (the pre-clone behavior).
    pub const fn flat(guest_ip: &'a str) -> Self {
        Self {
            guest_ip,
            netns: None,
        }
    }

    /// Access with an optional host network namespace.
    pub const fn new(guest_ip: &'a str, netns: Option<&'a str>) -> Self {
        Self { guest_ip, netns }
    }
}

/// Probe the guest's sshd until it accepts a pubkey connection, or the
/// configured timeout elapses.
///
/// Returns `FirecrackerError::GuestUnreachable` (preserving the last underlying
/// stderr) on timeout so callers can distinguish guest-unreachability from
/// host-side substrate failures. Takes `&RoomsConfig` so the probe inherits
/// `guest_reach_timeout` and `guest_reach_poll_interval` knobs without each
/// caller redefining them.
pub async fn wait_for_ssh(
    target: GuestTarget<'_>,
    key_path: &Path,
    config: &RoomsConfig,
) -> Result<(), FirecrackerError> {
    wait_for_ssh_observed(target, key_path, config, false, |_| {}).await
}

/// [`wait_for_ssh`], reporting readiness signals to `observe` as they happen.
///
/// `observe` sees [`GuestSignal::GuestReady`] exactly once and then
/// [`GuestSignal::SshReady`] exactly once, in that order, on the success path;
/// nothing on timeout. With `probe_ping` set, each poll cycle also pings the
/// guest until it first answers — the kernel-is-up signal that is *earlier*
/// than (and independent of) sshd. Without it the ordering guarantee still
/// holds: an accepted SSH connection proves the kernel booted, so both signals
/// fire back-to-back. One deadline bounds the whole wait either way.
pub async fn wait_for_ssh_observed(
    target: GuestTarget<'_>,
    key_path: &Path,
    config: &RoomsConfig,
    probe_ping: bool,
    mut observe: impl FnMut(GuestSignal),
) -> Result<(), FirecrackerError> {
    let guest_ip = target.guest_ip;
    let key = key_path
        .to_str()
        .ok_or_else(|| FirecrackerError::Internal("key path not utf-8".to_owned()))?;
    let timeout = config.guest_reach_timeout;
    let poll = config.guest_reach_poll_interval;
    let deadline = Instant::now() + timeout;
    let mut guest_seen = false;
    let mut last_err = String::new();

    loop {
        if Instant::now() >= deadline {
            return Err(FirecrackerError::GuestUnreachable {
                reason: format!(
                    "sshd at {guest_ip} did not accept connections within {timeout:?} (last stderr: {last_err})"
                ),
            });
        }

        // OpenSSH's per-connection bound is deliberately generous enough for
        // an eight-way handshake on a contended host. The caller's deadline
        // is still authoritative: dropping the timed-out future kills its
        // child (`kill_on_drop` below) instead of allowing one probe to extend
        // a short configured reachability timeout by ten seconds.
        let remaining = deadline.saturating_duration_since(Instant::now());
        let probed = tokio::time::timeout(remaining, probe_ssh_once(target, key)).await;
        let probe = match probed {
            Ok(result) => result?,
            Err(_) => {
                return Err(FirecrackerError::GuestUnreachable {
                    reason: format!(
                        "sshd at {guest_ip} did not accept connections within {timeout:?} (last stderr: {last_err})"
                    ),
                });
            }
        };
        match probe {
            None => {
                info!(guest_ip, "sshd accepted pubkey connection");
                signal_ready(&mut observe, guest_seen);
                return Ok(());
            }
            Some(stderr) => last_err = stderr,
        }
        debug!(guest_ip, stderr = %last_err, "sshd probe failed; retrying");

        if probe_ping && !guest_seen {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match tokio::time::timeout(remaining, probe_ping_once(target)).await {
                Ok(true) => {
                    info!(guest_ip, "guest answered ping (kernel up; sshd not yet)");
                    guest_seen = true;
                    observe(GuestSignal::GuestReady);
                }
                Ok(false) => {}
                Err(_) => {
                    return Err(FirecrackerError::GuestUnreachable {
                        reason: format!(
                            "sshd at {guest_ip} did not accept connections within {timeout:?} (last stderr: {last_err})"
                        ),
                    });
                }
            }
        }
        sleep(poll.min(deadline.saturating_duration_since(Instant::now()))).await;
    }
}

/// Emit the terminal readiness signals: `GuestReady` first when ping never
/// answered (an accepted SSH connection proves the kernel booted), then
/// `SshReady`.
fn signal_ready(observe: &mut impl FnMut(GuestSignal), guest_seen: bool) {
    if !guest_seen {
        observe(GuestSignal::GuestReady);
    }
    observe(GuestSignal::SshReady);
}

/// One sshd pubkey probe: `Ok(None)` accepted, `Ok(Some(stderr))` refused,
/// `Err` when the host-side ssh client couldn't even spawn.
async fn probe_ssh_once(
    target: GuestTarget<'_>,
    key: &str,
) -> Result<Option<String>, FirecrackerError> {
    let output = ssh_probe_command(target, key)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|e| FirecrackerError::Internal(format!("ssh probe spawn failed: {e}")))?;

    if output.status.success() {
        return Ok(None);
    }
    Ok(Some(
        String::from_utf8_lossy(&output.stderr).trim().to_owned(),
    ))
}

fn ssh_probe_command(target: GuestTarget<'_>, key: &str) -> Command {
    let destination = format!("{GUEST_USER}@{}", target.guest_ip);
    let mut command = guest_process(target, "ssh");
    command.args([
        "-i",
        key,
        "-o",
        "BatchMode=yes",
        "-o",
        SSH_PROBE_CONNECT_TIMEOUT_OPTION,
        "-o",
        "StrictHostKeyChecking=no",
        "-o",
        "UserKnownHostsFile=/dev/null",
        "-o",
        "LogLevel=ERROR",
        destination.as_str(),
        "true",
    ]);
    command
}

/// One ICMP echo probe (`ping -c 1 -W 1`). Best-effort observation: any
/// failure — no ping binary, ICMP filtered, no reply — is just `false`.
async fn probe_ping_once(target: GuestTarget<'_>) -> bool {
    ping_command(target)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .status()
        .await
        .is_ok_and(|status| status.success())
}

/// Outcome of a guest command exec, including artifact metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestExecOutcome {
    pub exit_code: i32,
    pub status: RunStatus,
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
    /// A post-run branch-push failure (cursor + `--push-branch` only). The
    /// workload finished and `exit_code`/`result.json` are real; the caller
    /// decides whether the persist failure fails the run.
    pub push_error: Option<String>,
}

/// Drive `runner` in the guest, writing `result.json` per the runner contract.
///
/// The substrate stays agent-agnostic: it dispatches to the literal-command
/// path or the cursor path, both of which route their guest command through the
/// same `EXIT=` wrapper so exit-code capture and `result.json` ownership are
/// identical.
pub async fn exec(
    target: GuestTarget<'_>,
    key_path: &Path,
    runner: &Runner,
    config: &RoomsConfig,
) -> Result<GuestExecOutcome> {
    match runner {
        Runner::Command(command) => exec_in_guest(target, key_path, command, config).await,
        Runner::Cursor(request) => exec_cursor_in_guest(target, key_path, request, config).await,
    }
}

/// Tar-over-ssh: stream the guest's `/workspace/out` into `host_dir`. `GH_TOKEN` not forwarded.
pub async fn collect_out_to_host(
    target: GuestTarget<'_>,
    key_path: &Path,
    host_dir: &Path,
) -> Result<()> {
    // Fresh dir per collection; a missing dir is fine, any other remove failure is fatal.
    match tokio::fs::remove_dir_all(host_dir).await {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("clear --out dir {}", host_dir.display())),
    }
    tokio::fs::create_dir_all(host_dir)
        .await
        .with_context(|| format!("create --out dir {}", host_dir.display()))?;
    // Empty stream (not a `cd` error) when /workspace/out is absent; `.output()` drains both pipes.
    let ssh_out = ssh_command(
        target,
        key_path,
        "if [ -d /workspace/out ]; then tar cf - -C /workspace/out .; else exit 0; fi",
        false,
        SSH_AUXILIARY_CONNECT_TIMEOUT,
    )?
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .kill_on_drop(true)
    .output()
    .await
    .context("failed to run guest tar over ssh")?;
    if !ssh_out.status.success() {
        let stderr = String::from_utf8_lossy(&ssh_out.stderr);
        anyhow::bail!("guest tar failed (exit {}): {stderr}", ssh_out.status);
    }
    if ssh_out.stdout.is_empty() {
        return Ok(());
    }
    // The archive is guest-controlled: reject unsafe members before extracting.
    ensure_tar_regular_only(&ssh_out.stdout).await?;
    let extract = run_host_tar(&ssh_out.stdout, &["-xf", "-", "-C"], Some(host_dir)).await?;
    if !extract.status.success() {
        let stderr = String::from_utf8_lossy(&extract.stderr);
        anyhow::bail!(
            "host tar extract failed (exit {}): {stderr}",
            extract.status
        );
    }
    Ok(())
}

/// One `sudo bash` over the upperdir: emit NUL-delimited `<op>\t<relpath>`
/// records (op `A`/`M`/`D`) for every changed entry, or the `NOOVERLAY` sentinel
/// when there is no overlay (a writable-rootfs run). Runs as root in-process (not
/// per-file sudo) so it also sees root-owned escapes.
///
/// Walks regular files, symlinks, and every special file an agent can leave on
/// the filesystem (block/char devices via `mknod`, FIFOs, a bound socket) — each
/// is a persistent-path lane escape when written outside `/workspace`, and a
/// `-type f`-only walk would miss it. A
/// char device that is `0:0` is an overlayfs **whiteout** (a deletion marker →
/// `D`); any other char device (a real `mknod c`) classifies as a normal escape
/// (→ `A`/`M`), so flattening that check is what stops a non-`0:0` device from
/// being silently dropped. `/oldroot` is the RO lower, `/oldroot/mnt/upper` the
/// tmpfs upper (see `scripts/lib/overlay-init.sh`).
const ENUMERATE_OVERLAY: &str = r#"sudo bash -c 'UP=/oldroot/mnt/upper; LOW=/oldroot
if [ ! -d "$UP" ]; then printf NOOVERLAY; exit 0; fi
find "$UP" \( -type f -o -type l -o -type b -o -type p -o -type s -o -type c \) -print0 | while IFS= read -r -d "" p; do
  rel=${p#"$UP"/}
  if [ -c "$p" ] && [ "$(stat -c %t "$p" 2>/dev/null)" = 0 ] && [ "$(stat -c %T "$p" 2>/dev/null)" = 0 ]; then printf "D\t%s\0" "$rel"
  elif [ -e "$LOW/$rel" ] || [ -L "$LOW/$rel" ]; then printf "M\t%s\0" "$rel"
  else printf "A\t%s\0" "$rel"; fi
done'"#;

/// Enumerate the overlay change set in the guest and write `changeset.json` into
/// `host_dir`. Best-effort and read-only: callers run this after
/// `collect_out_to_host` and never fail a run on its error.
pub async fn collect_changeset_to_host(
    target: GuestTarget<'_>,
    key_path: &Path,
    host_dir: &Path,
) -> Result<()> {
    let ssh_out = ssh_command(
        target,
        key_path,
        ENUMERATE_OVERLAY,
        false,
        SSH_AUXILIARY_CONNECT_TIMEOUT,
    )?
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .kill_on_drop(true)
    .output()
    .await
    .context("failed to enumerate overlay over ssh")?;
    if !ssh_out.status.success() {
        let stderr = String::from_utf8_lossy(&ssh_out.stderr);
        anyhow::bail!(
            "guest overlay enumeration failed (exit {}): {stderr}",
            ssh_out.status
        );
    }
    let changeset = parse_changeset_stream(&ssh_out.stdout);
    let json = serde_json::to_string_pretty(&changeset).context("serialize changeset")?;
    write_host_artifact_atomic(host_dir, CHANGESET_JSON, json.as_bytes()).await
}

/// Atomic artifact write: temp file in the same dir, then rename. A crash
/// mid-write leaves no half-written `changeset.json` for a reader to choke on.
async fn write_host_artifact_atomic(dir: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    let final_path = dir.join(name);
    let tmp_path = dir.join(format!("{name}.tmp"));
    tokio::fs::write(&tmp_path, bytes)
        .await
        .with_context(|| format!("write {}", tmp_path.display()))?;
    tokio::fs::rename(&tmp_path, &final_path)
        .await
        .with_context(|| format!("rename into {}", final_path.display()))
}

/// Reject any unsafe member (link, device, `..`, absolute path) in a guest tar before extraction.
async fn ensure_tar_regular_only(archive: &[u8]) -> Result<()> {
    let listing = run_host_tar(archive, &["-tvf", "-"], None).await?;
    if !listing.status.success() {
        let stderr = String::from_utf8_lossy(&listing.stderr);
        anyhow::bail!(
            "could not list guest tar (exit {}): {stderr}",
            listing.status
        );
    }
    let text = String::from_utf8_lossy(&listing.stdout);
    for line in text.lines() {
        if !tar_member_is_safe(line) {
            anyhow::bail!("refusing unsafe guest archive member: {line}");
        }
    }
    Ok(())
}

/// Safe `tar -tv` member: regular file / dir, no token absolute or with a `..` component (covers spaced names).
fn tar_member_is_safe(line: &str) -> bool {
    if !matches!(line.chars().next(), Some('-' | 'd')) {
        return false;
    }
    !line
        .split_whitespace()
        .any(|tok| tok.starts_with('/') || tok.split('/').any(|c| c == ".."))
}

/// Run host `tar args [path]` with `archive` on stdin (spawned writer + `wait_with_output`, deadlock-free).
async fn run_host_tar(
    archive: &[u8],
    args: &[&str],
    path: Option<&Path>,
) -> Result<std::process::Output> {
    let mut cmd = Command::new("tar");
    cmd.args(args);
    if let Some(p) = path {
        cmd.arg(p);
    }
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("failed to spawn host tar")?;
    let mut stdin = child.stdin.take().context("host tar stdin missing")?;
    let bytes = archive.to_vec();
    let writer = tokio::spawn(async move {
        let _ = stdin.write_all(&bytes).await;
    });
    let out = child
        .wait_with_output()
        .await
        .context("failed to wait for host tar")?;
    let _ = writer.await;
    Ok(out)
}

/// Exec `command` in the guest as the `rooms` user via SSH.
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
/// Forwards `ANTHROPIC_API_KEY`, `CLAUDE_CODE_OAUTH_TOKEN` (the
/// `claude setup-token` credential), `ANTHROPIC_AUTH_TOKEN`, and
/// `CURSOR_API_KEY` from the host process env
/// via SSH's `SendEnv` option; the matching `AcceptEnv` lines live in the guest's
/// `/etc/ssh/sshd_config`. `GH_TOKEN` is not forwarded here — it is sent only on
/// the cursor push step (see `push_branch_in_guest`), so a `--command` run and
/// the agent never see the push token.
pub async fn exec_in_guest(
    target: GuestTarget<'_>,
    key_path: &Path,
    command: &str,
    config: &RoomsConfig,
) -> Result<GuestExecOutcome> {
    let connect_timeout = config.guest_reach_timeout;
    let run = run_wrapped(target, key_path, command, connect_timeout).await?;
    let status = ResultJson::status_from_exit_code(run.exit_code);
    let result = ResultJson::from_exec(
        run.exit_code,
        status,
        run.started_at,
        run.ended_at,
        guest_command_argv(command),
    );
    write_guest_result_json_with_timeout(target, key_path, &result, connect_timeout).await?;

    Ok(GuestExecOutcome {
        exit_code: run.exit_code,
        status,
        started_at: run.started_at,
        ended_at: run.ended_at,
        push_error: None,
    })
}

/// Clone the repo, stage the task + meta, then drive `cursor-runner.js` against
/// `/workspace/repo`, returning its outcome.
///
/// `result.json` is written by the substrate (this function) from the runner's
/// exit code, with the cursor artifact paths set: `cursor-runner.js` always
/// creates `events.ndjson` and `summary.md` — even an auth failure leaves a
/// structured error line plus an empty summary — so those references never
/// dangle. `result.patch` is generated here from `git diff` after the run.
pub async fn exec_cursor_in_guest(
    target: GuestTarget<'_>,
    key_path: &Path,
    request: &CursorRequest,
    config: &RoomsConfig,
) -> Result<GuestExecOutcome> {
    let connect_timeout = config.guest_reach_timeout;
    clone_repo_in_guest(
        target,
        key_path,
        &request.repo_url,
        &request.meta.base_sha,
        connect_timeout,
    )
    .await?;
    stage_cursor_input(
        target,
        key_path,
        &request.task_md,
        &request.meta,
        connect_timeout,
    )
    .await?;

    let run = run_wrapped(
        target,
        key_path,
        &format!("node {CURSOR_RUNNER_JS} < /dev/null"),
        connect_timeout,
    )
    .await?;

    let patch_written = match generate_result_patch(target, key_path, connect_timeout).await {
        Ok(()) => true,
        Err(err) => {
            warn!(error = %err, "failed to generate result.patch; omitting patch_path");
            false
        }
    };

    // Self-persist: if a push branch was requested AND the agent succeeded,
    // commit the agent's changes and push them (mirrors cursor cloud). A failed
    // agent run never pushes its partial work. The push error is captured rather
    // than `?`-propagated, so `result.json` is still written below — recording
    // the run's outcome — before the error surfaces.
    let mut push_err: Option<anyhow::Error> = None;
    let pushed_branch = match (&request.push_branch, run.exit_code) {
        (Some(branch), 0) => {
            match push_branch_in_guest(target, key_path, &request.repo_url, branch, connect_timeout)
                .await
            {
                Ok(true) => Some(branch.clone()),
                Ok(false) => None,
                Err(e) => {
                    push_err = Some(e);
                    None
                }
            }
        }
        _ => None,
    };

    let status = ResultJson::status_from_exit_code(run.exit_code);
    let mut result = ResultJson::from_exec(
        run.exit_code,
        status,
        run.started_at,
        run.ended_at,
        cursor_command_argv(),
    );
    // cursor-runner.js writes events.ndjson + summary.md at startup, but if it
    // never ran (an image missing the cursor extension, or a crash before those
    // writes) the unconditional paths below would dangle and `RunnerArtifacts::
    // load` would reject the otherwise-collectable failed run. Touch placeholders
    // as a backstop — a no-op when the files exist, so a real error line and
    // summary survive.
    run_setup_ssh(
        target,
        key_path,
        "mkdir -p /workspace/out && touch /workspace/out/events.ndjson /workspace/out/summary.md",
        "ensure cursor output artifacts exist",
        connect_timeout,
    )
    .await?;
    result.summary_path = Some("summary.md".to_owned());
    result.events_path = Some("events.ndjson".to_owned());
    if patch_written {
        result.patch_path = Some("result.patch".to_owned());
    }
    result.pushed_branch = pushed_branch;
    write_guest_result_json_with_timeout(target, key_path, &result, connect_timeout).await?;

    // result.json is recorded before the push failure surfaces, so a push
    // error never eats the run's artifact. The outcome carries the failure
    // alongside the real exit code instead of replacing it: the caller keeps
    // both facts — the workload's exit and the persist step that failed.
    Ok(GuestExecOutcome {
        exit_code: run.exit_code,
        status,
        started_at: run.started_at,
        ended_at: run.ended_at,
        push_error: push_err.map(|e| e.to_string()),
    })
}

/// Parsed result of a wrapped guest command: the exit code and the timing the
/// substrate stamps into `result.json`.
struct WrappedRun {
    exit_code: i32,
    started_at: DateTime<Utc>,
    ended_at: DateTime<Utc>,
}

/// Run `inner` (a single shell command) through the guest wrapper that captures
/// stdout/stderr under `/workspace/out/logs/` and prints an `EXIT=<n>` trailer.
///
/// `inner` runs through `bash -c <quoted>` rather than being inlined into the
/// wrapper string. Inlining was vulnerable to shell-meta injection: a `#` in the
/// command would comment out the wrapper's `EXIT=` trailer on the same line,
/// breaking the marker contract. Single-quoting around the command (with the
/// standard `'\''` escape for embedded singles) makes the whole input a single
/// argument to `bash -c`, so no syntax in it can reach the outer wrapper.
/// `bash -c` also gives subshell isolation, so a user `exit 42` aborts only the
/// inner bash and the wrapper's `echo EXIT=$?` still prints.
async fn run_wrapped(
    target: GuestTarget<'_>,
    key_path: &Path,
    inner: &str,
    connect_timeout: Duration,
) -> Result<WrappedRun> {
    let started_at = Utc::now();
    let quoted_command = shell_single_quote(inner);
    let remote = format!(
        "if ! mkdir -p /workspace/out/logs; then echo {WORKSPACE_UNWRITABLE_MARKER} >&2; exit 1; fi; \
         bash -c {quoted_command} > /workspace/out/logs/stdout.log 2> /workspace/out/logs/stderr.log; \
         echo EXIT=$?"
    );
    let output = ssh_command(target, key_path, &remote, false, connect_timeout)?
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await
        .context("failed to spawn ssh; is openssh-client installed?")?;

    let ended_at = Utc::now();
    // A failed `mkdir /workspace/out/logs` means the image gives the runner user
    // no writable /workspace. Catch its marker first and fail with an actionable
    // message — otherwise the `; echo EXIT=$?` trailer reports a phantom exit 1
    // with empty logs (the old silent no-op).
    let stderr_str = String::from_utf8_lossy(&output.stderr);
    if stderr_str.contains(WORKSPACE_UNWRITABLE_MARKER) {
        anyhow::bail!(
            "guest /workspace is not writable by the runner user ({GUEST_USER}); the image \
             must provide a rooms-owned /workspace — agent images built with \
             scripts/build-rootfs-alpine.sh carry it, and scripts/bake-rootfs-ssh.sh \
             provisions it into a stock image"
        );
    }
    // Decide failure mode by looking for the EXIT= marker. If we see it, SSH ran
    // the wrapper to completion and the exit code is in `output.stdout` — even
    // if SSH itself returned non-zero (which it does when the inner command
    // exits non-zero and bash surfaces that). If we don't see it, the wrapper
    // never ran and this is genuine SSH-transport failure.
    let stdout_str = String::from_utf8_lossy(&output.stdout);
    if !stdout_str.lines().any(|l| l.starts_with("EXIT=")) {
        anyhow::bail!(
            "guest exec via SSH failed before wrapper completed (ssh exit {}): {stderr_str}",
            output.status
        );
    }

    let exit_code = parse_remote_exit_code(&output.stdout)?;
    Ok(WrappedRun {
        exit_code,
        started_at,
        ended_at,
    })
}

/// Clone `repo_url` into `/workspace/repo`, check out `base_sha`, and pin the
/// resolved base commit as `refs/rooms/base`.
///
/// Pinning a ref (rather than re-resolving `base_sha` later) keeps the patch and
/// push steps comparing against a concrete commit even when `base_sha` is
/// symbolic (e.g. `HEAD`) — otherwise it would re-resolve to the agent's tip and
/// look like "no changes". A hard error: the cursor runner can't run without a
/// populated repo. The URL and sha are single-quoted into the remote shell so
/// neither can inject.
async fn clone_repo_in_guest(
    target: GuestTarget<'_>,
    key_path: &Path,
    repo_url: &str,
    base_sha: &str,
    connect_timeout: Duration,
) -> Result<()> {
    let url = shell_single_quote(repo_url);
    let sha = shell_single_quote(base_sha);
    let remote = format!(
        "rm -rf /workspace/repo && git clone {url} /workspace/repo && \
         git -C /workspace/repo checkout {sha} && \
         git -C /workspace/repo update-ref refs/rooms/base HEAD"
    );
    run_setup_ssh(
        target,
        key_path,
        &remote,
        "clone repo in guest",
        connect_timeout,
    )
    .await
}

/// Resolve the optional repository entirely on the host and build the
/// credential-free input served to the base agent.
pub fn prepare_base_provisioning(
    repo: Option<&str>,
    warm: Option<&str>,
) -> Result<crate::vsock::ProvisioningPayload, FirecrackerError> {
    if let Some(command) = warm {
        validate_neutral_warm(command)?;
    }
    let bundle = match repo {
        Some(repo) => create_repo_bundle(repo)?,
        None => Vec::new(),
    };
    Ok(crate::vsock::ProvisioningPayload::new(bundle, warm))
}

fn validate_neutral_warm(command: &str) -> Result<(), FirecrackerError> {
    const CREDENTIAL_MARKERS: [&str; 11] = [
        "token=",
        "password=",
        "secret=",
        "oauth=",
        "api_key=",
        "authorization:",
        ".netrc",
        ".ssh/",
        ".git-credentials",
        "credential.helper",
        "gh auth",
    ];
    let lower = command.to_ascii_lowercase();
    if let Some(marker) = CREDENTIAL_MARKERS
        .iter()
        .find(|marker| lower.contains(*marker))
    {
        return Err(FirecrackerError::Internal(format!(
            "base warm-up refuses credential source marker {marker:?}; authenticated warm-up runs after restore"
        )));
    }
    Ok(())
}

fn create_repo_bundle(repo: &str) -> Result<Vec<u8>, FirecrackerError> {
    if repo_url_has_userinfo(repo) {
        return Err(FirecrackerError::Internal(
            "base --repo URL must not embed credentials".to_owned(),
        ));
    }

    let temp = std::env::temp_dir().join(format!("rooms-base-{}", ulid::Ulid::new()));
    std::fs::create_dir(&temp)?;
    let cleanup = TempPath(temp.clone());
    let source = Path::new(repo);
    let repo_dir = if source.exists() {
        std::fs::canonicalize(source)?
    } else {
        let mirror = temp.join("repo.git");
        run_git(
            Path::new("."),
            &["clone", "--mirror", repo, &mirror.to_string_lossy()],
            "resolve base repository on host",
        )?;
        mirror
    };
    let bundle_path = temp.join("repo.bundle");
    run_git(
        &repo_dir,
        &["bundle", "create", &bundle_path.to_string_lossy(), "--all"],
        "create credential-free repository bundle",
    )?;
    let bytes = std::fs::read(&bundle_path)?;
    drop(cleanup);
    Ok(bytes)
}

fn run_git(cwd: &Path, args: &[&str], action: &str) -> Result<(), FirecrackerError> {
    let output = std::process::Command::new("git")
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .args(args)
        .output()
        .map_err(|e| FirecrackerError::Internal(format!("{action}: {e}")))?;
    if output.status.success() {
        return Ok(());
    }
    Err(FirecrackerError::Internal(format!(
        "{action}: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    )))
}

fn repo_url_has_userinfo(repo: &str) -> bool {
    repo.split_once("://")
        .and_then(|(_, rest)| rest.split('/').next())
        .is_some_and(|authority| authority.contains('@'))
}

struct TempPath(PathBuf);

impl Drop for TempPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Write `task.md` and `meta.json` into `/workspace/in/` for `cursor-runner.js`.
async fn stage_cursor_input(
    target: GuestTarget<'_>,
    key_path: &Path,
    task_md: &str,
    meta: &CursorMeta,
    connect_timeout: Duration,
) -> Result<()> {
    write_guest_file(
        target,
        key_path,
        "/workspace/in",
        "task.md",
        task_md.as_bytes(),
        connect_timeout,
    )
    .await?;
    let meta_json = serde_json::to_string_pretty(meta).context("serialize meta.json")?;
    write_guest_file(
        target,
        key_path,
        "/workspace/in",
        "meta.json",
        meta_json.as_bytes(),
        connect_timeout,
    )
    .await
}

/// Generate `/workspace/out/result.patch` from `git diff` against the pinned
/// `refs/rooms/base`, capturing both committed and working-tree changes.
///
/// Best effort: a git error still leaves an (empty) patch file via the `>`
/// redirect, but a transport failure propagates so the caller can omit
/// `patch_path` rather than reference a missing file.
async fn generate_result_patch(
    target: GuestTarget<'_>,
    key_path: &Path,
    connect_timeout: Duration,
) -> Result<()> {
    let remote = "mkdir -p /workspace/out && cd /workspace/repo && git add -A 2>/dev/null; \
         git diff --cached refs/rooms/base > /workspace/out/result.patch 2>/dev/null || true";
    run_setup_ssh(
        target,
        key_path,
        remote,
        "generate result.patch",
        connect_timeout,
    )
    .await
}

/// Commit the agent's changes and push them to `branch` on the repo's remote.
///
/// `Ok(true)` when a commit was pushed, `Ok(false)` when the agent produced no
/// changes (nothing to push), `Err` on a real git/transport failure. Auth uses a
/// git credential helper that reads `GH_TOKEN` from the guest env (forwarded via
/// SSH `SendEnv`), so the token is never placed in argv.
async fn push_branch_in_guest(
    target: GuestTarget<'_>,
    key_path: &Path,
    repo_url: &str,
    branch: &str,
    connect_timeout: Duration,
) -> Result<bool> {
    let url = shell_single_quote(repo_url);
    let branch_q = shell_single_quote(branch);
    // Commit any working-tree changes the agent left, then push iff HEAD has
    // moved past the pinned base (`refs/rooms/base`, set at clone). Comparing the
    // pinned ref — not a re-resolved base_sha — covers BOTH a working-tree edit
    // committed just now AND the cursor SDK committing internally (clean tree,
    // HEAD already ahead), and stays correct when base_sha was symbolic. `exit 3`
    // marks "nothing to push"; `set -e` maps real git failures to Err. The
    // credential helper echoes `$GH_TOKEN` at git-invoke time; the single quotes
    // keep the guest shell from expanding it into argv.
    let remote = format!(
        "set -e; cd /workspace/repo; git checkout -B {branch_q}; git add -A; \
         if ! git diff --cached --quiet; then \
             git -c user.email=cursor@rooms.local -c user.name='rooms cursor agent' \
             commit -q -m 'rooms cursor agent run' -m 'Co-authored-by: Cursor <cursoragent@cursor.com>'; \
         fi; \
         if [ \"$(git rev-parse HEAD)\" = \"$(git rev-parse refs/rooms/base)\" ]; then exit 3; fi; \
         git -c credential.helper='!f(){{ echo username=x-access-token; echo \"password=$GH_TOKEN\"; }}; f' \
             push {url} HEAD:{branch_q}"
    );
    let output = ssh_command(target, key_path, &remote, true, connect_timeout)?
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await
        .context("failed to spawn ssh for push")?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(3) => Ok(false),
        _ => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("git push failed (exit {}): {stderr}", output.status)
        }
    }
}

/// Ensure the guest artifact dir + empty log files exist.
///
/// `exec_in_guest` creates these as a side effect of running the wrapped
/// command, but on a Ctrl-C that fires before exec actually started they're
/// missing — and `RunnerArtifacts::load` then bails with `MissingRequired`
/// even though `result.json` has been written. Touching empty placeholders
/// keeps the contract intact for cancelled runs.
pub async fn ensure_guest_artifact_skeleton(
    target: GuestTarget<'_>,
    key_path: &Path,
) -> Result<()> {
    let remote = "mkdir -p /workspace/out/logs \
         && : > /workspace/out/logs/stdout.log \
         && : > /workspace/out/logs/stderr.log";
    run_setup_ssh(
        target,
        key_path,
        remote,
        "create cancelled-run artifact skeleton",
        SSH_AUXILIARY_CONNECT_TIMEOUT,
    )
    .await
}

/// Write `result.json` into the guest artifact directory.
pub async fn write_guest_result_json(
    target: GuestTarget<'_>,
    key_path: &Path,
    result: &ResultJson,
) -> Result<()> {
    write_guest_result_json_with_timeout(target, key_path, result, SSH_AUXILIARY_CONNECT_TIMEOUT)
        .await
}

async fn write_guest_result_json_with_timeout(
    target: GuestTarget<'_>,
    key_path: &Path,
    result: &ResultJson,
    connect_timeout: Duration,
) -> Result<()> {
    let json = serde_json::to_string_pretty(result).context("serialize result.json")?;
    write_guest_file(
        target,
        key_path,
        "/workspace/out",
        "result.json",
        json.as_bytes(),
        connect_timeout,
    )
    .await
}

/// Pipe `contents` into `{dir}/{name}` in the guest via SSH stdin, creating
/// `dir` first.
///
/// `dir` and `name` are fixed substrate constants (never operator input), so no
/// shell-quoting is required for them.
async fn write_guest_file(
    target: GuestTarget<'_>,
    key_path: &Path,
    dir: &str,
    name: &str,
    contents: &[u8],
    connect_timeout: Duration,
) -> Result<()> {
    let remote = format!("mkdir -p {dir} && cat > {dir}/{name}");
    let mut child = ssh_command(target, key_path, &remote, false, connect_timeout)?
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("spawn ssh to write {name}"))?;

    let mut stdin = child
        .stdin
        .take()
        .with_context(|| format!("{name} ssh has no stdin (unexpected)"))?;
    stdin
        .write_all(contents)
        .await
        .with_context(|| format!("write {name} to ssh stdin"))?;
    stdin
        .shutdown()
        .await
        .with_context(|| format!("close ssh stdin after {name}"))?;
    drop(stdin);

    let output = child
        .wait_with_output()
        .await
        .with_context(|| format!("wait on {name} ssh"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "write {name} via SSH failed (exit {}): {stderr}",
            output.status
        );
    }
    Ok(())
}

/// Run a setup command in the guest that must succeed (clone, mkdir, etc.).
///
/// Unlike [`run_wrapped`], there is no `EXIT=` marker contract: failure is a
/// hard error surfaced to the caller, with the guest stderr attached.
async fn run_setup_ssh(
    target: GuestTarget<'_>,
    key_path: &Path,
    remote: &str,
    what: &str,
    connect_timeout: Duration,
) -> Result<()> {
    let output = ssh_command(target, key_path, remote, false, connect_timeout)?
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await
        .with_context(|| format!("failed to spawn ssh for {what}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("{what} failed (exit {}): {stderr}", output.status);
    }
    Ok(())
}

fn guest_command_argv(command: &str) -> Vec<String> {
    vec!["sh".to_owned(), "-c".to_owned(), command.to_owned()]
}

fn cursor_command_argv() -> Vec<String> {
    vec!["node".to_owned(), CURSOR_RUNNER_JS.to_owned()]
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

/// Build a host process that reaches `program` through the guest's namespace
/// when one is assigned. The namespace is passed as one argv token: this never
/// invokes a shell on the host.
fn guest_process(target: GuestTarget<'_>, program: &str) -> Command {
    let Some(netns) = target.netns else {
        return Command::new(program);
    };
    let mut command = Command::new("ip");
    command.args(["netns", "exec", netns, program]);
    command
}

fn ping_command(target: GuestTarget<'_>) -> Command {
    let mut command = guest_process(target, "ping");
    command.args(["-c", "1", "-W", "1", target.guest_ip]);
    command
}

fn ssh_command(
    target: GuestTarget<'_>,
    key_path: &Path,
    remote: &str,
    forward_gh_token: bool,
    connect_timeout: Duration,
) -> Result<Command> {
    let key = key_path.to_str().context("key path not utf-8")?;
    let dest = format!("{GUEST_USER}@{}", target.guest_ip);
    let connect_timeout_option = ssh_connect_timeout_option(connect_timeout);
    let mut cmd = guest_process(target, "ssh");
    cmd.args([
        "-i",
        key,
        "-o",
        "BatchMode=yes",
        "-o",
        connect_timeout_option.as_str(),
        "-o",
        "StrictHostKeyChecking=no",
        "-o",
        "UserKnownHostsFile=/dev/null",
        "-o",
        "LogLevel=ERROR",
        "-o",
        "SendEnv=ANTHROPIC_API_KEY",
        "-o",
        "SendEnv=CLAUDE_CODE_OAUTH_TOKEN",
        "-o",
        "SendEnv=ANTHROPIC_AUTH_TOKEN",
        "-o",
        "SendEnv=CURSOR_API_KEY",
    ]);
    // GH_TOKEN is forwarded ONLY for the push step — never to the agent run or
    // arbitrary `--command` execs — so guest/agent code can't read the push token.
    if forward_gh_token {
        cmd.args(["-o", "SendEnv=GH_TOKEN"]);
    }
    cmd.args([dest.as_str(), "--", remote]);
    Ok(cmd)
}

/// OpenSSH accepts an integral-second `ConnectTimeout`. Round a configured
/// duration up so the generated client never expires earlier than the caller's
/// connection-establishment budget; zero still gets a finite one-second bound.
fn ssh_connect_timeout_option(timeout: Duration) -> String {
    let seconds = timeout
        .as_secs()
        .saturating_add(u64::from(timeout.subsec_nanos() != 0))
        .max(1);
    format!("ConnectTimeout={seconds}")
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

    use super::{
        cursor_command_argv, ping_command, repo_url_has_userinfo, shell_single_quote, ssh_command,
        ssh_connect_timeout_option, ssh_probe_command, tar_member_is_safe, validate_neutral_warm,
        wait_for_ssh, CursorMeta, GuestTarget, SSH_AUXILIARY_CONNECT_TIMEOUT,
    };
    use crate::config::RoomsConfig;
    use crate::error::FirecrackerError;

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

    #[test]
    fn neutral_warm_refuses_credential_sources() {
        assert!(validate_neutral_warm("cargo fetch --locked").is_ok());
        assert!(validate_neutral_warm("curl -H 'Authorization: bearer x' host").is_err());
        assert!(validate_neutral_warm("cp ~/.netrc /tmp/input").is_err());
        assert!(repo_url_has_userinfo("https://user:pass@example.com/repo"));
        assert!(!repo_url_has_userinfo("https://example.com/repo"));
    }

    #[test]
    fn ssh_command_scopes_gh_token_to_push() {
        let key = std::path::Path::new("/tmp/id_rooms");
        let collect = |forward| {
            ssh_command(
                GuestTarget::flat("10.0.0.1"),
                key,
                "true",
                forward,
                SSH_AUXILIARY_CONNECT_TIMEOUT,
            )
            .expect("build ssh command")
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
        };
        let without = collect(false);
        let with = collect(true);
        assert!(
            !without.iter().any(|a| a == "SendEnv=GH_TOKEN"),
            "GH_TOKEN must not be forwarded for non-push commands; got: {without:?}"
        );
        assert!(
            without.iter().any(|a| a == "SendEnv=CURSOR_API_KEY"),
            "agent keys should still be forwarded; got: {without:?}"
        );
        for var in [
            "SendEnv=CLAUDE_CODE_OAUTH_TOKEN",
            "SendEnv=ANTHROPIC_AUTH_TOKEN",
        ] {
            assert!(
                without.iter().any(|a| a == var),
                "OAuth credentials should be forwarded like the API key; missing {var} in: {without:?}"
            );
        }
        let tok = with
            .iter()
            .position(|a| a == "SendEnv=GH_TOKEN")
            .expect("GH_TOKEN forwarded for push");
        let dest = with
            .iter()
            .position(|a| a.contains("rooms@"))
            .expect("destination present");
        assert!(
            tok < dest,
            "SendEnv=GH_TOKEN must precede the destination; got: {with:?}"
        );
    }

    #[test]
    fn guest_commands_keep_flat_argv_and_add_direct_netns_prefix() {
        let key = std::path::Path::new("/tmp/id_rooms");
        let flat = ssh_command(
            GuestTarget::flat("10.0.0.1"),
            key,
            "true",
            false,
            SSH_AUXILIARY_CONNECT_TIMEOUT,
        )
        .expect("build flat ssh command");
        let namespaced = ssh_command(
            GuestTarget::new("10.0.0.1", Some("rooms-c3")),
            key,
            "true",
            false,
            SSH_AUXILIARY_CONNECT_TIMEOUT,
        )
        .expect("build namespaced ssh command");
        let (flat_program, flat_args) = command_parts(&flat);
        let (namespaced_program, namespaced_args) = command_parts(&namespaced);

        assert_eq!(flat_program, "ssh");
        assert_eq!(
            flat_args,
            [
                "-i",
                "/tmp/id_rooms",
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
                "-o",
                "SendEnv=CLAUDE_CODE_OAUTH_TOKEN",
                "-o",
                "SendEnv=ANTHROPIC_AUTH_TOKEN",
                "-o",
                "SendEnv=CURSOR_API_KEY",
                "rooms@10.0.0.1",
                "--",
                "true",
            ]
        );
        assert_eq!(namespaced_program, "ip");
        assert_eq!(
            namespaced_args,
            ["netns", "exec", "rooms-c3", "ssh"]
                .into_iter()
                .chain(flat_args.iter().map(String::as_str))
                .collect::<Vec<_>>()
        );

        let (flat_ping_program, flat_ping_args) =
            command_parts(&ping_command(GuestTarget::flat("10.0.0.1")));
        let (netns_ping_program, netns_ping_args) = command_parts(&ping_command(GuestTarget::new(
            "10.0.0.1",
            Some("rooms-c3"),
        )));
        assert_eq!(flat_ping_program, "ping");
        assert_eq!(flat_ping_args, ["-c", "1", "-W", "1", "10.0.0.1"]);
        assert_eq!(netns_ping_program, "ip");
        assert_eq!(
            netns_ping_args,
            ["netns", "exec", "rooms-c3", "ping", "-c", "1", "-W", "1", "10.0.0.1",]
        );
    }

    #[test]
    fn ssh_readiness_probe_allows_a_contended_full_handshake() {
        let command = ssh_probe_command(
            GuestTarget::new("10.0.0.1", Some("rooms-c3")),
            "/tmp/id_rooms",
        );
        let (program, args) = command_parts(&command);

        assert_eq!(program, "ip");
        assert_eq!(
            args,
            [
                "netns",
                "exec",
                "rooms-c3",
                "ssh",
                "-i",
                "/tmp/id_rooms",
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=10",
                "-o",
                "StrictHostKeyChecking=no",
                "-o",
                "UserKnownHostsFile=/dev/null",
                "-o",
                "LogLevel=ERROR",
                "rooms@10.0.0.1",
                "true",
            ]
        );
    }

    #[test]
    fn workload_ssh_uses_the_configured_overall_connect_budget() {
        let config = RoomsConfig::default();
        let command = ssh_command(
            GuestTarget::flat("10.0.0.1"),
            std::path::Path::new("/tmp/id_rooms"),
            "true",
            false,
            config.guest_reach_timeout,
        )
        .expect("build workload ssh command");
        let (_, args) = command_parts(&command);

        assert!(
            args.iter().any(|arg| arg == "ConnectTimeout=120"),
            "the post-readiness workload handshake must get the full configured budget: {args:?}"
        );
        assert!(
            !args.iter().any(|arg| arg == "ConnectTimeout=5"),
            "the old one-shot five-second banner deadline must not survive: {args:?}"
        );
        assert_eq!(
            ssh_connect_timeout_option(Duration::from_millis(1_001)),
            "ConnectTimeout=2",
            "fractional seconds must round up instead of shortening the budget"
        );
    }

    #[test]
    fn namespace_metacharacters_remain_one_argv_token() {
        let namespace = "rooms-c3; touch /tmp/escaped $(false)";
        let command = ssh_command(
            GuestTarget::new("10.0.0.1", Some(namespace)),
            std::path::Path::new("/tmp/id_rooms"),
            "true",
            false,
            SSH_AUXILIARY_CONNECT_TIMEOUT,
        )
        .expect("build namespaced ssh command");
        let (program, args) = command_parts(&command);

        assert_eq!(program, "ip");
        assert_eq!(args.get(2).map(String::as_str), Some(namespace));
        assert_eq!(
            args.iter().filter(|arg| arg.as_str() == namespace).count(),
            1
        );
        assert!(!args.iter().any(|arg| arg == "sh" || arg == "-c"));
    }

    fn command_parts(command: &tokio::process::Command) -> (String, Vec<String>) {
        let command = command.as_std();
        let program = command.get_program().to_string_lossy().into_owned();
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        (program, args)
    }

    #[test]
    fn tar_member_gate_rejects_links_devices_and_traversal() {
        assert!(tar_member_is_safe(
            "-rw-r--r-- 0/0 12 2026-06-03 04:00 ./result.json"
        ));
        assert!(tar_member_is_safe(
            "drwxr-xr-x 0/0 0 2026-06-03 04:00 ./logs/"
        ));
        // links + devices rejected by the type gate
        assert!(!tar_member_is_safe(
            "lrwxrwxrwx 0/0 0 2026-06-03 04:00 ./evil -> /etc/passwd"
        ));
        assert!(!tar_member_is_safe(
            "crw-rw-rw- 0/0 0 2026-06-03 04:00 ./dev/null"
        ));
        // regular files rejected when the path escapes host_dir
        assert!(!tar_member_is_safe(
            "-rw-r--r-- 0/0 9 2026-06-03 04:00 ../../etc/cron.d/x"
        ));
        assert!(!tar_member_is_safe(
            "-rw-r--r-- 0/0 9 2026-06-03 04:00 /etc/passwd"
        ));
        assert!(!tar_member_is_safe(""));
    }

    #[test]
    fn cursor_command_argv_points_at_baked_script() {
        assert_eq!(
            cursor_command_argv(),
            vec![
                "node".to_owned(),
                "/opt/rooms/cursor-runner/cursor-runner.js".to_owned()
            ]
        );
    }

    #[test]
    fn cursor_meta_serializes_base_sha_and_model_id() {
        let meta = CursorMeta {
            base_sha: "abc123".to_owned(),
            model_id: "composer-2.5".to_owned(),
        };
        let json = serde_json::to_string(&meta).unwrap();
        assert_eq!(json, r#"{"base_sha":"abc123","model_id":"composer-2.5"}"#);
    }

    #[tokio::test]
    async fn wait_for_ssh_times_out_when_no_sshd() {
        let key_path = std::path::Path::new("/nonexistent/key");
        let config = RoomsConfig {
            guest_reach_timeout: Duration::from_secs(2),
            guest_reach_poll_interval: Duration::from_secs(1),
            ..RoomsConfig::default()
        };
        let start = std::time::Instant::now();

        let err = wait_for_ssh(GuestTarget::flat("127.0.0.255"), key_path, &config)
            .await
            .expect_err("unreachable address should time out");

        assert!(
            matches!(err, FirecrackerError::GuestUnreachable { .. }),
            "unexpected error: {err}"
        );
        assert!(
            start.elapsed() >= config.guest_reach_timeout,
            "should wait at least the timeout duration"
        );
    }
}
