//! `rooms` — disposable Firecracker microVMs with specified deps.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand, ValueEnum};
use rooms::artifacts::{ResultJson, RunStatus};
use rooms::{
    artifacts, config::RoomsConfig, doctor, error::RoomsError, firecracker, rootfs, runner,
};
use tracing::{info, warn};

/// rooms — disposable Firecracker microVMs with specified deps.
#[derive(Parser, Debug)]
#[command(name = "rooms", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
#[allow(
    clippy::large_enum_variant,
    reason = "Run aggregates every `rooms run` clap flag so it dwarfs the other subcommands; the enum is parsed once on the stack, so boxing would only complicate the derive for no real gain"
)]
enum Command {
    /// Boot a microVM and optionally run a single command in it via SSH.
    Run {
        /// Path to the rootfs image (ext4).
        #[arg(long)]
        image: PathBuf,
        /// Keep the room alive until Ctrl-C instead of the default 3s auto-shutdown.
        /// Mutually exclusive with the exec paths. Suppresses cleanup for debugging.
        #[arg(long, conflicts_with_all = ["command", "task"])]
        keep: bool,
        /// Run a single command in the guest via SSH, capture its stdout/stderr on
        /// host stdout/stderr, propagate its exit code, then shut down.
        ///
        /// `conflicts_with task` also makes `--runner cursor --command` invalid:
        /// `cursor` requires `--task` (below), which `--command` excludes.
        #[arg(long, conflicts_with_all = ["keep", "task"], value_parser = non_empty_command)]
        command: Option<String>,
        /// Runner backend. `command` runs `--command` verbatim (default; the POC
        /// path); `cursor` clones `--repo` and drives the baked cursor-runner.js.
        #[arg(long, value_enum, default_value = "command")]
        runner: RunnerKind,
        /// Git URL cloned into `/workspace/repo` for `--runner cursor`.
        #[arg(long, required_if_eq("runner", "cursor"))]
        repo: Option<String>,
        /// Path to the task prompt (markdown) for `--runner cursor`.
        #[arg(long, required_if_eq("runner", "cursor"))]
        task: Option<PathBuf>,
        /// Model id for `--runner cursor` (e.g. "composer-2.5").
        #[arg(long, required_if_eq("runner", "cursor"))]
        model: Option<String>,
        /// Base git sha for `--runner cursor`; checked out before the run and
        /// used as the `result.patch` diff base.
        #[arg(long = "base-sha", required_if_eq("runner", "cursor"))]
        base_sha: Option<String>,
        /// Branch to push the agent's changes to (cursor only); requires `GH_TOKEN`
        /// in the env. Omit to leave the changes in the guest (no push).
        #[arg(long = "push-branch", conflicts_with_all = ["command", "keep"])]
        push_branch: Option<String>,
        /// Collect the guest's `/workspace/out` into this host directory (created
        /// and cleared each run) so `rooms collect --from <dir>` can read it.
        #[arg(long = "out", conflicts_with = "keep")]
        out_dir: Option<PathBuf>,
        /// Mount the rootfs read-only with a tmpfs overlay (needs an image
        /// carrying `/sbin/overlay-init`). Auto-enabled for `--runner cursor`;
        /// set it on a `--command` run to make the change set visible to
        /// `rooms diff`.
        #[arg(long = "readonly-rootfs")]
        readonly_rootfs: bool,
        /// Hard wall-clock cap on the run: when reached, the exec is aborted, a
        /// `timed_out` result.json is written, and the room is torn down. An
        /// integer with an optional `s`/`m`/`h` suffix (bare = seconds): `90s`,
        /// `30m`, `2h`. Omit for no cap (unbounded). Not valid with `--keep`.
        #[arg(long = "max-wall", value_parser = parse_max_wall, conflicts_with = "keep")]
        max_wall: Option<Duration>,
    },
    /// Validate runner artifacts in a local `out/` directory.
    Collect {
        /// Path to the collected `out/` directory on the host.
        #[arg(long)]
        from: PathBuf,
    },
    /// Check the host environment (KVM, Firecracker, image, etc.).
    Doctor {
        /// Path to the rootfs image for kernel/rootfs checks.
        #[arg(long)]
        image: Option<PathBuf>,
        /// Emit structured JSON output.
        #[arg(long)]
        json: bool,
    },
    /// Show the overlay change set from a run's collected `out/` directory.
    ///
    /// Exit codes: 0 = verified, no lane escape; 3 = a write escaped /workspace
    /// to a persistent path; 2 = indeterminate — couldn't verify the lane (no
    /// out-dir, an unreadable/foreign changeset, or a run with no overlay). A
    /// gate never reads "couldn't verify" as "clean".
    Diff {
        /// Path to the collected `out/` directory (the `--out` target of a run).
        #[arg(long)]
        from: PathBuf,
        /// Emit the raw `changeset.json` instead of a human summary.
        #[arg(long)]
        json: bool,
    },
}

/// Which runner backend drives the guest command.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lowercase")]
enum RunnerKind {
    /// Run `--command` verbatim (the POC path).
    Command,
    /// Drive the baked cursor-runner.js one-shot.
    Cursor,
}

/// Parsed `rooms run` inputs, bundled so the orchestration functions stay under
/// the argument-count cap.
struct RunArgs {
    image: PathBuf,
    keep: bool,
    command: Option<String>,
    runner: RunnerKind,
    repo: Option<String>,
    task: Option<PathBuf>,
    model: Option<String>,
    base_sha: Option<String>,
    push_branch: Option<String>,
    out_dir: Option<PathBuf>,
    readonly_rootfs: bool,
    max_wall: Option<Duration>,
}

/// What to do after the microVM boots: hold it open, exec a runner, or idle.
enum Action {
    Keep,
    Exec(runner::Runner),
    Idle,
}

fn non_empty_command(s: &str) -> Result<String, String> {
    if s.is_empty() {
        Err("--command must not be empty".to_owned())
    } else {
        Ok(s.to_owned())
    }
}

/// Parse a wall-clock cap: an integer with an optional `s`/`m`/`h` suffix
/// (bare = seconds), e.g. `90s`, `30m`, `2h`, `1800`. Zero is rejected.
fn parse_max_wall(s: &str) -> Result<Duration, String> {
    let trimmed = s.trim();
    let (digits, secs_per_unit) = split_duration_unit(trimmed);
    let value: u64 = digits.parse().map_err(|_| {
        format!("invalid --max-wall '{s}': want an integer with an optional s/m/h suffix")
    })?;
    if value == 0 {
        return Err("--max-wall must be greater than zero".to_owned());
    }
    Ok(Duration::from_secs(value.saturating_mul(secs_per_unit)))
}

/// Split a duration string into `(digits, seconds-per-unit)`; a bare number is
/// seconds.
fn split_duration_unit(s: &str) -> (&str, u64) {
    if let Some(digits) = s.strip_suffix('h') {
        return (digits, 3600);
    }
    if let Some(digits) = s.strip_suffix('m') {
        return (digits, 60);
    }
    if let Some(digits) = s.strip_suffix('s') {
        return (digits, 1);
    }
    (s, 1)
}

#[tokio::main]
async fn main() -> ExitCode {
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    match dispatch(cli).await {
        Ok(code) => ExitCode::from(code),
        Err(err) => {
            warn!(error = %err, "command failed");
            ExitCode::from(2)
        }
    }
}

async fn dispatch(cli: Cli) -> Result<u8, RoomsError> {
    let config = RoomsConfig::default();
    match cli.command {
        Command::Run {
            image,
            keep,
            command,
            runner,
            repo,
            task,
            model,
            base_sha,
            push_branch,
            out_dir,
            readonly_rootfs,
            max_wall,
        } => {
            run_room(
                RunArgs {
                    image,
                    keep,
                    command,
                    runner,
                    repo,
                    task,
                    model,
                    base_sha,
                    push_branch,
                    out_dir,
                    readonly_rootfs,
                    max_wall,
                },
                &config,
            )
            .await
        }
        Command::Collect { from } => collect_artifacts(from).await,
        Command::Doctor { image, json } => run_doctor_cmd(image.as_deref(), json, &config),
        Command::Diff { from, json } => diff_changeset(&from, json).await,
    }
}

fn run_doctor_cmd(
    image: Option<&Path>,
    json: bool,
    config: &RoomsConfig,
) -> Result<u8, RoomsError> {
    info!("rooms doctor");
    let report = doctor::run_doctor(config, image);

    if json {
        let out = serde_json::to_string_pretty(&report)
            .map_err(|e| RoomsError::Internal(e.to_string()))?;
        // JSON report goes to stdout so `rooms doctor --json > report.json`
        // captures clean machine-readable output; tracing logs continue to
        // flow on stderr.
        #[allow(
            clippy::print_stdout,
            reason = "machine-readable doctor output; stdout is the documented contract"
        )]
        {
            println!("{out}");
        }
    } else {
        for check in &report.checks {
            let status = if check.ok {
                if check.message.starts_with("warn:") {
                    "WARN"
                } else {
                    "ok"
                }
            } else {
                "FAIL"
            };
            eprintln!("[{status}] {}: {}", check.name, check.message);
        }
    }

    Ok(u8::from(!report.all_ok()))
}

async fn run_room(args: RunArgs, config: &RoomsConfig) -> Result<u8, RoomsError> {
    info!(image = ?args.image, keep = args.keep, runner = ?args.runner, "rooms run");

    let kernel = args
        .image
        .parent()
        .map(|p| p.join("vmlinux.bin"))
        .ok_or_else(|| {
            RoomsError::Internal(format!(
                "--image has no parent directory: {}",
                args.image.display()
            ))
        })?;
    rootfs::validate_kernel(&kernel).map_err(RoomsError::Rootfs)?;

    let network = firecracker::NetworkConfig {
        tap_name: "tap-fc0".to_owned(),
        guest_ip: "172.16.0.2".to_owned(),
        gateway_ip: "172.16.0.1".to_owned(),
        netmask: "255.255.255.0".to_owned(),
    };
    let key = key_path()?;
    // Resolve the post-boot action before booting so a missing --task file (or
    // other host-side input error) fails fast without spending a microVM boot.
    let action = resolve_action(&args).await?;
    // Read-only rootfs + tmpfs overlay on the cursor agent path (it runs
    // untrusted code) or when the operator opts in with --readonly-rootfs; a
    // plain `rooms run --command` otherwise keeps a writable rootfs so any
    // image — including ones without /sbin/overlay-init — still boots.
    let readonly_rootfs = args.readonly_rootfs || matches!(args.runner, RunnerKind::Cursor);
    let mut vm = firecracker::boot(
        &kernel,
        &args.image,
        Some(&network),
        config,
        readonly_rootfs,
    )
    .await?;

    if args.keep {
        vm.guard_mut().set_suppress_cleanup(true);
    }

    let outcome = post_boot(&network, &key, &action, &mut vm, config, args.max_wall).await;
    if let Some(out_dir) = args.out_dir.as_deref() {
        collect_if_exec(&network.guest_ip, &key, &action, out_dir).await;
    }
    if args.keep {
        vm.guard_mut().dismiss();
        // Prevent kill_on_drop from terminating the microVM — operator inspects manually.
        std::mem::forget(vm);
        info!("--keep: cleanup suppressed; firecracker process and state dir preserved");
    } else if let Err(e) = vm.shutdown().await {
        warn!(error = %e, "shutdown reported an error after post-boot");
    }
    outcome
}

/// Best-effort collect `/workspace/out` to the host after an exec (no-op for Idle/Keep); a failure is logged, never fatal.
async fn collect_if_exec(guest_ip: &str, key: &Path, action: &Action, out_dir: &Path) {
    // No-op for Action::Idle (--command/--runner omitted); Action::Keep is
    // already excluded by clap's --out/--keep conflict.
    if !matches!(action, Action::Exec(_)) {
        return;
    }
    match runner::collect_out_to_host(guest_ip, key, out_dir).await {
        Ok(()) => {
            info!(out = %out_dir.display(), "collected /workspace/out to host");
        }
        Err(e) => {
            warn!(error = %e, out = %out_dir.display(), "collect /workspace/out to host failed");
        }
    }
    // The overlay change set (cursor / --readonly-rootfs runs only). Best-effort:
    // an absent overlay or a read failure never affects the run's result.
    match runner::collect_changeset_to_host(guest_ip, key, out_dir).await {
        Ok(()) => info!(out = %out_dir.display(), "collected overlay changeset"),
        Err(e) => warn!(error = %e, "collect overlay changeset failed"),
    }
}

/// Translate parsed flags into the post-boot [`Action`], reading the `--task`
/// file for the cursor path. clap's `required_if_eq` guarantees the cursor
/// flags are present, so the `ok_or_else` arms are defensive.
async fn resolve_action(args: &RunArgs) -> Result<Action, RoomsError> {
    if args.keep {
        return Ok(Action::Keep);
    }
    match args.runner {
        RunnerKind::Cursor => {
            let task_path = args.task.as_ref().ok_or_else(|| {
                RoomsError::Internal("--task is required for --runner cursor".to_owned())
            })?;
            let task_md = tokio::fs::read_to_string(task_path).await.map_err(|e| {
                RoomsError::Internal(format!("read --task file {}: {e}", task_path.display()))
            })?;
            let repo_url = args.repo.clone().ok_or_else(|| {
                RoomsError::Internal("--repo is required for --runner cursor".to_owned())
            })?;
            let base_sha = args.base_sha.clone().ok_or_else(|| {
                RoomsError::Internal("--base-sha is required for --runner cursor".to_owned())
            })?;
            let model_id = args.model.clone().ok_or_else(|| {
                RoomsError::Internal("--model is required for --runner cursor".to_owned())
            })?;
            if args.push_branch.is_some() && std::env::var_os("GH_TOKEN").is_none() {
                return Err(RoomsError::Internal(
                    "--push-branch requires GH_TOKEN in the environment".to_owned(),
                ));
            }
            Ok(Action::Exec(runner::Runner::Cursor(
                runner::CursorRequest {
                    repo_url,
                    task_md,
                    meta: runner::CursorMeta { base_sha, model_id },
                    push_branch: args.push_branch.clone(),
                },
            )))
        }
        RunnerKind::Command => {
            if args.push_branch.is_some() {
                return Err(RoomsError::Internal(
                    "--push-branch is only valid with --runner cursor".to_owned(),
                ));
            }
            let action = args.command.clone().map_or(Action::Idle, |command| {
                Action::Exec(runner::Runner::Command(command))
            });
            Ok(action)
        }
    }
}

async fn post_boot(
    network: &firecracker::NetworkConfig,
    key: &Path,
    action: &Action,
    vm: &mut firecracker::BootedVm,
    config: &RoomsConfig,
    max_wall: Option<Duration>,
) -> Result<u8, RoomsError> {
    match action {
        Action::Keep => {
            info!(
                guest_ip = %network.guest_ip,
                "microVM is up; Ctrl-C to shut down (try `ping {}` from another shell)",
                network.guest_ip,
            );
            tokio::signal::ctrl_c()
                .await
                .map_err(|e| RoomsError::Internal(e.to_string()))?;
            Ok(0)
        }
        Action::Exec(run) => {
            let guest_ip = network.guest_ip.clone();
            // Wrap the entire setup-and-exec sequence (probe sshd, then exec) in
            // one tokio::select! vs ctrl_c. Dropping `work` cascades through each
            // child future — kill_on_drop fires on every spawned ssh client — so
            // a Ctrl-C at any point still lets run_room's vm.shutdown() run cleanly.
            let work = async {
                runner::wait_for_ssh(&network.guest_ip, key, config)
                    .await
                    .map_err(RoomsError::Firecracker)?;
                let outcome = runner::exec(&network.guest_ip, key, run)
                    .await
                    .map_err(|e| RoomsError::Internal(e.to_string()))?;
                Ok::<u8, RoomsError>(u8::try_from(outcome.exit_code).unwrap_or(2))
            };
            // started_at captures when rooms began attempting exec (SSH probe,
            // then runner). Exec writes its own started_at into result.json on
            // the success path; this outer one only surfaces in the abort
            // (cancel / timeout) branches below, where the guest command may
            // never have begun.
            let started_at = Utc::now();
            tokio::pin!(work);
            // Fires at the wall-clock cap, or never when there's no cap — so an
            // unset --max-wall leaves the select racing only work vs ctrl_c.
            let cap = async {
                match max_wall {
                    Some(limit) => tokio::time::sleep(limit).await,
                    None => std::future::pending::<()>().await,
                }
            };
            tokio::pin!(cap);
            tokio::select! {
                res = &mut work => res,
                _ = tokio::signal::ctrl_c() => {
                    info!("ctrl-c received during exec setup or run; aborting and shutting down");
                    record_aborted_run(&guest_ip, key, 130, RunStatus::Cancelled, started_at, run.command_argv()).await;
                    Ok(130)
                }
                () = &mut cap => {
                    warn!(?max_wall, "max wall-clock cap reached during exec; aborting and shutting down");
                    record_aborted_run(&guest_ip, key, 124, RunStatus::TimedOut, started_at, run.command_argv()).await;
                    Ok(124)
                }
            }
        }
        Action::Idle => {
            tokio::time::sleep(Duration::from_secs(3)).await;
            if vm.is_alive().map_err(RoomsError::Firecracker)? {
                info!("microVM is up; shutting down (POC: no exec yet)");
                Ok(0)
            } else {
                Err(RoomsError::Firecracker(
                    rooms::error::FirecrackerError::ProcessExitedEarly {
                        exit_code: -1,
                        stderr_tail: String::new(),
                    },
                ))
            }
        }
    }
}

/// How long to spend recording an aborted run before abandoning it so teardown
/// can proceed: the guest may be the unresponsive one a wall cap just fired on
/// (it can accept TCP yet never service the request — `ConnectTimeout` bounds
/// only the connect), and `vm.shutdown()` must never wait on a best-effort SSH
/// to it.
const ABORT_RECORD_GRACE: Duration = Duration::from_secs(10);

/// Record an aborted run (cancel / timeout): ensure the guest artifact skeleton
/// exists so `rooms collect` validation passes, then write a `result.json` with
/// the override status + exit code. Best-effort AND time-bounded — a stalled
/// guest can't block the caller's teardown; failures and the grace expiry are
/// logged, never fatal.
async fn record_aborted_run(
    guest_ip: &str,
    key: &Path,
    exit_code: i32,
    status: RunStatus,
    started_at: DateTime<Utc>,
    command_argv: Vec<String>,
) {
    let write = async move {
        if let Err(err) = runner::ensure_guest_artifact_skeleton(guest_ip, key).await {
            warn!(error = %err, "failed to create aborted-run artifact skeleton");
        }
        let result = ResultJson::from_exec(exit_code, status, started_at, Utc::now(), command_argv);
        if let Err(err) = runner::write_guest_result_json(guest_ip, key, &result).await {
            warn!(error = %err, "failed to write aborted-run result.json");
        }
    };
    if tokio::time::timeout(ABORT_RECORD_GRACE, write)
        .await
        .is_err()
    {
        warn!(
            grace = ?ABORT_RECORD_GRACE,
            "aborted-run record timed out (guest unresponsive); tearing down without it"
        );
    }
}

fn key_path() -> Result<PathBuf, RoomsError> {
    let home = std::env::var("HOME").map_err(|_| {
        RoomsError::Internal(
            "HOME env var unset; rooms needs it to locate ~/.ssh/id_rooms".to_owned(),
        )
    })?;
    Ok(PathBuf::from(home).join(".ssh/id_rooms"))
}

async fn collect_artifacts(from: PathBuf) -> Result<u8, RoomsError> {
    info!(from = %from.display(), "rooms collect");
    let loaded = artifacts::RunnerArtifacts::load(&from)
        .await
        .map_err(|e| RoomsError::Internal(e.to_string()))?;
    info!(
        status = ?loaded.result.status,
        exit_code = loaded.result.exit_code,
        "artifacts validated"
    );
    Ok(0)
}

async fn diff_changeset(from: &Path, json: bool) -> Result<u8, RoomsError> {
    info!(from = %from.display(), "rooms diff");
    // A gate must never read "couldn't verify" as "clean": a missing out-dir or
    // an absent changeset.json exits 2 (indeterminate), distinct from 0 (verified)
    // and 3 (lane escape). A best-effort collect failure leaves no changeset.json,
    // so without this an exit-code gate would pass an unverified run.
    if !matches!(tokio::fs::metadata(from).await, Ok(m) if m.is_dir()) {
        eprintln!("--from is not an existing directory: {}", from.display());
        return Ok(2);
    }
    let path = from.join(artifacts::CHANGESET_JSON);
    let raw = match tokio::fs::read_to_string(&path).await {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!(
                "no {} in {}: collect the run with --out, under --runner cursor or --readonly-rootfs",
                artifacts::CHANGESET_JSON,
                from.display()
            );
            return Ok(2);
        }
        Err(e) => {
            return Err(RoomsError::Internal(format!(
                "read {}: {e}",
                path.display()
            )))
        }
    };
    let changeset: artifacts::Changeset = serde_json::from_str(&raw)
        .map_err(|e| RoomsError::Internal(format!("parse changeset: {e}")))?;
    // A changeset written by a different schema version can't be read under v1
    // lane-escape semantics — treat it as indeterminate rather than risk a v2
    // escape reading as a v1 "clean" (mirrors result.json's version discipline).
    if changeset.schema_version != artifacts::CHANGESET_SCHEMA_VERSION {
        eprintln!(
            "changeset.json schema_version {} != supported {}: regenerate with this rooms build",
            changeset.schema_version,
            artifacts::CHANGESET_SCHEMA_VERSION
        );
        return Ok(2);
    }
    render_changeset(&changeset, &raw, json);
    Ok(changeset_exit_code(&changeset))
}

/// The gate's exit code: **3** when the run escaped its lane (wrote outside
/// `/workspace` to a persistent path), **2** when the overlay was inactive — a
/// writable-rootfs run has no lane to check, so the question is unanswerable and
/// a gate must not read it as clean — **0** when an active overlay shows no escape.
fn changeset_exit_code(changeset: &artifacts::Changeset) -> u8 {
    if !changeset.overlay_active {
        return 2;
    }
    if changeset.lane_escapes().is_empty() {
        return 0;
    }
    3
}

// stdout is the documented data surface for `rooms diff` (logs stay on stderr),
// mirroring `rooms doctor --json`.
#[allow(
    clippy::print_stdout,
    reason = "diff output is the documented stdout contract"
)]
fn render_changeset(changeset: &artifacts::Changeset, raw: &str, json: bool) {
    if json {
        println!("{raw}");
        return;
    }
    if !changeset.overlay_active {
        eprintln!(
            "no overlay change set: this run used a writable rootfs (only --runner cursor or --readonly-rootfs produce an overlay)"
        );
        return;
    }
    let escapes = changeset.lane_escapes();
    if !escapes.is_empty() {
        println!(
            "[!] {} lane escape(s) — writes outside /workspace to persistent paths:",
            escapes.len()
        );
        print_escapes_by_op(changeset);
    }
    let (added, modified, deleted) = changeset.workspace_counts();
    println!("/workspace: +{added} ~{modified} -{deleted}");
    let ephemeral = changeset.ephemeral_count();
    if ephemeral > 0 {
        println!("({ephemeral} runtime write(s) under /run, /var/log, ... filtered)");
    }
    // is_empty() is false whenever an escape fired (escapes live in the three
    // lists), so "(no changes)" can't print alongside a lane-escape block above.
    if changeset.is_empty() {
        println!("(no changes)");
    }
}

// Same stdout contract as `render_changeset`.
#[allow(
    clippy::print_stdout,
    reason = "diff output is the documented stdout contract"
)]
fn print_escapes_by_op(changeset: &artifacts::Changeset) {
    for path in changeset
        .added
        .iter()
        .filter(|path| artifacts::is_lane_escape(path.as_str()))
    {
        println!("    A /{path}");
    }
    for path in changeset
        .modified
        .iter()
        .filter(|path| artifacts::is_lane_escape(path.as_str()))
    {
        println!("    M /{path}");
    }
    for path in changeset
        .deleted
        .iter()
        .filter(|path| artifacts::is_lane_escape(path.as_str()))
    {
        println!("    D /{path}");
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        reason = "test module"
    )]

    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use super::{
        changeset_exit_code, diff_changeset, parse_max_wall, resolve_action, Cli, Command,
        RoomsError, RunArgs, RunnerKind,
    };
    use crate::artifacts::Changeset;
    use clap::{CommandFactory, Parser};
    use tempfile::tempdir;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn readonly_rootfs_flag_parses() {
        let cli = Cli::try_parse_from([
            "rooms",
            "run",
            "--image",
            "x",
            "--command",
            "id",
            "--readonly-rootfs",
        ])
        .expect("--readonly-rootfs should parse");
        match cli.command {
            Command::Run {
                readonly_rootfs, ..
            } => assert!(readonly_rootfs),
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[test]
    fn parse_max_wall_accepts_units_and_bare_seconds() {
        // Compare seconds (not constructed Durations) so the input -> N-seconds
        // intent stays explicit and clippy's duration-units lint stays quiet.
        let secs = |s: &str| parse_max_wall(s).map(|d| d.as_secs());
        assert_eq!(secs("90s"), Ok(90));
        assert_eq!(secs("30m"), Ok(1800));
        assert_eq!(secs("2h"), Ok(7200));
        assert_eq!(secs("1800"), Ok(1800));
    }

    #[test]
    fn parse_max_wall_rejects_zero_and_junk() {
        // uppercase suffixes are intentionally NOT accepted — the grammar is
        // lowercase s/m/h (matches --help + the spec).
        for bad in ["0", "0s", "abc", "10x", "m", "", "2H", "90S"] {
            assert!(parse_max_wall(bad).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn max_wall_flag_parses_onto_run() {
        let cli = Cli::try_parse_from([
            "rooms",
            "run",
            "--image",
            "x",
            "--command",
            "id",
            "--max-wall",
            "45s",
        ])
        .expect("--max-wall should parse");
        match cli.command {
            Command::Run { max_wall, .. } => assert_eq!(max_wall, Some(Duration::from_secs(45))),
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[test]
    fn max_wall_conflicts_with_keep() {
        let err = Cli::try_parse_from([
            "rooms",
            "run",
            "--image",
            "x",
            "--keep",
            "--max-wall",
            "30s",
        ])
        .expect_err("--max-wall + --keep should fail to parse");
        assert!(
            err.to_string().contains("cannot be used with") || err.to_string().contains("--keep"),
            "expected a conflict error naming --keep; got: {err}"
        );
    }

    #[test]
    fn diff_verb_parses_from_and_json() {
        let cli = Cli::try_parse_from(["rooms", "diff", "--from", "/tmp/out", "--json"])
            .expect("diff should parse");
        match cli.command {
            Command::Diff { from, json } => {
                assert_eq!(from, PathBuf::from("/tmp/out"));
                assert!(json);
            }
            other => panic!("expected Diff, got {other:?}"),
        }
    }

    #[test]
    fn changeset_exit_code_flags_lane_escape() {
        let mut cs = Changeset {
            schema_version: 1,
            overlay_active: true,
            added: vec!["workspace/a".to_owned()],
            modified: Vec::new(),
            deleted: Vec::new(),
        };
        assert_eq!(changeset_exit_code(&cs), 0);
        cs.added.push("etc/hosts".to_owned());
        assert_eq!(changeset_exit_code(&cs), 3);
        cs.overlay_active = false;
        // inactive overlay -> indeterminate (no lane to check), never a clean 0.
        assert_eq!(changeset_exit_code(&cs), 2);
    }

    #[tokio::test]
    async fn diff_from_missing_dir_is_indeterminate() {
        // A typo'd / non-existent --from must not read as exit 0 ("clean").
        let code = diff_changeset(Path::new("rooms-no-such-dir-xyz"), false)
            .await
            .expect("diff on a missing dir returns a code, not an error");
        assert_eq!(code, 2);
    }

    #[tokio::test]
    async fn diff_dir_without_changeset_is_indeterminate() {
        // out-dir exists but collection produced no changeset.json (e.g. a
        // best-effort collect failure) -> indeterminate, never a silent exit 0.
        let dir = tempdir().expect("tempdir");
        let code = diff_changeset(dir.path(), false)
            .await
            .expect("diff returns a code, not an error");
        assert_eq!(code, 2);
    }

    #[tokio::test]
    async fn diff_unreadable_changeset_is_not_clean() {
        // A changeset.json that can't be read (here: it is a directory) must not
        // read as a clean exit 0 -> diff_changeset errors, which main maps to
        // exit 2. Locks "couldn't read/parse" out of the verified-clean path.
        let dir = tempdir().expect("tempdir");
        tokio::fs::create_dir(dir.path().join(crate::artifacts::CHANGESET_JSON))
            .await
            .expect("create a directory in place of changeset.json");
        let result = diff_changeset(dir.path(), false).await;
        assert!(result.is_err(), "an unreadable changeset must not be Ok(0)");
    }

    #[tokio::test]
    async fn diff_unknown_schema_version_is_indeterminate() {
        // A foreign schema_version must not be read under v1 lane-escape
        // semantics: even with an escape-shaped path, it exits 2, not 3 or 0.
        let dir = tempdir().expect("tempdir");
        let raw = r#"{"schema_version":999,"overlay_active":true,"added":["etc/hosts"],"modified":[],"deleted":[]}"#;
        tokio::fs::write(dir.path().join(crate::artifacts::CHANGESET_JSON), raw)
            .await
            .expect("write changeset.json");
        let code = diff_changeset(dir.path(), false)
            .await
            .expect("diff returns a code, not an error");
        assert_eq!(code, 2);
    }

    #[test]
    fn keep_and_command_are_mutually_exclusive() {
        let err = Cli::try_parse_from([
            "rooms",
            "run",
            "--image",
            "x",
            "--keep",
            "--command",
            "echo hi",
        ])
        .expect_err("--keep + --command should fail to parse");
        assert!(
            err.to_string().contains("--keep") && err.to_string().contains("--command"),
            "expected error to name both flags; got: {err}"
        );
    }

    #[test]
    fn runner_defaults_to_command() {
        let cli = Cli::try_parse_from(["rooms", "run", "--image", "x", "--command", "echo hi"])
            .expect("default command runner should parse");
        match cli.command {
            Command::Run { runner, .. } => assert_eq!(runner, RunnerKind::Command),
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[test]
    fn cursor_requires_repo_task_model_base_sha() {
        let err = Cli::try_parse_from(["rooms", "run", "--image", "x", "--runner", "cursor"])
            .expect_err("--runner cursor without its required flags should fail");
        let msg = err.to_string();
        assert!(
            ["--repo", "--task", "--model", "--base-sha"]
                .iter()
                .any(|flag| msg.contains(flag)),
            "expected a required cursor flag in the error; got: {msg}"
        );
    }

    #[test]
    fn cursor_parses_with_all_required_flags() {
        let cli = Cli::try_parse_from([
            "rooms",
            "run",
            "--image",
            "x",
            "--runner",
            "cursor",
            "--repo",
            "https://example.test/r.git",
            "--task",
            "task.md",
            "--model",
            "composer-2.5",
            "--base-sha",
            "abc123",
        ])
        .expect("full cursor invocation should parse");
        match cli.command {
            Command::Run { runner, .. } => assert_eq!(runner, RunnerKind::Cursor),
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[test]
    fn keep_conflicts_with_cursor_task() {
        let err = Cli::try_parse_from(["rooms", "run", "--image", "x", "--keep", "--task", "t.md"])
            .expect_err("--keep + --task should conflict");
        assert!(
            err.to_string().contains("--keep"),
            "expected error to name --keep; got: {err}"
        );
    }

    #[test]
    fn push_branch_conflicts_with_command() {
        let err = Cli::try_parse_from([
            "rooms",
            "run",
            "--image",
            "x",
            "--command",
            "echo hi",
            "--push-branch",
            "feature",
        ])
        .expect_err("--push-branch + --command should fail to parse");
        assert!(
            err.to_string().contains("--push-branch") && err.to_string().contains("--command"),
            "expected error to name --push-branch and --command; got: {err}"
        );
    }

    #[tokio::test]
    async fn push_branch_without_cursor_runner_is_rejected() {
        // Covers the resolve-time guard (the clap conflict can't catch the bare
        // default-command case: `rooms run --image x --push-branch foo`).
        let args = RunArgs {
            image: PathBuf::from("x"),
            keep: false,
            command: None,
            runner: RunnerKind::Command,
            repo: None,
            task: None,
            model: None,
            base_sha: None,
            push_branch: Some("feature".to_owned()),
            out_dir: None,
            readonly_rootfs: false,
            max_wall: None,
        };
        match resolve_action(&args).await {
            Err(RoomsError::Internal(m)) => assert!(
                m.contains("--push-branch"),
                "expected the error to name --push-branch; got: {m}"
            ),
            Ok(_) => panic!("--push-branch with the default command runner should be rejected"),
            Err(other) => panic!("expected an Internal error; got: {other:?}"),
        }
    }

    #[test]
    fn out_conflicts_with_keep() {
        let err =
            Cli::try_parse_from(["rooms", "run", "--image", "x", "--keep", "--out", "/tmp/o"])
                .expect_err("--keep + --out should conflict");
        assert!(
            err.to_string().contains("--out") && err.to_string().contains("--keep"),
            "expected error to name --out and --keep; got: {err}"
        );
    }
}
