//! `rooms` — disposable Firecracker microVMs with specified deps.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand, ValueEnum};
use rooms::artifacts::{ResultJson, RunStatus};
use rooms::{
    artifacts,
    config::RoomsConfig,
    doctor,
    error::{RoomsError, SlotError},
    firecracker, registry, room, rootfs, runner, slot,
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
        /// Lower the pool ceiling for this invocation. The host cap (default 8)
        /// is the source of truth; this can only lower it, never raise it. Above
        /// 63 is rejected — the /24 pool carves into 64 /30s minus the reserved
        /// slot 0, so slot indices top out there. Also settable via
        /// `ROOMS_MAX_POOL`.
        #[arg(long = "max-pool", env = "ROOMS_MAX_POOL", value_parser = parse_max_pool)]
        max_pool: Option<u8>,
        /// Emit a machine-readable terminal record on stdout. On failure this
        /// carries the `error_kind` (e.g. `pool_full`) so a caller can branch on
        /// the field without matching an exit code or a message string.
        #[arg(long)]
        json: bool,
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
    /// List rooms and their liveness (running | kept | orphaned-dead | unknown).
    Ls {
        /// Emit structured JSON output (schema'd; logs stay on stderr).
        #[arg(long)]
        json: bool,
    },
    /// Reap orphaned (dead-but-leaked) rooms. Never touches a live or kept room.
    Gc {
        /// Preview what would be reaped; remove nothing.
        #[arg(long = "dry-run")]
        dry_run: bool,
        /// Reap only this room id (still only if it's orphaned-dead).
        id: Option<String>,
    },
    /// Terminate a live room: signal its firecracker, then reap. A dead or
    /// unknown-liveness id is a safe no-op (an already-dead room is `gc`'s job).
    Kill {
        /// The room id to kill.
        id: String,
        /// Emit structured JSON output (schema'd; logs stay on stderr).
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
    max_pool: Option<u8>,
    json: bool,
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

/// Parse `--max-pool` / `ROOMS_MAX_POOL`: a positive integer no greater than the
/// addressing ceiling. The two ceilings are distinct axes — this is the
/// *addressing* one: the pool is a /24 carved into 64 /30s minus the reserved
/// slot 0, so slot indices top out at [`slot::MAX_SLOT`]. The host *resource*
/// cap is separate and can only be lower. Rejecting above the addressing ceiling
/// at parse time keeps a claim from ever being asked to walk off the /24.
fn parse_max_pool(s: &str) -> Result<u8, String> {
    let n: u8 = s.trim().parse().map_err(|_| {
        format!(
            "invalid --max-pool '{s}': want an integer 1..={}",
            slot::MAX_SLOT
        )
    })?;
    if n == 0 {
        return Err("--max-pool must be greater than zero".to_owned());
    }
    if n > slot::MAX_SLOT {
        return Err(format!(
            "--max-pool {n} exceeds the addressing ceiling {}: the pool is a /24 carved into /30s \
             (64 slots minus the reserved slot 0), so slot indices top out at {}",
            slot::MAX_SLOT,
            slot::MAX_SLOT
        ));
    }
    Ok(n)
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
            ExitCode::from(exit_code_for_error(&err))
        }
    }
}

/// The process exit code for a terminal [`RoomsError`]. `PoolFull` gets its own
/// reserved code so a caller can tell "pool at capacity, retry later" apart from
/// a generic failure without parsing stderr. The exit-code contract: `0` ok,
/// `2` generic error, `3` `diff`'s lane escape, **`4` pool full**; a guest's own
/// code passes through `0..=255` on the `Ok` path (so exit 4 alone is ambiguous
/// with a guest that exited 4 — the `--json` `error_kind` disambiguates).
const fn exit_code_for_error(err: &RoomsError) -> u8 {
    match err {
        RoomsError::Slot(SlotError::PoolFull { .. }) => 4,
        _ => 2,
    }
}

/// The coarse `error_kind` for the `rooms run --json` terminal record. Ship's
/// runner keys on `pool_full` — the one terminal error it must distinguish
/// (fail-fast backpressure, not a real failure) — without matching a string or
/// an exit code a guest could collide with.
const fn error_kind(err: &RoomsError) -> &'static str {
    match err {
        RoomsError::Slot(SlotError::PoolFull { .. }) => "pool_full",
        RoomsError::Slot(_) => "slot",
        RoomsError::Firecracker(_) => "firecracker",
        RoomsError::Rootfs(_) => "rootfs",
        RoomsError::Transport(_) => "transport",
        RoomsError::Runner(_) => "runner",
        RoomsError::Registry(_) => "registry",
        RoomsError::Internal(_) => "internal",
    }
}

/// The `rooms run --json` terminal record: the machine-readable outcome ship
/// keys on. `cap` rides along only for `pool_full`, where it's the pool size
/// actually walked.
#[derive(serde::Serialize)]
struct RunErrorRecord {
    error_kind: &'static str,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    cap: Option<u8>,
}

/// Build the terminal record for a failed `rooms run --json`.
fn run_error_record(err: &RoomsError) -> RunErrorRecord {
    let cap = match err {
        RoomsError::Slot(SlotError::PoolFull { cap }) => Some(*cap),
        _ => None,
    };
    RunErrorRecord {
        error_kind: error_kind(err),
        message: err.to_string(),
        cap,
    }
}

/// Print the `--json` terminal record for a failed run to stdout — the same
/// machine-readable stdout contract `doctor`/`ls`/`diff --json` follow (logs
/// stay on stderr). A serialize failure is logged, never fatal: the exit code
/// still carries the outcome.
fn emit_run_error_json(err: &RoomsError) {
    match serde_json::to_string(&run_error_record(err)) {
        Ok(line) => {
            #[allow(
                clippy::print_stdout,
                reason = "run --json terminal record; stdout is the documented contract"
            )]
            {
                println!("{line}");
            }
        }
        Err(e) => warn!(error = %e, "failed to serialize run --json terminal record"),
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
            max_pool,
            json,
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
                    max_pool,
                    json,
                },
                &config,
            )
            .await
        }
        Command::Collect { from } => collect_artifacts(from).await,
        Command::Doctor { image, json } => run_doctor_cmd(image.as_deref(), json, &config),
        Command::Diff { from, json } => diff_changeset(&from, json).await,
        Command::Ls { json } => list_rooms_cmd(json, &config),
        Command::Gc { dry_run, id } => gc_cmd(dry_run, id, &config),
        Command::Kill { id, json } => kill_cmd(&id, json, &config),
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
            let status = if !check.ok {
                "FAIL"
            } else if check.is_warning() {
                "WARN"
            } else {
                "ok"
            };
            eprintln!("[{status}] {}: {}", check.name, check.message);
        }
    }

    Ok(u8::from(!report.all_ok()))
}

/// Boot a room, optionally emitting the `--json` terminal error record. The
/// exit-code side of the `PoolFull` contract lives in `main`; this owns the
/// stdout data surface (like every other `--json` command), so the two
/// concerns — process code and machine-readable record — stay separated.
async fn run_room(args: RunArgs, config: &RoomsConfig) -> Result<u8, RoomsError> {
    let json = args.json;
    let result = run_room_inner(args, config).await;
    if json {
        if let Err(err) = &result {
            emit_run_error_json(err);
        }
    }
    result
}

async fn run_room_inner(args: RunArgs, config: &RoomsConfig) -> Result<u8, RoomsError> {
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

    // Degraded-mode precheck: the host firewall chain must be installed before
    // we claim a slot, so a mis-provisioned host fails fast with the exact
    // remediation rather than booting a room that can't be isolated. Fail-open
    // on an unprobeable host (no iptables / not root) — the boot's own root/KVM
    // checks own those errors.
    if let Err(remediation) = doctor::ensure_rooms_fwd_installed() {
        return Err(RoomsError::Internal(remediation));
    }

    // Every room path derives from the state base; resolve it once up front.
    let state_base = config.resolved_state_base().ok_or_else(|| {
        RoomsError::Internal("HOME unset; cannot locate the rooms state base".to_owned())
    })?;
    // Resolve every host-side input that can fail BEFORE claiming a slot: a
    // missing --task file, a bad key path, or other input error must fail fast
    // without leaking a claim — repeated, that would exhaust the pool until a
    // `rooms gc`. After the claim, only boot can fail, and its guard frees the
    // slot.
    let key = key_path()?;
    let action = resolve_action(&args).await?;
    // Read-only rootfs + tmpfs overlay on the cursor agent path (it runs
    // untrusted code) or when the operator opts in with --readonly-rootfs; a
    // plain `rooms run --command` otherwise keeps a writable rootfs so any
    // image — including ones without /sbin/overlay-init — still boots.
    let readonly_rootfs = args.readonly_rootfs || matches!(args.runner, RunnerKind::Cursor);

    // Mint the room id BEFORE the claim, so the one value is the slot-file
    // contents, the room dir name, and room.json.id. Then claim a pool slot and
    // derive the guest network from it.
    let room_id = firecracker::mint_room_id();
    let me = slot::Claimer::current().ok_or_else(|| {
        RoomsError::Internal("cannot read this process's identity for the slot claim".to_owned())
    })?;
    // The cap is a host fact; --max-pool / ROOMS_MAX_POOL can only lower it.
    let cap = config.effective_max_pool(args.max_pool);
    let claimed = slot::claim(&state_base, &room_id, me, cap, None)?;
    let network = firecracker::NetworkConfig {
        tap_name: claimed.tap.clone(),
        guest_ip: claimed.guest.to_string(),
        gateway_ip: claimed.gateway.to_string(),
        prefix: claimed.prefix,
    };
    let descriptor = room::RoomDescriptor {
        command: Some(room_label(&action)),
        keep: args.keep,
    };
    let boot_req = firecracker::BootRequest {
        kernel: &kernel,
        rootfs: &args.image,
        network: Some(&network),
        slot: Some(&claimed),
        room_id: &room_id,
        readonly_rootfs,
        descriptor: &descriptor,
    };
    let mut vm = match firecracker::boot(&boot_req, config).await {
        Ok(vm) => vm,
        Err(e) => {
            // boot's guard frees the slot when it got far enough to take
            // ownership; this covers an early failure before that. Compare-and-
            // delete makes the double-free safe (AlreadyFree / AlreadyReassigned).
            let _ = slot::free(&state_base, claimed.index, &room_id);
            return Err(e.into());
        }
    };

    if args.keep {
        vm.guard_mut().set_suppress_cleanup(true);
    }

    let outcome = post_boot(&network, &key, &action, &mut vm, config, args.max_wall).await;
    if let Some(out_dir) = args.out_dir.as_deref() {
        // Bound collection too: its SSH is only `ConnectTimeout`-bounded, and a
        // wall cap fires precisely when the guest may be unresponsive, so an
        // unbounded collect could hang teardown — force it past the grace.
        let collect = collect_if_exec(&network.guest_ip, &key, &action, out_dir);
        if tokio::time::timeout(PRE_TEARDOWN_GRACE, collect)
            .await
            .is_err()
        {
            warn!("artifact collection timed out (guest unresponsive); proceeding to teardown");
        }
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
                // Bias toward completed work: if the exec finished the same instant
                // the cap fired, honor its real result instead of a spurious 124.
                biased;
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
            if max_wall.is_some() {
                warn!("--max-wall set but no exec to bound (idle run); the cap has no effect");
            }
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

/// Upper bound on best-effort guest I/O (recording an aborted run, collecting
/// `--out`) before teardown is forced. The guest may be the unresponsive one a
/// wall cap just fired on — it can accept TCP yet never service the request, and
/// SSH `ConnectTimeout` bounds only the connect — so `vm.shutdown()` must never
/// wait on it indefinitely.
const PRE_TEARDOWN_GRACE: Duration = Duration::from_secs(15);

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
    if tokio::time::timeout(PRE_TEARDOWN_GRACE, write)
        .await
        .is_err()
    {
        warn!(
            grace = ?PRE_TEARDOWN_GRACE,
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

/// A short human label for `room.json`, derived from the resolved post-boot
/// action — what `rooms ls` shows under COMMAND.
fn room_label(action: &Action) -> String {
    match action {
        Action::Keep => "(keep)".to_owned(),
        Action::Idle => "(idle)".to_owned(),
        Action::Exec(runner::Runner::Command(cmd)) => cmd.clone(),
        Action::Exec(runner::Runner::Cursor(req)) => format!("cursor:{}", req.repo_url),
    }
}

fn list_rooms_cmd(json: bool, config: &RoomsConfig) -> Result<u8, RoomsError> {
    info!("rooms ls");
    let rooms = registry::list_rooms(config)?;
    if json {
        return render_rooms_json(&rooms);
    }
    render_rooms_human(&rooms);
    Ok(0)
}

// stdout is the documented data surface for `rooms ls --json` (logs stay on
// stderr), mirroring `rooms doctor --json` / `rooms diff --json`.
#[allow(
    clippy::print_stdout,
    reason = "machine-readable ls output; stdout is the documented contract"
)]
fn render_rooms_json(rooms: &[registry::RoomEntry]) -> Result<u8, RoomsError> {
    let report = registry::ListReport::new(rooms.to_vec());
    let out =
        serde_json::to_string_pretty(&report).map_err(|e| RoomsError::Internal(e.to_string()))?;
    println!("{out}");
    Ok(0)
}

// `rooms ls` is a data surface like `docker ps`; the table is its stdout
// contract (the empty-state note goes to stderr so a piped `ls` stays clean).
#[allow(
    clippy::print_stdout,
    reason = "ls table is the documented stdout contract"
)]
fn render_rooms_human(rooms: &[registry::RoomEntry]) {
    if rooms.is_empty() {
        eprintln!("no rooms");
        return;
    }
    println!(
        "{:<26}  {:<13}  {:<7}  {:<8}  {:<4}  COMMAND",
        "ID", "STATE", "PID", "AGE", "SLOT"
    );
    for r in rooms {
        let pid = r.pid.map_or_else(|| "-".to_owned(), |p| p.to_string());
        let slot = r
            .slot
            .as_ref()
            .map_or_else(|| "-".to_owned(), |s| s.index.to_string());
        println!(
            "{:<26}  {:<13}  {:<7}  {:<8}  {:<4}  {}",
            r.id,
            r.state.label(),
            pid,
            format_age(r.started_at),
            slot,
            truncate_label(r.label.as_deref(), 48),
        );
    }
}

/// Humanized age from a room's start time; `?` when unknown (no metadata).
fn format_age(started_at: Option<DateTime<Utc>>) -> String {
    let Some(start) = started_at else {
        return "?".to_owned();
    };
    humanize_secs((Utc::now() - start).num_seconds().max(0))
}

/// Render a non-negative second count compactly (`45s`, `2m3s`, `1h4m`, `3d2h`).
fn humanize_secs(secs: i64) -> String {
    if secs < 60 {
        return format!("{secs}s");
    }
    if secs < 3600 {
        return format!("{}m{}s", secs / 60, secs % 60);
    }
    if secs < 86_400 {
        return format!("{}h{}m", secs / 3600, (secs % 3600) / 60);
    }
    format!("{}d{}h", secs / 86_400, (secs % 86_400) / 3600)
}

/// Truncate a label to `max` display chars, marking elision with a single `…`.
fn truncate_label(label: Option<&str>, max: usize) -> String {
    let Some(s) = label else {
        return "-".to_owned();
    };
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let head: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{head}…")
}

fn gc_cmd(dry_run: bool, id: Option<String>, config: &RoomsConfig) -> Result<u8, RoomsError> {
    info!(dry_run, ?id, "rooms gc");
    let only = id.clone();
    let report = registry::gc(config, &registry::GcOptions { dry_run, only: id })?;
    render_gc(&report, only.as_deref());
    // Exit non-zero when a real reap failed (a reapable room left un-reaped), so
    // a scripted caller sees the partial failure — mirroring `doctor`/`diff`'s
    // exit-code-as-contract. A dry-run never reaps, so it's always exit 0.
    let reap_failed = !report.dry_run
        && report
            .outcomes
            .iter()
            .any(|o| o.state.is_reapable() && !o.reaped);
    Ok(u8::from(reap_failed))
}

fn render_gc(report: &registry::GcReport, only: Option<&str>) {
    if report.outcomes.is_empty() {
        render_gc_empty(only);
        return;
    }
    for o in &report.outcomes {
        eprintln!("{}  {}", o.id, o.reason);
    }
    if report.dry_run {
        let n = report
            .outcomes
            .iter()
            .filter(|o| o.state.is_reapable())
            .count();
        eprintln!("dry-run: {n} room(s) would be reaped");
        return;
    }
    let n = report.outcomes.iter().filter(|o| o.reaped).count();
    eprintln!("reaped {n} room(s)");
}

fn render_gc_empty(only: Option<&str>) {
    let Some(id) = only else {
        eprintln!("no rooms to reap");
        return;
    };
    eprintln!("no room with id {id} (already gone?)");
}

fn kill_cmd(id: &str, json: bool, config: &RoomsConfig) -> Result<u8, RoomsError> {
    info!(%id, "rooms kill");
    let report = registry::kill(config, id)?;
    // Exit code is the script-composition contract (like gc/diff): 0 killed or
    // already-dead no-op, 1 the kill couldn't complete (survived / reap leak), 2
    // refused (indeterminate liveness).
    let code = report.exit_code();
    if json {
        render_kill_json(&report)?;
        return Ok(code);
    }
    render_kill_human(&report, id);
    Ok(code)
}

// stdout is the documented data surface for `rooms kill --json` (logs stay on
// stderr), mirroring `rooms ls --json` / `rooms diff --json`.
#[allow(
    clippy::print_stdout,
    reason = "machine-readable kill output; stdout is the documented contract"
)]
fn render_kill_json(report: &registry::KillReport) -> Result<(), RoomsError> {
    let out =
        serde_json::to_string_pretty(report).map_err(|e| RoomsError::Internal(e.to_string()))?;
    println!("{out}");
    Ok(())
}

// `rooms kill` is an action like `gc`: the per-room outcome line goes to stderr;
// stdout is reserved for `--json`. Single-id kill yields 0 or 1 outcome.
fn render_kill_human(report: &registry::KillReport, id: &str) {
    let Some(outcome) = report.outcomes.first() else {
        eprintln!("no room with id {id} (already gone?)");
        return;
    };
    eprintln!("{}  {}", outcome.id, outcome.reason);
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
        changeset_exit_code, diff_changeset, exit_code_for_error, humanize_secs, parse_max_pool,
        parse_max_wall, resolve_action, run_error_record, truncate_label, Cli, Command, RoomsError,
        RunArgs, RunnerKind, SlotError,
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
    fn max_pool_and_json_flags_parse_onto_run() {
        let cli = Cli::try_parse_from([
            "rooms",
            "run",
            "--image",
            "x",
            "--command",
            "id",
            "--max-pool",
            "4",
            "--json",
        ])
        .expect("--max-pool + --json should parse");
        match cli.command {
            Command::Run { max_pool, json, .. } => {
                assert_eq!(max_pool, Some(4));
                assert!(json);
            }
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[test]
    fn parse_max_pool_accepts_the_valid_range() {
        assert_eq!(parse_max_pool("1"), Ok(1));
        assert_eq!(parse_max_pool("8"), Ok(8));
        assert_eq!(parse_max_pool("63"), Ok(super::slot::MAX_SLOT));
    }

    #[test]
    fn parse_max_pool_rejects_zero_and_junk() {
        for bad in ["0", "abc", "-1", "", "12x"] {
            assert!(parse_max_pool(bad).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn max_pool_above_the_addressing_ceiling_is_rejected_at_parse_time() {
        // 64 is one past the /24 carve; the message must name that origin so the
        // operator sees the addressing axis, not just a bare range error.
        let err = parse_max_pool("64").expect_err("64 must be rejected");
        assert!(
            err.contains("63") && (err.contains("/24") || err.contains("/30")),
            "expected a message naming the addressing ceiling; got: {err}"
        );
        // And it's rejected all the way through clap at parse time.
        assert!(
            Cli::try_parse_from([
                "rooms",
                "run",
                "--image",
                "x",
                "--command",
                "id",
                "--max-pool",
                "64"
            ])
            .is_err(),
            "--max-pool 64 must fail to parse"
        );
    }

    #[test]
    fn exit_code_reserves_four_for_pool_full() {
        assert_eq!(
            exit_code_for_error(&RoomsError::Slot(SlotError::PoolFull { cap: 8 })),
            4,
            "pool full gets its own reserved code"
        );
        // Every other error — including other slot errors — stays generic (2).
        assert_eq!(
            exit_code_for_error(&RoomsError::Slot(SlotError::TargetTaken { index: 5 })),
            2
        );
        assert_eq!(
            exit_code_for_error(&RoomsError::Internal("boom".to_owned())),
            2
        );
    }

    #[test]
    fn run_error_record_marks_pool_full_with_its_cap() {
        let record = run_error_record(&RoomsError::Slot(SlotError::PoolFull { cap: 8 }));
        assert_eq!(record.error_kind, "pool_full");
        assert_eq!(record.cap, Some(8));
        let json = serde_json::to_string(&record).expect("serialize");
        assert!(
            json.contains("\"error_kind\":\"pool_full\"") && json.contains("\"cap\":8"),
            "ship keys on error_kind, with the cap alongside; got: {json}"
        );
    }

    #[test]
    fn run_error_record_classifies_other_errors_and_omits_cap() {
        let record = run_error_record(&RoomsError::Internal("boom".to_owned()));
        assert_eq!(record.error_kind, "internal");
        assert_eq!(record.cap, None);
        let json = serde_json::to_string(&record).expect("serialize");
        assert!(
            !json.contains("cap"),
            "cap is omitted for a non-pool-full error; got: {json}"
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
            max_pool: None,
            json: false,
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

    #[test]
    fn ls_verb_parses_json_and_default() {
        let json = Cli::try_parse_from(["rooms", "ls", "--json"]).expect("ls --json parses");
        match json.command {
            Command::Ls { json } => assert!(json),
            other => panic!("expected Ls, got {other:?}"),
        }
        let human = Cli::try_parse_from(["rooms", "ls"]).expect("ls parses");
        match human.command {
            Command::Ls { json } => assert!(!json),
            other => panic!("expected Ls, got {other:?}"),
        }
    }

    #[test]
    fn gc_verb_parses_dry_run_and_id() {
        let id = "01abcdefghijklmnopqrstuvwx";
        let cli = Cli::try_parse_from(["rooms", "gc", "--dry-run", id])
            .expect("gc --dry-run <id> parses");
        match cli.command {
            Command::Gc { dry_run, id: got } => {
                assert!(dry_run);
                assert_eq!(got.as_deref(), Some(id));
            }
            other => panic!("expected Gc, got {other:?}"),
        }
        let bare = Cli::try_parse_from(["rooms", "gc"]).expect("bare gc parses");
        match bare.command {
            Command::Gc { dry_run, id } => {
                assert!(!dry_run);
                assert!(id.is_none());
            }
            other => panic!("expected Gc, got {other:?}"),
        }
    }

    #[test]
    fn kill_verb_parses_id_and_requires_it() {
        let id = "01abcdefghijklmnopqrstuvwx";
        let cli =
            Cli::try_parse_from(["rooms", "kill", id, "--json"]).expect("kill <id> --json parses");
        match cli.command {
            Command::Kill { id: got, json } => {
                assert_eq!(got, id);
                assert!(json);
            }
            other => panic!("expected Kill, got {other:?}"),
        }
        // id is required: bare `rooms kill` must fail to parse.
        assert!(
            Cli::try_parse_from(["rooms", "kill"]).is_err(),
            "kill requires an id"
        );
    }

    #[test]
    fn humanize_secs_formats_each_magnitude() {
        assert_eq!(humanize_secs(0), "0s");
        assert_eq!(humanize_secs(45), "45s");
        assert_eq!(humanize_secs(125), "2m5s");
        assert_eq!(humanize_secs(3_661), "1h1m");
        assert_eq!(humanize_secs(90_061), "1d1h");
    }

    #[test]
    fn truncate_label_elides_long_and_passes_short() {
        assert_eq!(truncate_label(None, 10), "-");
        assert_eq!(truncate_label(Some("short"), 10), "short");
        let long = truncate_label(Some("abcdefghijklmnop"), 5);
        assert_eq!(long.chars().count(), 5, "truncated to max display width");
        assert!(long.ends_with('…'), "elision marker present: {long}");
    }
}
