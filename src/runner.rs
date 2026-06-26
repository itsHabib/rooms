//! Run commands inside a booted microVM, via SSH.
//!
//! POC: shells out to the `ssh` client. A native russh/openssh-rs client is
//! a productionization concern.

use std::path::Path;
use std::process::Stdio;
use std::time::Instant;

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

/// Absolute path of the baked cursor runner script inside the guest image.
const CURSOR_RUNNER_JS: &str = "/opt/rooms/cursor-runner/cursor-runner.js";

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

/// Probe the guest's sshd until it accepts a pubkey connection, or the
/// configured timeout elapses.
///
/// Returns `FirecrackerError::GuestUnreachable` (preserving the last underlying
/// stderr) on timeout so callers can distinguish guest-unreachability from
/// host-side substrate failures. Takes `&RoomsConfig` so the probe inherits
/// `guest_reach_timeout` and `guest_reach_poll_interval` knobs without each
/// caller redefining them.
pub async fn wait_for_ssh(
    guest_ip: &str,
    key_path: &Path,
    config: &RoomsConfig,
) -> Result<(), FirecrackerError> {
    let key = key_path
        .to_str()
        .ok_or_else(|| FirecrackerError::Internal("key path not utf-8".to_owned()))?;
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
                &format!("{GUEST_USER}@{guest_ip}"),
                "true",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(|e| FirecrackerError::Internal(format!("ssh probe spawn failed: {e}")))?;

        if output.status.success() {
            info!(guest_ip, "sshd accepted pubkey connection");
            return Ok(());
        }

        last_err = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        debug!(guest_ip, stderr = %last_err, "sshd probe failed; retrying");
        sleep(poll).await;
    }
}

/// Outcome of a guest command exec, including artifact metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestExecOutcome {
    pub exit_code: i32,
    pub status: RunStatus,
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
}

/// Drive `runner` in the guest, writing `result.json` per the runner contract.
///
/// The substrate stays agent-agnostic: it dispatches to the literal-command
/// path or the cursor path, both of which route their guest command through the
/// same `EXIT=` wrapper so exit-code capture and `result.json` ownership are
/// identical.
pub async fn exec(guest_ip: &str, key_path: &Path, runner: &Runner) -> Result<GuestExecOutcome> {
    match runner {
        Runner::Command(command) => exec_in_guest(guest_ip, key_path, command).await,
        Runner::Cursor(request) => exec_cursor_in_guest(guest_ip, key_path, request).await,
    }
}

/// Tar-over-ssh: stream the guest's `/workspace/out` into `host_dir`. `GH_TOKEN` not forwarded.
pub async fn collect_out_to_host(guest_ip: &str, key_path: &Path, host_dir: &Path) -> Result<()> {
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
        guest_ip,
        key_path,
        "if [ -d /workspace/out ]; then tar cf - -C /workspace/out .; else exit 0; fi",
        false,
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
    guest_ip: &str,
    key_path: &Path,
    host_dir: &Path,
) -> Result<()> {
    let ssh_out = ssh_command(guest_ip, key_path, ENUMERATE_OVERLAY, false)?
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
/// Forwards `ANTHROPIC_API_KEY` and `CURSOR_API_KEY` from the host process env
/// via SSH's `SendEnv` option; the matching `AcceptEnv` lines live in the guest's
/// `/etc/ssh/sshd_config`. `GH_TOKEN` is not forwarded here — it is sent only on
/// the cursor push step (see `push_branch_in_guest`), so a `--command` run and
/// the agent never see the push token.
pub async fn exec_in_guest(
    guest_ip: &str,
    key_path: &Path,
    command: &str,
) -> Result<GuestExecOutcome> {
    let run = run_wrapped(guest_ip, key_path, command).await?;
    let status = ResultJson::status_from_exit_code(run.exit_code);
    let result = ResultJson::from_exec(
        run.exit_code,
        status,
        run.started_at,
        run.ended_at,
        guest_command_argv(command),
    );
    write_guest_result_json(guest_ip, key_path, &result).await?;

    Ok(GuestExecOutcome {
        exit_code: run.exit_code,
        status,
        started_at: run.started_at,
        ended_at: run.ended_at,
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
    guest_ip: &str,
    key_path: &Path,
    request: &CursorRequest,
) -> Result<GuestExecOutcome> {
    clone_repo_in_guest(
        guest_ip,
        key_path,
        &request.repo_url,
        &request.meta.base_sha,
    )
    .await?;
    stage_cursor_input(guest_ip, key_path, &request.task_md, &request.meta).await?;

    let run = run_wrapped(
        guest_ip,
        key_path,
        &format!("node {CURSOR_RUNNER_JS} < /dev/null"),
    )
    .await?;

    let patch_written = match generate_result_patch(guest_ip, key_path).await {
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
            match push_branch_in_guest(guest_ip, key_path, &request.repo_url, branch).await {
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
        guest_ip,
        key_path,
        "mkdir -p /workspace/out && touch /workspace/out/events.ndjson /workspace/out/summary.md",
        "ensure cursor output artifacts exist",
    )
    .await?;
    result.summary_path = Some("summary.md".to_owned());
    result.events_path = Some("events.ndjson".to_owned());
    if patch_written {
        result.patch_path = Some("result.patch".to_owned());
    }
    result.pushed_branch = pushed_branch;
    write_guest_result_json(guest_ip, key_path, &result).await?;

    // result.json is recorded; surface a push failure last, so a push error never
    // eats the run's artifact (the High finding).
    if let Some(err) = push_err {
        return Err(err);
    }

    Ok(GuestExecOutcome {
        exit_code: run.exit_code,
        status,
        started_at: run.started_at,
        ended_at: run.ended_at,
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
async fn run_wrapped(guest_ip: &str, key_path: &Path, inner: &str) -> Result<WrappedRun> {
    let started_at = Utc::now();
    let quoted_command = shell_single_quote(inner);
    let remote = format!(
        "mkdir -p /workspace/out/logs && \
         bash -c {quoted_command} > /workspace/out/logs/stdout.log 2> /workspace/out/logs/stderr.log; \
         echo EXIT=$?"
    );
    let output = ssh_command(guest_ip, key_path, &remote, false)?
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await
        .context("failed to spawn ssh; is openssh-client installed?")?;

    let ended_at = Utc::now();
    // Decide failure mode by looking for the EXIT= marker. If we see it, SSH ran
    // the wrapper to completion and the exit code is in `output.stdout` — even
    // if SSH itself returned non-zero (which it does when the inner command
    // exits non-zero and bash surfaces that). If we don't see it, the wrapper
    // never ran and this is genuine SSH-transport failure.
    let stdout_str = String::from_utf8_lossy(&output.stdout);
    if !stdout_str.lines().any(|l| l.starts_with("EXIT=")) {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "guest exec via SSH failed before wrapper completed (ssh exit {}): {stderr}",
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
    guest_ip: &str,
    key_path: &Path,
    repo_url: &str,
    base_sha: &str,
) -> Result<()> {
    let url = shell_single_quote(repo_url);
    let sha = shell_single_quote(base_sha);
    let remote = format!(
        "rm -rf /workspace/repo && git clone {url} /workspace/repo && \
         git -C /workspace/repo checkout {sha} && \
         git -C /workspace/repo update-ref refs/rooms/base HEAD"
    );
    run_setup_ssh(guest_ip, key_path, &remote, "clone repo in guest").await
}

/// Write `task.md` and `meta.json` into `/workspace/in/` for `cursor-runner.js`.
async fn stage_cursor_input(
    guest_ip: &str,
    key_path: &Path,
    task_md: &str,
    meta: &CursorMeta,
) -> Result<()> {
    write_guest_file(
        guest_ip,
        key_path,
        "/workspace/in",
        "task.md",
        task_md.as_bytes(),
    )
    .await?;
    let meta_json = serde_json::to_string_pretty(meta).context("serialize meta.json")?;
    write_guest_file(
        guest_ip,
        key_path,
        "/workspace/in",
        "meta.json",
        meta_json.as_bytes(),
    )
    .await
}

/// Generate `/workspace/out/result.patch` from `git diff` against the pinned
/// `refs/rooms/base`, capturing both committed and working-tree changes.
///
/// Best effort: a git error still leaves an (empty) patch file via the `>`
/// redirect, but a transport failure propagates so the caller can omit
/// `patch_path` rather than reference a missing file.
async fn generate_result_patch(guest_ip: &str, key_path: &Path) -> Result<()> {
    let remote = "mkdir -p /workspace/out && cd /workspace/repo && git add -A 2>/dev/null; \
         git diff --cached refs/rooms/base > /workspace/out/result.patch 2>/dev/null || true";
    run_setup_ssh(guest_ip, key_path, remote, "generate result.patch").await
}

/// Commit the agent's changes and push them to `branch` on the repo's remote.
///
/// `Ok(true)` when a commit was pushed, `Ok(false)` when the agent produced no
/// changes (nothing to push), `Err` on a real git/transport failure. Auth uses a
/// git credential helper that reads `GH_TOKEN` from the guest env (forwarded via
/// SSH `SendEnv`), so the token is never placed in argv.
async fn push_branch_in_guest(
    guest_ip: &str,
    key_path: &Path,
    repo_url: &str,
    branch: &str,
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
    let output = ssh_command(guest_ip, key_path, &remote, true)?
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
pub async fn ensure_guest_artifact_skeleton(guest_ip: &str, key_path: &Path) -> Result<()> {
    let remote = "mkdir -p /workspace/out/logs \
         && : > /workspace/out/logs/stdout.log \
         && : > /workspace/out/logs/stderr.log";
    run_setup_ssh(
        guest_ip,
        key_path,
        remote,
        "create cancelled-run artifact skeleton",
    )
    .await
}

/// Write `result.json` into the guest artifact directory.
pub async fn write_guest_result_json(
    guest_ip: &str,
    key_path: &Path,
    result: &ResultJson,
) -> Result<()> {
    let json = serde_json::to_string_pretty(result).context("serialize result.json")?;
    write_guest_file(
        guest_ip,
        key_path,
        "/workspace/out",
        "result.json",
        json.as_bytes(),
    )
    .await
}

/// Pipe `contents` into `{dir}/{name}` in the guest via SSH stdin, creating
/// `dir` first.
///
/// `dir` and `name` are fixed substrate constants (never operator input), so no
/// shell-quoting is required for them.
async fn write_guest_file(
    guest_ip: &str,
    key_path: &Path,
    dir: &str,
    name: &str,
    contents: &[u8],
) -> Result<()> {
    let remote = format!("mkdir -p {dir} && cat > {dir}/{name}");
    let mut child = ssh_command(guest_ip, key_path, &remote, false)?
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
async fn run_setup_ssh(guest_ip: &str, key_path: &Path, remote: &str, what: &str) -> Result<()> {
    let output = ssh_command(guest_ip, key_path, remote, false)?
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

fn ssh_command(
    guest_ip: &str,
    key_path: &Path,
    remote: &str,
    forward_gh_token: bool,
) -> Result<Command> {
    let key = key_path.to_str().context("key path not utf-8")?;
    let dest = format!("{GUEST_USER}@{guest_ip}");
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
        cursor_command_argv, shell_single_quote, ssh_command, tar_member_is_safe, wait_for_ssh,
        CursorMeta,
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
    fn ssh_command_scopes_gh_token_to_push() {
        let key = std::path::Path::new("/tmp/id_rooms");
        let collect = |forward| {
            ssh_command("10.0.0.1", key, "true", forward)
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

        let err = wait_for_ssh("127.0.0.255", key_path, &config)
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
