//! `rooms` — disposable Firecracker microVMs with specified deps.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand, ValueEnum};
use rooms::artifacts::{ResultJson, RunStatus};
use rooms::lifecycle::{Event, Lifecycle, WorkloadStatus};
use rooms::{
    artifacts,
    config::RoomsConfig,
    doctor,
    error::{RoomsError, SlotError},
    firecracker, registry, room, rootfs, runner, slot, vsock, witness,
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
        /// Append a machine-readable lifecycle stream to this host path:
        /// NDJSON, one event per line, monotonic seq from 1. The stream
        /// distinguishes slot allocation (and structured `pool_full`), VMM
        /// start, guest readiness vs SSH readiness, workload start/exit,
        /// collection, and cleanup, so a supervising process can track the run
        /// without parsing logs.
        #[arg(long)]
        lifecycle: Option<PathBuf>,
        /// Record the room's egress on the host side: capture packets on its own
        /// tap with `tcpdump`, then emit `witness.json` + `witness.pcap` into
        /// `--out`. The witness is observed outside the guest's trust boundary,
        /// so a compromised guest can neither forge nor hide it. Requires
        /// `tcpdump` on the host (a missing one is a hard error). Off by default.
        /// Requires `--out`: the witness has nowhere to persist without it, so
        /// it's a parse error rather than a run that silently emits nothing.
        /// Excludes `--keep`: the witness persists into `--out` (which `--keep`
        /// forbids), and a kept room outlives the capture.
        #[arg(long, conflicts_with = "keep", requires = "out_dir")]
        witness: bool,
        /// Deliver the named host env var into the guest over a per-room
        /// vsock instead of SSH env forwarding (repeatable). The value is
        /// read from this process's environment at admission — unset or
        /// empty fails before any slot is claimed — and then removed from
        /// the process env, so SSH `SendEnv` can no longer forward it. The
        /// guest stages secrets at `/run/rooms/secrets.env` (tmpfs, 0600)
        /// and the workload starts only after delivery is acked; no ack
        /// fails the room closed (`secrets_failed`, no workload).
        #[arg(long = "secret", value_parser = valid_secret_name)]
        secret: Vec<String>,
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
#[allow(
    clippy::struct_excessive_bools,
    reason = "a flat DTO mirroring independent, orthogonal CLI flags 1:1 — not a state machine to fold into enums"
)]
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
    lifecycle: Option<PathBuf>,
    witness: bool,
    /// Harvested `--secret` payload — built pre-runtime in `main`, where the
    /// env mutation it entails is still sound.
    secrets: Option<vsock::SecretsPayload>,
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

/// Validate a `--secret` name: env-var shaped, so a name can never smuggle
/// `=` or framing characters into the delivery blob.
fn valid_secret_name(s: &str) -> Result<String, String> {
    let mut chars = s.chars();
    let head_ok = chars
        .next()
        .is_some_and(|c| c.is_ascii_uppercase() || c == '_');
    let tail_ok = chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
    if head_ok && tail_ok {
        return Ok(s.to_owned());
    }
    Err(format!(
        "invalid --secret name '{s}': want an env-var name matching [A-Z_][A-Z0-9_]*"
    ))
}

/// Harvest `--secret` values from this process's environment into the vsock
/// blob, then REMOVE each variable from the environment: SSH `SendEnv` can
/// only forward what exists, so the removal *is* the suppression — a
/// vsock-delivered name can no longer also reach the guest ambiently over
/// any SSH session of this run.
///
/// Fails closed at admission: an unset or empty variable, or a value that
/// would break the line-oriented blob framing, rejects the run before any
/// slot is claimed.
fn harvest_secrets(names: &[String]) -> Result<Option<vsock::SecretsPayload>, RoomsError> {
    if names.is_empty() {
        return Ok(None);
    }
    let mut seen = std::collections::BTreeSet::new();
    let mut pairs = Vec::new();
    for name in names {
        if !seen.insert(name.as_str()) {
            continue;
        }
        let value = std::env::var(name).map_err(|_| {
            RoomsError::Internal(format!("--secret {name}: not set in the host environment"))
        })?;
        if value.is_empty() {
            return Err(RoomsError::Internal(format!(
                "--secret {name}: value is empty"
            )));
        }
        if value.contains('\n') {
            return Err(RoomsError::Internal(format!(
                "--secret {name}: value contains a newline, which the line-oriented blob cannot carry"
            )));
        }
        std::env::remove_var(name);
        pairs.push((name.clone(), value));
    }
    Ok(Some(vsock::SecretsPayload::encode(&pairs)))
}

/// Admission check for `--secret`: the guest kernel must carry virtio-vsock,
/// or the fetch hook could never run and the room would fail closed only
/// after a full boot. Scans the kernel image for the driver's symbol strings
/// — crude but static; a miss fails here with remediation rather than
/// mid-run as an opaque `secrets_failed`.
fn ensure_kernel_has_vsock(kernel: &Path) -> Result<(), RoomsError> {
    let bytes = std::fs::read(kernel)
        .map_err(|e| RoomsError::Internal(format!("read kernel {}: {e}", kernel.display())))?;
    if doctor::kernel_carries_vsock(&bytes) {
        return Ok(());
    }
    Err(RoomsError::Internal(format!(
        "--secret: guest kernel {} has no virtio_vsock support; use a kernel built with CONFIG_VIRTIO_VSOCKETS=y",
        kernel.display()
    )))
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

fn main() -> ExitCode {
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    // Harvest `--secret` values (which mutates the process environment —
    // each harvested var is removed so SSH `SendEnv` can no longer forward
    // it) while the process is still single-threaded: `std::env` mutation
    // is unsound once the tokio worker threads may be reading the
    // environment concurrently.
    let secrets = match harvest_cli_secrets(&cli) {
        Ok(secrets) => secrets,
        Err(err) => {
            if run_wants_json(&cli) {
                emit_run_error_json(&err);
            }
            warn!(error = %err, "command failed");
            return ExitCode::from(exit_code_for_error(&err));
        }
    };
    async_main(cli, secrets)
}

/// Extract and harvest the `--secret` names of a `run` invocation; every
/// other command carries none.
fn harvest_cli_secrets(cli: &Cli) -> Result<Option<vsock::SecretsPayload>, RoomsError> {
    let Command::Run { secret, .. } = &cli.command else {
        return Ok(None);
    };
    harvest_secrets(secret)
}

/// Whether the invocation asked for the machine-readable error record.
const fn run_wants_json(cli: &Cli) -> bool {
    matches!(cli.command, Command::Run { json: true, .. })
}

#[tokio::main]
async fn async_main(cli: Cli, secrets: Option<vsock::SecretsPayload>) -> ExitCode {
    match dispatch(cli, secrets).await {
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

async fn dispatch(cli: Cli, secrets: Option<vsock::SecretsPayload>) -> Result<u8, RoomsError> {
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
            lifecycle,
            witness,
            secret: _,
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
                    lifecycle,
                    witness,
                    secrets,
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

    // Fail closed on --witness without tcpdump, before anything else: a host that
    // can't witness must never boot a room that would run unwitnessed (only the
    // initial start fails closed; a mid-run capture death is tolerated).
    if args.witness {
        witness::ensure_tcpdump_available().map_err(RoomsError::Internal)?;
    }

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
    // `--secret` admission, part two (values were harvested pre-runtime in
    // `main`): prove the guest kernel can even open a vsock, before any slot
    // is claimed or VM booted.
    if args.secrets.is_some() {
        ensure_kernel_has_vsock(&kernel)?;
    }

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
    // The lifecycle stream is also a fallible host-side input: create it before
    // the claim so an unwritable path fails fast without leaking a slot.
    let lifecycle = resolve_lifecycle(args.lifecycle.as_deref(), &room_id)?;
    let me = slot::Claimer::current().ok_or_else(|| {
        RoomsError::Internal("cannot read this process's identity for the slot claim".to_owned())
    })?;
    // The cap is a host fact; --max-pool / ROOMS_MAX_POOL can only lower it.
    let cap = config.effective_max_pool(args.max_pool);
    let claimed = match slot::claim(&state_base, &room_id, me, cap, None) {
        Ok(claimed) => claimed,
        Err(e) => {
            if let SlotError::PoolFull { cap } = &e {
                lifecycle.emit(&Event::PoolFull { cap: *cap });
            }
            return Err(e.into());
        }
    };
    lifecycle.emit(&Event::SlotAllocated {
        slot: claimed.index,
        tap: claimed.tap.clone(),
    });
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
        witness: args.witness,
        secrets: args.secrets.as_ref(),
    };
    let mut vm = match firecracker::boot(&boot_req, config).await {
        Ok(vm) => vm,
        Err(e) => {
            lifecycle.emit(&Event::BootFailed {
                error: e.to_string(),
            });
            // boot's guard frees the slot when it got far enough to take
            // ownership; this covers an early failure before that. Compare-and-
            // delete makes the double-free safe (AlreadyFree / AlreadyReassigned).
            let _ = slot::free(&state_base, claimed.index, &room_id);
            return Err(e.into());
        }
    };
    emit_started(
        &lifecycle,
        vm.pid(),
        args.witness.then(|| claimed.tap.clone()),
    );
    if args.keep {
        vm.guard_mut().set_suppress_cleanup(true);
    }

    let env = PostBootEnv {
        network: &network,
        key: &key,
        config,
        max_wall: args.max_wall,
        lifecycle: &lifecycle,
    };
    let secrets_delivery = vm.take_secrets_delivery();
    let outcome = post_boot(&env, &action, &mut vm, secrets_delivery).await;
    collect_run_artifacts(&env, &action, &claimed, args.out_dir.as_deref(), &mut vm).await;
    let residue = CleanupResidue::for_room(config, &state_base, &claimed, &room_id);
    teardown(vm, args.keep, &lifecycle, &residue).await;
    outcome
}

/// The on-disk traces a completed teardown must have erased. `cleanup` keeps
/// them on purpose when it can't finish (a stranded jail mount, a failed
/// room-dir removal) so `rooms gc` can retry — which is exactly what the
/// stream must report as not-done.
struct CleanupResidue {
    room_dir: Option<PathBuf>,
    jail_dir: Option<PathBuf>,
    slot_file: PathBuf,
    room_id: String,
}

impl CleanupResidue {
    /// The residue a run's teardown must erase, derived from its resolved paths.
    fn for_room(
        config: &RoomsConfig,
        state_base: &Path,
        claimed: &room::Slot,
        room_id: &str,
    ) -> Self {
        Self {
            room_dir: config.room_dir(room_id),
            jail_dir: config.jail_instance_dir(room_id),
            slot_file: state_base
                .join(slot::SLOTS_DIR)
                .join(claimed.index.to_string()),
            room_id: room_id.to_owned(),
        }
    }

    /// What teardown left behind, or `None` when the room is fully reaped.
    fn remaining(&self) -> Option<String> {
        let mut left = Vec::new();
        if self.room_dir.as_deref().is_some_and(Path::exists) {
            left.push("room dir");
        }
        if self.jail_dir.as_deref().is_some_and(Path::exists) {
            left.push("jail dir");
        }
        if self.slot_still_ours() {
            left.push("slot claim");
        }
        if left.is_empty() {
            return None;
        }
        Some(left.join(" + "))
    }

    /// Whether the slot file still names this room. A missing file is freed;
    /// a file naming a different room is a sibling's fresh claim of the reused
    /// index, never this room's residue.
    fn slot_still_ours(&self) -> bool {
        let Ok(token) = std::fs::read_to_string(&self.slot_file) else {
            return false;
        };
        token.lines().next() == Some(self.room_id.as_str())
    }
}

/// Tear the room down — or preserve it under `--keep` — recording the cleanup
/// outcome on the lifecycle stream. A `--keep` run records nothing: cleanup is
/// deliberately suppressed, not done and not failed. `cleanup_done` is
/// verified against the on-disk residue, never inferred from shutdown's
/// return — cleanup deliberately keeps the room dir + slot behind on a
/// partial teardown so `rooms gc` can retry, and the stream must say so.
async fn teardown(
    mut vm: firecracker::BootedVm,
    keep: bool,
    lifecycle: &Lifecycle,
    residue: &CleanupResidue,
) {
    if keep {
        vm.guard_mut().dismiss();
        // Prevent kill_on_drop from terminating the microVM — operator inspects manually.
        std::mem::forget(vm);
        info!("--keep: cleanup suppressed; firecracker process and state dir preserved");
        return;
    }
    if let Err(e) = vm.shutdown().await {
        warn!(error = %e, "shutdown reported an error after post-boot");
        lifecycle.emit(&Event::CleanupFailed {
            error: e.to_string(),
        });
        return;
    }
    match residue.remaining() {
        None => lifecycle.emit(&Event::CleanupDone),
        Some(left) => lifecycle.emit(&Event::CleanupFailed {
            error: format!("{left} left behind; `rooms gc` will retry"),
        }),
    }
}

/// The `--lifecycle` sink for this run: disabled without the flag, otherwise
/// the created stream file. A create failure is an input error, mapped so the
/// caller can fail fast before any slot is claimed.
fn resolve_lifecycle(path: Option<&Path>, room_id: &str) -> Result<Lifecycle, RoomsError> {
    let Some(path) = path else {
        return Ok(Lifecycle::disabled());
    };
    Lifecycle::create(path, room_id).map_err(|e| {
        RoomsError::Internal(format!("create --lifecycle stream {}: {e}", path.display()))
    })
}

/// Emit the "started" transitions once the VMM is up. `witness_started` (when
/// `--witness`) precedes `vmm_started` to match causal order: capture began
/// inside `boot`, before the VMM, so no guest packet predates it.
fn emit_started(lifecycle: &Lifecycle, pid: Option<u32>, witness_tap: Option<String>) {
    if let Some(tap) = witness_tap {
        lifecycle.emit(&Event::WitnessStarted { tap });
    }
    lifecycle.emit(&Event::VmmStarted { pid });
}

/// Finalize a run's artifacts before teardown: collect `/workspace/out` into
/// `--out`, then stop + summarize the egress witness (if any) and drop its
/// artifacts beside it. Reuses the [`PostBootEnv`] the run already threaded
/// (network + key + lifecycle) plus the slot + `--out` dir.
///
/// Ordering is load-bearing. The witness is stopped *after* collection so the
/// capture covers the whole live room — a guest that egresses during the
/// collection window (the VM is still up) is still witnessed. The witness is
/// summarized regardless of `--out` (so `witness_done` always fires) and
/// persisted after collection because collection clears the `--out` dir first.
/// All of this must precede teardown, which deletes the tap and the room dir
/// where `witness.pcap` is staged.
async fn collect_run_artifacts(
    env: &PostBootEnv<'_>,
    action: &Action,
    slot: &room::Slot,
    out_dir: Option<&Path>,
    vm: &mut firecracker::BootedVm,
) {
    if let Some(out_dir) = out_dir {
        collect_and_record(
            &env.network.guest_ip,
            env.key,
            action,
            out_dir,
            env.lifecycle,
        )
        .await;
    }
    let witnessed = match vm.take_witness() {
        Some(capture) => Some(summarize_witness(capture, slot, env.lifecycle).await),
        None => None,
    };
    let (Some(w), Some(out_dir)) = (&witnessed, out_dir) else {
        return;
    };
    persist_witness(w, out_dir).await;
}

/// A finalized egress witness: the summary plus the raw pcap bytes, held
/// between summarizing (while the staged file still exists) and persisting
/// into `--out` (teardown removes the room dir where the file was staged).
struct Witnessed {
    summary: artifacts::Witness,
    raw: Vec<u8>,
}

/// Stop the capture, summarize the pcap, and record the outcome on the
/// lifecycle stream. Never fatal: a mid-run capture death or an unreadable pcap
/// yields an empty, `capture_complete: false` witness rather than failing the
/// run (only the initial `--witness` start fails closed, back in `boot`).
///
/// The pcap is read *after* the capture stops — reading first would race the
/// final flush — and a read failure forces `capture_complete: false`: evidence
/// that can't be read must never present as an exhaustive record.
async fn summarize_witness(
    capture: witness::Capture,
    slot: &room::Slot,
    lifecycle: &Lifecycle,
) -> Witnessed {
    let tap = capture.tap().to_owned();
    let pcap_path = capture.pcap_path().to_owned();
    let outcome = capture.stop().await;
    let (raw, readable) = match tokio::fs::read(&pcap_path).await {
        Ok(bytes) => (bytes, true),
        Err(e) => {
            warn!(pcap = %pcap_path.display(), error = %e, "failed to read witness.pcap; summary marked incomplete");
            (Vec::new(), false)
        }
    };
    let local = artifacts::GatewayLocal {
        gateway: slot.gateway,
        guest: slot.guest,
    };
    let complete = outcome.complete && readable;
    let summary = artifacts::summarize_pcap(&raw, &tap, local, complete);
    lifecycle.emit(&Event::WitnessDone {
        destinations: summary.destinations.len(),
        complete: summary.capture_complete,
    });
    Witnessed { summary, raw }
}

/// Persist the witness artifacts into `--out`: `witness.json` and `witness.pcap`,
/// both written atomically (temp + rename), so the evidence survives the room.
/// Creates the dir first — a run that collected nothing (an idle room) still
/// gets its witness. Best-effort — a write failure is logged, never fatal (the
/// run's own outcome and lifecycle summary stand).
async fn persist_witness(w: &Witnessed, out_dir: &Path) {
    if let Err(e) = tokio::fs::create_dir_all(out_dir).await {
        warn!(out = %out_dir.display(), error = %e, "failed to create --out for the witness");
        return;
    }
    match serde_json::to_vec_pretty(&w.summary) {
        Ok(bytes) => {
            if let Err(e) = write_out_atomic(out_dir, artifacts::WITNESS_JSON, &bytes).await {
                warn!(error = %e, "failed to write witness.json");
            }
        }
        Err(e) => warn!(error = %e, "failed to serialize witness.json"),
    }
    if let Err(e) = write_out_atomic(out_dir, artifacts::WITNESS_PCAP, &w.raw).await {
        warn!(error = %e, "failed to write witness.pcap into --out");
    }
}

/// Atomic artifact write into `--out`: temp file in the same dir, then rename,
/// so a crash mid-write never leaves a half-written file for a reader to choke
/// on. Mirrors the runner's host-artifact discipline for `changeset.json`.
async fn write_out_atomic(dir: &Path, name: &str, bytes: &[u8]) -> std::io::Result<()> {
    let final_path = dir.join(name);
    let tmp_path = dir.join(format!("{name}.tmp"));
    tokio::fs::write(&tmp_path, bytes).await?;
    tokio::fs::rename(&tmp_path, &final_path).await
}

/// Best-effort collect `/workspace/out` to the host after an exec (no-op for
/// Idle/Keep), recording the collection transitions on the lifecycle stream.
/// A failure is logged and recorded, never fatal to the run. Bounded by
/// [`PRE_TEARDOWN_GRACE`]: collection's SSH is only `ConnectTimeout`-bounded,
/// and a wall cap fires precisely when the guest may be unresponsive, so an
/// unbounded collect could hang teardown.
async fn collect_and_record(
    guest_ip: &str,
    key: &Path,
    action: &Action,
    out_dir: &Path,
    lifecycle: &Lifecycle,
) {
    // No-op for Action::Idle (--command/--runner omitted); Action::Keep is
    // already excluded by clap's --out/--keep conflict.
    if !matches!(action, Action::Exec(_)) {
        return;
    }
    lifecycle.emit(&Event::CollectionStarted);
    let collect = collect_to_host(guest_ip, key, out_dir);
    match tokio::time::timeout(PRE_TEARDOWN_GRACE, collect).await {
        Ok(Ok(())) => lifecycle.emit(&Event::CollectionDone),
        Ok(Err(error)) => lifecycle.emit(&Event::CollectionFailed { error }),
        Err(_) => {
            warn!("artifact collection timed out (guest unresponsive); proceeding to teardown");
            lifecycle.emit(&Event::CollectionFailed {
                error: "timed out (guest unresponsive)".to_owned(),
            });
        }
    }
}

/// Pull `/workspace/out` and the overlay change set to the host. The out-dir
/// collect is the primary artifact — its failure is the returned error; the
/// change set stays best-effort (an absent overlay is normal on a
/// writable-rootfs run and never affects the result).
async fn collect_to_host(guest_ip: &str, key: &Path, out_dir: &Path) -> Result<(), String> {
    let primary = runner::collect_out_to_host(guest_ip, key, out_dir).await;
    match runner::collect_changeset_to_host(guest_ip, key, out_dir).await {
        Ok(()) => info!(out = %out_dir.display(), "collected overlay changeset"),
        Err(e) => warn!(error = %e, "collect overlay changeset failed"),
    }
    if let Err(e) = primary {
        warn!(error = %e, out = %out_dir.display(), "collect /workspace/out to host failed");
        return Err(e.to_string());
    }
    info!(out = %out_dir.display(), "collected /workspace/out to host");
    Ok(())
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

/// Everything the post-boot phase needs besides the action and the VM itself,
/// bundled to stay under the argument-count cap.
struct PostBootEnv<'a> {
    network: &'a firecracker::NetworkConfig,
    key: &'a Path,
    config: &'a RoomsConfig,
    max_wall: Option<Duration>,
    lifecycle: &'a Lifecycle,
}

async fn post_boot(
    env: &PostBootEnv<'_>,
    action: &Action,
    vm: &mut firecracker::BootedVm,
    secrets: Option<vsock::Delivery>,
) -> Result<u8, RoomsError> {
    match action {
        Action::Keep => {
            info!(
                guest_ip = %env.network.guest_ip,
                "microVM is up; Ctrl-C to shut down (try `ping {}` from another shell)",
                env.network.guest_ip,
            );
            tokio::signal::ctrl_c()
                .await
                .map_err(|e| RoomsError::Internal(e.to_string()))?;
            // Explicitly after the await: a kept room's guest may still be
            // fetching while the operator inspects, so the handle (and its
            // listener) must survive the whole Ctrl-C wait.
            drop(secrets);
            Ok(0)
        }
        Action::Exec(run) => exec_workload(env, run, secrets).await,
        Action::Idle => idle_linger(env, vm).await,
    }
}

/// How long a bare `rooms run` (no `--command`, no `--runner cursor`) lingers
/// before auto-shutdown — a POC placeholder that just proves the boot came up.
///
/// This is the ONLY fixed auto-shutdown in a run, and it is scoped to the
/// no-exec path alone: an exec run ([`Action::Exec`]) never touches it. Its
/// workload is never cut by *this* fixed timer — only an explicit `--max-wall`
/// cap or Ctrl-C ends the exec race (see [`race_workload`]); the SSH-readiness
/// probe keeps its own `guest_reach_timeout`. A `--command` whose guest becomes
/// SSH-ready after this window must still run, so this must never leak into the
/// exec path.
const BARE_BOOT_LINGER: Duration = Duration::from_secs(3);

/// The bare-boot (`Action::Idle`) path: linger [`BARE_BOOT_LINGER`], confirm the
/// VMM is still alive, then shut down. No workload runs here, so `--max-wall`
/// has nothing to bound.
async fn idle_linger(
    env: &PostBootEnv<'_>,
    vm: &mut firecracker::BootedVm,
) -> Result<u8, RoomsError> {
    if env.max_wall.is_some() {
        warn!("--max-wall set but no exec to bound (idle run); the cap has no effect");
    }
    tokio::time::sleep(BARE_BOOT_LINGER).await;
    if vm.is_alive().map_err(RoomsError::Firecracker)? {
        info!("microVM is up; shutting down (POC: no exec yet)");
        return Ok(0);
    }
    Err(RoomsError::Firecracker(
        rooms::error::FirecrackerError::ProcessExitedEarly {
            exit_code: -1,
            stderr_tail: String::new(),
        },
    ))
}

/// Which arm of the exec race won. Split from its side effects (lifecycle
/// emit + the abort-run record) so the race itself — the correctness-critical
/// invariant that the workload is bounded ONLY by an explicit `--max-wall` and
/// Ctrl-C, never a fixed auto-shutdown timer — is unit-testable without SSH,
/// KVM, or an OS signal. `Completed` carries the workload's own `Result` so a
/// real exec error keeps its message; the abort arms carry no result — their
/// exit code (130 / 124) is decided by the arm, not the workload.
#[derive(Debug)]
enum ExecRace {
    /// The workload ran to completion; carries its own result.
    Completed(Result<u8, RoomsError>),
    /// Ctrl-C fired first (exit 130).
    Cancelled,
    /// The `--max-wall` cap fired first (exit 124).
    TimedOut,
}

/// Race the workload future against Ctrl-C and the optional wall-clock cap.
///
/// The workload has NO fixed timeout of its own: an unset `max_wall` leaves the
/// select racing `work` only against Ctrl-C (the cap arm is a future that never
/// resolves), so a guest whose sshd becomes reachable seconds after boot still
/// runs to completion. This is the bug this function guards against — the exec
/// path must outlive the bare-boot 3s placeholder, never be cut by it — so the
/// race lives here, isolated and tested, rather than inline where a fixed bound
/// could creep back in unnoticed.
///
/// `biased` means a `work` that finished the same instant the cap fired keeps
/// its real result rather than a spurious 124.
async fn race_workload<W>(work: W, max_wall: Option<Duration>) -> ExecRace
where
    W: std::future::Future<Output = Result<u8, RoomsError>>,
{
    tokio::pin!(work);
    // Fires at the wall-clock cap, or never when there's no cap.
    let cap = async {
        match max_wall {
            Some(limit) => tokio::time::sleep(limit).await,
            None => std::future::pending::<()>().await,
        }
    };
    tokio::pin!(cap);
    tokio::select! {
        biased;
        res = &mut work => ExecRace::Completed(res),
        _ = tokio::signal::ctrl_c() => ExecRace::Cancelled,
        () = &mut cap => ExecRace::TimedOut,
    }
}

/// Drive the exec path — wait for the workload channel, run the workload —
/// racing the whole sequence against Ctrl-C and the wall-clock cap, and
/// recording each transition on the lifecycle stream.
async fn exec_workload(
    env: &PostBootEnv<'_>,
    run: &runner::Runner,
    secrets: Option<vsock::Delivery>,
) -> Result<u8, RoomsError> {
    let guest_ip = env.network.guest_ip.clone();
    let lifecycle = env.lifecycle;
    // started_at captures when rooms began attempting exec (SSH probe,
    // then runner). Exec writes its own started_at into result.json on
    // the success path; this outer one surfaces only in the abort records
    // (cancel / timeout below, and the secrets gate inside the workload),
    // where the guest command may never have begun.
    let started_at = Utc::now();
    // Wrap the entire setup-and-exec sequence (probe sshd, then exec). Dropping
    // `work` cascades through each child future — kill_on_drop fires on every
    // spawned ssh client — so a Ctrl-C at any point still lets run_room's
    // vm.shutdown() run cleanly.
    let work = run_workload(env, run, secrets, started_at);
    match race_workload(work, env.max_wall).await {
        ExecRace::Completed(res) => res,
        // The abort outcome is decided the instant the arm fires; the stream
        // records it BEFORE the best-effort guest write below, which can stall
        // up to its grace bound on the very guest that just went unresponsive.
        ExecRace::Cancelled => {
            info!("ctrl-c received during exec setup or run; aborting and shutting down");
            lifecycle.emit(&Event::WorkloadExited {
                exit_code: 130,
                status: WorkloadStatus::Cancelled,
            });
            record_aborted_run(
                &guest_ip,
                env.key,
                130,
                RunStatus::Cancelled,
                started_at,
                run.command_argv(),
            )
            .await;
            Ok(130)
        }
        ExecRace::TimedOut => {
            warn!(max_wall = ?env.max_wall, "max wall-clock cap reached during exec; aborting and shutting down");
            lifecycle.emit(&Event::WorkloadExited {
                exit_code: 124,
                status: WorkloadStatus::TimedOut,
            });
            record_aborted_run(
                &guest_ip,
                env.key,
                124,
                RunStatus::TimedOut,
                started_at,
                run.command_argv(),
            )
            .await;
            Ok(124)
        }
    }
}

/// Wait for the workload channel, pass the secrets gate when one is armed,
/// then run the workload, recording each transition on the lifecycle stream.
/// Returns the resolved exit code; a post-run push failure fails the run
/// without erasing the workload's real exit (which the stream + result.json
/// already carry).
async fn run_workload(
    env: &PostBootEnv<'_>,
    run: &runner::Runner,
    secrets: Option<vsock::Delivery>,
    started_at: DateTime<Utc>,
) -> Result<u8, RoomsError> {
    let lifecycle = env.lifecycle;
    wait_for_channel(env).await?;
    if let Some(delivery) = secrets {
        gate_on_secrets(env, run, delivery, started_at).await?;
    }
    lifecycle.emit(&Event::WorkloadStarted {
        command: run.command_argv(),
    });
    let outcome = match runner::exec(&env.network.guest_ip, env.key, run).await {
        Ok(outcome) => outcome,
        Err(e) => {
            lifecycle.emit(&Event::WorkloadFailed {
                error: e.to_string(),
            });
            return Err(RoomsError::Internal(e.to_string()));
        }
    };
    lifecycle.emit(&Event::WorkloadExited {
        exit_code: outcome.exit_code,
        status: workload_status(outcome.status),
    });
    if let Some(push_error) = outcome.push_error {
        lifecycle.emit(&Event::WorkloadFailed {
            error: push_error.clone(),
        });
        return Err(RoomsError::Internal(push_error));
    }
    Ok(u8::try_from(outcome.exit_code).unwrap_or(2))
}

/// Wait for the guest's workload channel, reporting guest/SSH readiness on
/// the lifecycle stream. Readiness only: the workload handoff is recorded by
/// the caller, after the secrets gate — `workload_started` must never precede
/// a confirmed delivery. The ping probe runs only when a stream consumes it,
/// so runs without `--lifecycle` keep the exact probe behavior they had.
async fn wait_for_channel(env: &PostBootEnv<'_>) -> Result<(), RoomsError> {
    let lifecycle = env.lifecycle;
    let waited = runner::wait_for_ssh_observed(
        &env.network.guest_ip,
        env.key,
        env.config,
        lifecycle.is_enabled(),
        |signal| match signal {
            runner::GuestSignal::GuestReady => lifecycle.emit(&Event::GuestReady),
            runner::GuestSignal::SshReady => lifecycle.emit(&Event::SshReady),
        },
    )
    .await;
    if let Err(e) = waited {
        emit_readiness_failure(lifecycle, &e);
        return Err(RoomsError::Firecracker(e));
    }
    Ok(())
}

/// How long after SSH readiness the guest may take to ack the vsock secrets
/// delivery. The fetch hook runs before sshd in the guest's boot order, so
/// the ack normally exists well before this gate is even reached; the bound
/// exists for the pathological cases (an image without the hook, a wedged
/// fetch).
const SECRETS_ACK_TIMEOUT: Duration = Duration::from_secs(10);

/// The `--secret` workload gate: the guest must have staged and acked every
/// requested secret before the workload is handed over. A timeout or failed
/// delivery is terminal — the aborted run is recorded and `workload_started`
/// is never emitted (fail closed).
async fn gate_on_secrets(
    env: &PostBootEnv<'_>,
    run: &runner::Runner,
    delivery: vsock::Delivery,
    started_at: DateTime<Utc>,
) -> Result<(), RoomsError> {
    match delivery.await_delivered(SECRETS_ACK_TIMEOUT).await {
        Ok(()) => {
            env.lifecycle.emit(&Event::SecretsDelivered);
            Ok(())
        }
        Err(e) => {
            let error = format!("secrets: {e}");
            env.lifecycle.emit(&Event::SecretsFailed {
                error: error.clone(),
            });
            record_aborted_run(
                &env.network.guest_ip,
                env.key,
                1,
                RunStatus::Failed,
                started_at,
                run.command_argv(),
            )
            .await;
            Err(RoomsError::Internal(error))
        }
    }
}

/// Classify a readiness failure for the stream: a probe timeout means the
/// guest never became usable (`guest_unreachable`); anything else is a
/// host-side probe fault (`workload_failed`) — the guest may be fine.
fn emit_readiness_failure(lifecycle: &Lifecycle, e: &rooms::error::FirecrackerError) {
    if matches!(e, rooms::error::FirecrackerError::GuestUnreachable { .. }) {
        lifecycle.emit(&Event::GuestUnreachable {
            error: e.to_string(),
        });
        return;
    }
    lifecycle.emit(&Event::WorkloadFailed {
        error: e.to_string(),
    });
}

/// Map a `result.json` status onto the lifecycle vocabulary — the same terms,
/// so the two surfaces never disagree about one run.
const fn workload_status(status: RunStatus) -> WorkloadStatus {
    match status {
        RunStatus::Succeeded => WorkloadStatus::Succeeded,
        RunStatus::Failed => WorkloadStatus::Failed,
        RunStatus::TimedOut => WorkloadStatus::TimedOut,
        RunStatus::Cancelled => WorkloadStatus::Cancelled,
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
        changeset_exit_code, diff_changeset, exit_code_for_error, harvest_secrets, humanize_secs,
        parse_max_pool, parse_max_wall, race_workload, resolve_action, run_error_record,
        truncate_label, valid_secret_name, Cli, Command, ExecRace, RoomsError, RunArgs, RunnerKind,
        SlotError, BARE_BOOT_LINGER,
    };
    use crate::artifacts::Changeset;
    use clap::{CommandFactory, Parser};
    use tempfile::tempdir;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn secret_name_parser_accepts_env_shapes_and_rejects_others() {
        for good in ["CURSOR_API_KEY", "GH_TOKEN", "_X", "A1_B2"] {
            assert!(valid_secret_name(good).is_ok(), "{good} should parse");
        }
        for bad in ["", "lower_case", "1LEADING", "WITH-DASH", "WITH=EQ", "A B"] {
            assert!(
                valid_secret_name(bad).is_err(),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn harvest_secrets_removes_the_var_so_sendenv_cannot_forward_it() {
        std::env::set_var("ROOMS_TEST_HARVEST_OK", "v-1");
        let payload = harvest_secrets(&["ROOMS_TEST_HARVEST_OK".to_owned()])
            .expect("set var harvests")
            .expect("non-empty names yield a payload");
        drop(payload);
        assert!(
            std::env::var("ROOMS_TEST_HARVEST_OK").is_err(),
            "harvest must remove the env var — the removal IS the SendEnv suppression"
        );
    }

    #[test]
    fn harvest_secrets_fails_closed_on_unset_empty_and_multiline_values() {
        assert!(
            harvest_secrets(&["ROOMS_TEST_HARVEST_UNSET".to_owned()]).is_err(),
            "unset var must fail admission"
        );
        std::env::set_var("ROOMS_TEST_HARVEST_EMPTY", "");
        assert!(
            harvest_secrets(&["ROOMS_TEST_HARVEST_EMPTY".to_owned()]).is_err(),
            "empty value must fail admission"
        );
        std::env::set_var("ROOMS_TEST_HARVEST_NL", "a\nb");
        assert!(
            harvest_secrets(&["ROOMS_TEST_HARVEST_NL".to_owned()]).is_err(),
            "a newline in the value must fail admission (blob framing)"
        );
    }

    #[test]
    fn harvest_secrets_without_names_is_a_no_op() {
        assert!(harvest_secrets(&[]).expect("empty is fine").is_none());
    }

    /// The regression for the silent-reap bug: a `--command` run whose guest
    /// only becomes SSH-ready AFTER the bare-boot auto-shutdown window
    /// ([`BARE_BOOT_LINGER`], 3s) must still execute the command and return its
    /// exit code. With no `--max-wall`, the exec race is bounded only by Ctrl-C
    /// (never fires here) — no fixed timer may cut it. The paused clock lets us
    /// model a guest that takes 5s to accept SSH without a real 5s sleep.
    #[tokio::test(start_paused = true)]
    async fn exec_survives_when_guest_ready_after_auto_shutdown_window() {
        // Simulate a guest that only becomes reachable well past the 3s window,
        // then runs the command to exit code 7.
        let ready_at = BARE_BOOT_LINGER + Duration::from_secs(2);
        let work = async move {
            tokio::time::sleep(ready_at).await;
            Ok::<u8, RoomsError>(7)
        };
        // No cap: the race is work-vs-Ctrl-C only, so a slow-ready guest is not
        // reaped mid-exec.
        match race_workload(work, None).await {
            ExecRace::Completed(res) => assert_eq!(
                res.expect("the workload completed"),
                7,
                "a guest ready after the bare-boot window must still run its command"
            ),
            ExecRace::Cancelled => panic!("nothing sent Ctrl-C; the race must not cancel"),
            ExecRace::TimedOut => {
                panic!("no --max-wall was set; a fixed timer must not cut the exec")
            }
        }
    }

    /// A completed workload's own error propagates through the race unchanged —
    /// the `Completed` arm carries the workload's `Result`, so a genuine exec
    /// failure is not masked as a success or an abort.
    #[tokio::test(start_paused = true)]
    async fn exec_race_propagates_workload_error() {
        let work = async {
            tokio::time::sleep(Duration::from_secs(1)).await;
            Err::<u8, RoomsError>(RoomsError::Internal("guest exec blew up".to_owned()))
        };
        match race_workload(work, None).await {
            ExecRace::Completed(res) => {
                let err = res.expect_err("the workload failed");
                assert!(
                    err.to_string().contains("guest exec blew up"),
                    "the workload's own error must survive the race; got: {err}"
                );
            }
            other => panic!("expected Completed(Err), got a different arm: {other:?}"),
        }
    }

    /// `--max-wall` still bites: a workload that outlasts the cap loses the race
    /// to the timeout arm (exit 124). This is the counterpart to the survival
    /// test — the cap is the *explicit* bound, and it must still fire.
    #[tokio::test(start_paused = true)]
    async fn exec_race_times_out_when_workload_outlasts_max_wall() {
        let cap = Duration::from_secs(1);
        let work = async {
            // Outlasts the cap by a wide margin.
            tokio::time::sleep(cap + Duration::from_secs(30)).await;
            Ok::<u8, RoomsError>(0)
        };
        assert!(
            matches!(race_workload(work, Some(cap)).await, ExecRace::TimedOut),
            "a workload past --max-wall must lose the race to the cap"
        );
    }

    /// The exact-tie bias: when work completes the same instant the cap fires,
    /// the `biased` select honors the real result, not a spurious 124.
    #[tokio::test(start_paused = true)]
    async fn exec_race_biases_completed_work_over_a_simultaneous_cap() {
        let cap = Duration::from_secs(5);
        let work = async move {
            tokio::time::sleep(cap).await;
            Ok::<u8, RoomsError>(3)
        };
        match race_workload(work, Some(cap)).await {
            ExecRace::Completed(res) => assert_eq!(res.expect("completed"), 3),
            other => panic!("completed work must win the tie, got: {other:?}"),
        }
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
    fn cleanup_residue_reports_leftovers_and_ignores_reused_slots() {
        let dir = tempdir().expect("tempdir");
        let room_dir = dir.path().join("room");
        let slot_file = dir.path().join("slots-1");
        let residue = super::CleanupResidue {
            room_dir: Some(room_dir.clone()),
            jail_dir: Some(dir.path().join("jail")),
            slot_file: slot_file.clone(),
            room_id: "room-a".to_owned(),
        };
        // Fully reaped: nothing on disk.
        assert_eq!(residue.remaining(), None);

        // A surviving room dir and a slot file still naming this room are
        // residue; both must be reported.
        std::fs::create_dir(&room_dir).expect("create room dir");
        std::fs::write(&slot_file, "room-a\n1 2\n").expect("write slot file");
        let left = residue.remaining().expect("residue detected");
        assert!(
            left.contains("room dir") && left.contains("slot claim"),
            "expected both leftovers named; got: {left}"
        );

        // A slot file naming a DIFFERENT room is a sibling's fresh claim of
        // the reused index — never this room's residue.
        std::fs::remove_dir(&room_dir).expect("remove room dir");
        std::fs::write(&slot_file, "room-b\n3 4\n").expect("rewrite slot file");
        assert_eq!(residue.remaining(), None);
    }

    #[test]
    fn lifecycle_flag_parses_onto_run_and_defaults_off() {
        let cli = Cli::try_parse_from([
            "rooms",
            "run",
            "--image",
            "x",
            "--command",
            "id",
            "--lifecycle",
            "/tmp/lc.ndjson",
        ])
        .expect("--lifecycle should parse");
        match cli.command {
            Command::Run { lifecycle, .. } => {
                assert_eq!(lifecycle, Some(PathBuf::from("/tmp/lc.ndjson")));
            }
            other => panic!("expected Run, got {other:?}"),
        }
        let bare = Cli::try_parse_from(["rooms", "run", "--image", "x", "--command", "id"])
            .expect("bare run parses");
        match bare.command {
            Command::Run { lifecycle, .. } => assert!(lifecycle.is_none()),
            other => panic!("expected Run, got {other:?}"),
        }
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
            lifecycle: None,
            witness: false,
            secrets: None,
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
