//! `rooms` — disposable Firecracker microVMs with specified deps.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand, ValueEnum};
use rooms::artifacts::{ResultJson, RunStatus};
use rooms::lifecycle::{Event, Lifecycle, WorkloadStatus};
use rooms::{
    artifacts, clonenet,
    config::RoomsConfig,
    doctor, egress,
    error::{CloneNetError, RoomsError, SlotError},
    firecracker, registry, room, rootfs, runner, slot, snapshot_exec, vsock, witness,
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
        /// Enforce a host-side egress policy on the room's own tap. `none`
        /// drops all external egress; `allowlist:<host-or-cidr>[,...]` permits
        /// only the listed destinations and drops the rest. Enforcement is keyed
        /// on the unforgeable tap, so a root-compromised guest cannot spoof past
        /// it. Absent ⇒ observe-only (today's behavior). Hostnames are resolved
        /// and pinned once at launch (a rotating endpoint is the documented v1
        /// limitation — use CIDR form); under `allowlist` with hostnames the
        /// DNS resolver's IP must also be listed. The proof-of-absence receipt
        /// lands in `witness.json` under `--witness`.
        #[arg(long = "egress", value_parser = egress::parse)]
        egress: Option<egress::Policy>,
    },
    /// Boot a sealed neutral-base *candidate* and leave it running for a later
    /// `rooms snapshot`.
    ///
    /// The room boots read-only through the overlay init, with IPv6 and all host
    /// egress blocked. A credential-free guest agent receives a host-created
    /// repo bundle plus optional warm command, quiesces, and emits the terminal
    /// beacon before the room is atomically persisted as `neutral`.
    BaseCreate {
        /// Path to the rootfs image (ext4).
        #[arg(long)]
        image: PathBuf,
        /// Git URL cloned into `/workspace/repo` to warm the base (at `HEAD`).
        #[arg(long)]
        repo: Option<String>,
        /// A credential-free warm-up command run by the base agent after clone.
        /// It receives a fixed scrubbed environment and has no network access.
        #[arg(long, value_parser = non_empty_command)]
        warm: Option<String>,
        /// Compatibility flag; snapshot bases always use a read-only rootfs
        /// with a tmpfs overlay.
        #[arg(long = "readonly-rootfs")]
        readonly_rootfs: bool,
        /// Lower the pool ceiling for this invocation (never raise it). Also
        /// settable via `ROOMS_MAX_POOL`.
        #[arg(long = "max-pool", env = "ROOMS_MAX_POOL", value_parser = parse_max_pool)]
        max_pool: Option<u8>,
        /// Emit a machine-readable record (`room_id`, `slot`, `provenance`) on
        /// stdout.
        #[arg(long)]
        json: bool,
    },
    /// Freeze a sealed neutral base into a recoverable Full snapshot.
    Snapshot {
        /// Running sealed-neutral base room id.
        id: String,
        /// Completed artifact directory (defaults under the rooms state base).
        #[arg(long)]
        out: Option<PathBuf>,
        /// Emit a machine-readable completion record.
        #[arg(long)]
        json: bool,
    },
    /// Restore a snapshot into one fresh room on its frozen slot.
    ///
    /// Starts a fresh jailed Firecracker, leases the snapshot's reserved slot,
    /// installs custody (witness/egress) before the guest resumes, loads the
    /// snapshot, resumes, and gates readiness on the guest's hygiene ack
    /// (reseeded RNG, stepped clock, fresh sshd host key, new identity).
    /// Exactly one lifecycle mode is required: `--keep` or `--command`.
    Restore {
        /// Completed snapshot artifact directory (from `rooms snapshot`).
        snapshot_dir: PathBuf,
        /// The backing rootfs image; its hash must match the snapshot's pin.
        #[arg(long)]
        image: PathBuf,
        /// Keep the restored room alive; hands ownership to the persisted room.
        #[arg(long, conflicts_with = "command")]
        keep: bool,
        /// Run one command in the restored guest, then tear down.
        #[arg(long, conflicts_with = "keep", value_parser = non_empty_command)]
        command: Option<String>,
        /// Slot to restore onto; must equal the snapshot's frozen slot.
        #[arg(long)]
        slot: Option<u8>,
        /// Collect the guest's `/workspace/out` into this host directory.
        #[arg(long = "out", conflicts_with = "keep")]
        out_dir: Option<PathBuf>,
        /// Record the room's egress on its own tap (requires `--out`).
        #[arg(long, conflicts_with = "keep", requires = "out_dir")]
        witness: bool,
        /// Deliver the named host env var through the resume nudge
        /// (repeatable). Same admission rules as `rooms run --secret`; the
        /// value never traverses SSH environment forwarding.
        #[arg(long = "secret", value_parser = valid_secret_name)]
        secret: Vec<String>,
        /// Enforce a host-side egress policy on the restored room's tap.
        #[arg(long = "egress", value_parser = egress::parse)]
        egress: Option<egress::Policy>,
        /// Hard wall-clock cap on the `--command` workload.
        #[arg(long = "max-wall", value_parser = parse_max_wall, conflicts_with = "keep")]
        max_wall: Option<Duration>,
        /// Emit a machine-readable terminal record on stdout.
        #[arg(long)]
        json: bool,
    },
    /// Fork one frozen snapshot into several isolated restored rooms.
    ///
    /// Every clone gets a distinct host network namespace and veth identity
    /// while reusing the snapshot's frozen guest slot. With no `--command`,
    /// all clones are kept alive; command mode waits until the complete batch
    /// is ready, runs the command concurrently, and tears every clone down.
    Clone {
        /// Completed snapshot artifact directory (from `rooms snapshot`).
        snapshot_dir: PathBuf,
        /// The backing rootfs image; its hash must match the snapshot's pin.
        #[arg(long)]
        image: PathBuf,
        /// Number of clones to fork (bounded by the host's eight-room cap).
        #[arg(short = 'n', long = "count", value_parser = parse_clone_count)]
        count: u8,
        /// Run one command in every clone concurrently, then tear all down.
        /// Omit to keep the complete batch alive.
        #[arg(long, value_parser = non_empty_command)]
        command: Option<String>,
        /// Collect each clone beneath `<dir>/<room-id>`.
        #[arg(long = "out", requires = "command")]
        out_dir: Option<PathBuf>,
        /// Record each clone's namespaced egress (requires `--out`).
        #[arg(long, requires = "out_dir")]
        witness: bool,
        /// Deliver a named host env var through every clone's resume nudge.
        #[arg(long = "secret", value_parser = valid_secret_name)]
        secret: Vec<String>,
        /// Enforce one host-side egress policy independently per clone.
        #[arg(long = "egress", value_parser = egress::parse)]
        egress: Option<egress::Policy>,
        /// Hard wall-clock cap on every concurrent command workload.
        #[arg(long = "max-wall", value_parser = parse_max_wall, requires = "command")]
        max_wall: Option<Duration>,
        /// Lower the clone-network pool ceiling for this invocation.
        #[arg(long = "max-pool", env = "ROOMS_MAX_POOL", value_parser = parse_max_pool)]
        max_pool: Option<u8>,
        /// Emit one machine-readable terminal batch record on stdout.
        #[arg(long)]
        json: bool,
    },
    /// Resume an indexed snapshot transaction, or list all pending transactions.
    SnapshotRecover {
        /// Snapshot id to resume. Omit to list pending transactions.
        id: Option<String>,
        /// Emit a machine-readable report.
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
    /// The room's egress policy; [`egress::Policy::Observe`] when the flag is
    /// absent (non-breaking default). Resolved to an [`egress::Plan`] pre-claim.
    egress: egress::Policy,
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

/// Parse the bounded fan-out requested by `rooms clone -n`.
fn parse_clone_count(s: &str) -> Result<u8, String> {
    let count: u8 = s.trim().parse().map_err(|_| {
        format!(
            "invalid clone count '{s}': want an integer 1..={}",
            slot::MAX_CLONE_LEASES
        )
    })?;
    if usize::from(count) > slot::MAX_CLONE_LEASES || count == 0 {
        return Err(format!(
            "clone count must be in 1..={}",
            slot::MAX_CLONE_LEASES
        ));
    }
    Ok(count)
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
            if wants_json_error_record(&cli) {
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
    match &cli.command {
        Command::Run { secret, .. }
        | Command::Restore { secret, .. }
        | Command::Clone { secret, .. } => harvest_secrets(secret),
        _ => Ok(None),
    }
}

/// Whether the invocation asked for the machine-readable error record — the
/// `--json` verbs, for a failure raised before `async_main` starts.
const fn wants_json_error_record(cli: &Cli) -> bool {
    matches!(
        cli.command,
        Command::Run { json: true, .. }
            | Command::BaseCreate { json: true, .. }
            | Command::Restore { json: true, .. }
            | Command::Clone { json: true, .. }
    )
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
        RoomsError::Slot(SlotError::PoolFull { .. })
        | RoomsError::CloneNet(CloneNetError::PoolFull { .. }) => 4,
        _ => 2,
    }
}

/// The coarse `error_kind` for the `rooms run --json` terminal record. Ship's
/// runner keys on `pool_full` — the one terminal error it must distinguish
/// (fail-fast backpressure, not a real failure) — without matching a string or
/// an exit code a guest could collide with.
const fn error_kind(err: &RoomsError) -> &'static str {
    match err {
        RoomsError::Slot(SlotError::PoolFull { .. })
        | RoomsError::CloneNet(CloneNetError::PoolFull { .. }) => "pool_full",
        RoomsError::Slot(_) => "slot",
        RoomsError::CloneNet(_) => "clone_net",
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
        RoomsError::Slot(SlotError::PoolFull { cap })
        | RoomsError::CloneNet(CloneNetError::PoolFull { cap }) => Some(*cap),
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

#[allow(
    clippy::too_many_lines,
    reason = "flat 1:1 CLI-variant → DTO router; no logic to extract, only field lists"
)]
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
            egress,
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
                    egress: egress.unwrap_or(egress::Policy::Observe),
                },
                &config,
            )
            .await
        }
        Command::BaseCreate {
            image,
            repo,
            warm,
            readonly_rootfs: _,
            max_pool,
            json,
        } => {
            base_create(
                BaseCreateArgs {
                    image,
                    repo,
                    warm,
                    max_pool,
                    json,
                },
                &config,
            )
            .await
        }
        Command::Snapshot { id, out, json } => {
            snapshot_cmd(&id, out.as_deref(), json, &config).await
        }
        Command::Restore {
            snapshot_dir,
            image,
            keep,
            command,
            slot,
            out_dir,
            witness,
            secret: _,
            egress,
            max_wall,
            json,
        } => {
            restore_room(
                RestoreArgs {
                    snapshot_dir,
                    image,
                    keep,
                    command,
                    slot,
                    out_dir,
                    witness,
                    secrets,
                    egress: egress.unwrap_or(egress::Policy::Observe),
                    max_wall,
                    json,
                },
                &config,
            )
            .await
        }
        Command::Clone {
            snapshot_dir,
            image,
            count,
            command,
            out_dir,
            witness,
            secret: _,
            egress,
            max_wall,
            max_pool,
            json,
        } => {
            clone_rooms(
                CloneArgs {
                    snapshot_dir,
                    image,
                    count,
                    command,
                    out_dir,
                    witness,
                    secrets,
                    egress: egress.unwrap_or(egress::Policy::Observe),
                    max_wall,
                    max_pool,
                    json,
                },
                &config,
            )
            .await
        }
        Command::SnapshotRecover { id, json } => {
            snapshot_recover_cmd(id.as_deref(), json, &config).await
        }
        Command::Collect { from } => collect_artifacts(from).await,
        Command::Doctor { image, json } => run_doctor_cmd(image.as_deref(), json, &config),
        Command::Diff { from, json } => diff_changeset(&from, json).await,
        Command::Ls { json } => list_rooms_cmd(json, &config),
        Command::Gc { dry_run, id } => gc_cmd(dry_run, id, &config),
        Command::Kill { id, json } => kill_cmd(&id, json, &config),
    }
}

async fn snapshot_cmd(
    id: &str,
    out: Option<&Path>,
    json: bool,
    config: &RoomsConfig,
) -> Result<u8, RoomsError> {
    validate_json_emitted_path(out, json, "snapshot --out")?;
    let result = snapshot_exec::create(config, id, out)
        .await
        .map_err(|error| RoomsError::Internal(error.to_string()))?;
    render_snapshot_result(&result, json)?;
    Ok(0)
}

async fn snapshot_recover_cmd(
    id: Option<&str>,
    json: bool,
    config: &RoomsConfig,
) -> Result<u8, RoomsError> {
    let report = snapshot_exec::recover(config, id)
        .await
        .map_err(|error| RoomsError::Internal(error.to_string()))?;
    if json {
        let line = serde_json::to_string_pretty(&report)
            .map_err(|error| RoomsError::Internal(error.to_string()))?;
        #[allow(
            clippy::print_stdout,
            reason = "snapshot-recover --json stdout is the documented machine contract"
        )]
        {
            println!("{line}");
        }
        return Ok(0);
    }
    if let Some(completed) = &report.completed {
        render_snapshot_result(completed, false)?;
        return Ok(0);
    }
    #[allow(
        clippy::print_stdout,
        reason = "snapshot-recover human pending roster is the command output"
    )]
    {
        for pending in report.pending {
            println!(
                "{}  base={}  boundary={}  {}",
                pending.snapshot_id, pending.base_room, pending.boundary, pending.next_action
            );
        }
    }
    Ok(0)
}

fn render_snapshot_result(
    result: &snapshot_exec::SnapshotResult,
    json: bool,
) -> Result<(), RoomsError> {
    if json {
        let line = serde_json::to_string(result)
            .map_err(|error| RoomsError::Internal(error.to_string()))?;
        #[allow(
            clippy::print_stdout,
            reason = "snapshot --json stdout is the documented machine contract"
        )]
        {
            println!("{line}");
        }
        return Ok(());
    }
    #[allow(
        clippy::print_stdout,
        reason = "snapshot human completion summary is the command output"
    )]
    {
        println!(
            "snapshot {} created at {} from base {} on slot {} (provenance={:?})",
            result.snapshot_id,
            result.directory.display(),
            result.base_room,
            result.slot,
            result.provenance
        );
    }
    Ok(())
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

/// Enforcement works without `--witness`, but the proof-of-absence / blocked
/// receipt only lands in `witness.json`. Flag the gap rather than requiring it.
fn warn_egress_without_witness(plan: &egress::Plan, witness: bool) {
    if plan.enforces() && !witness {
        warn!("--egress enforcement is active but --witness is not set; no witness.json receipt (permitted/blocked/proof-of-absence) will be produced — add --witness to record it");
    }
}

/// The guest network wiring for a claimed pool slot.
fn network_config_for(slot: &room::Slot) -> firecracker::NetworkConfig {
    firecracker::NetworkConfig {
        tap_name: slot.tap.clone(),
        guest_ip: slot.guest.to_string(),
        gateway_ip: slot.gateway.to_string(),
        prefix: slot.prefix,
    }
}

async fn run_room_inner(args: RunArgs, config: &RoomsConfig) -> Result<u8, RoomsError> {
    info!(image = ?args.image, keep = args.keep, runner = ?args.runner, "rooms run");
    // Fail closed on --witness without tcpdump, before anything else: a host that
    // can't witness must never boot a room that would run unwitnessed (only the
    // initial start fails closed; a mid-run capture death is tolerated).
    ensure_witness_available(args.witness)?;

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

    // Resolve the egress policy (pinning allowlist hostnames to IPs) BEFORE the
    // claim, so an unresolvable host fails fast without leaking a slot.
    let egress_plan = egress::resolve(&args.egress).map_err(RoomsError::Internal)?;
    warn_egress_without_witness(&egress_plan, args.witness);

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

    // Mint the room id before the claim so it stays the canonical identity; then
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
    let network = network_config_for(&claimed);
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
        provisioning: None,
        egress: &egress_plan,
        base: false,
    };
    let mut vm = match firecracker::boot(&boot_req, config).await {
        Ok(vm) => vm,
        Err(e) => {
            lifecycle.emit(&Event::BootFailed {
                error: e.to_string(),
            });
            // boot's guard frees the slot once it owns it; this covers an early
            // failure before that. Compare-and-delete makes the double-free safe.
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
        network_namespace: None,
        key: &key,
        config,
        max_wall: args.max_wall,
        lifecycle: &lifecycle,
        egress: &egress_plan,
    };
    let secrets_delivery = vm.take_secrets_delivery();
    let outcome = post_boot(&env, &action, &mut vm, secrets_delivery, None).await;
    collect_run_artifacts(&env, &action, &claimed, args.out_dir.as_deref(), &mut vm).await;
    let residue = CleanupResidue::for_room(config, &state_base, &claimed, &room_id);
    teardown(vm, args.keep, &lifecycle, &residue).await;
    outcome
}

fn ensure_witness_available(requested: bool) -> Result<(), RoomsError> {
    if !requested {
        return Ok(());
    }
    witness::ensure_tcpdump_available().map_err(RoomsError::Internal)
}

/// Flags for `rooms base-create` (a flat mirror of the CLI variant).
struct BaseCreateArgs {
    image: PathBuf,
    repo: Option<String>,
    warm: Option<String>,
    max_pool: Option<u8>,
    json: bool,
}

/// `rooms base-create`: provision, verify, seal, and hand off a running neutral
/// base (see the [`Command::BaseCreate`] docs). The `--json` error record mirrors
/// `run`.
async fn base_create(args: BaseCreateArgs, config: &RoomsConfig) -> Result<u8, RoomsError> {
    let json = args.json;
    let result = base_create_inner(args, config).await;
    if json {
        if let Err(err) = &result {
            emit_run_error_json(err);
        }
    }
    result
}

async fn base_create_inner(args: BaseCreateArgs, config: &RoomsConfig) -> Result<u8, RoomsError> {
    info!(image = ?args.image, repo = ?args.repo, "rooms base-create");
    let json = args.json;

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
    rootfs::validate_rootfs(&args.image, config.min_rootfs_bytes).map_err(RoomsError::Rootfs)?;
    rootfs::validate_snapshot_base_image(&args.image).map_err(RoomsError::Internal)?;
    // A base always wires the vsock device; prove the guest kernel can open one
    // before claiming a slot or booting.
    ensure_kernel_has_vsock(&kernel)?;
    if let Err(remediation) = doctor::ensure_rooms_fwd_installed() {
        return Err(RoomsError::Internal(remediation));
    }

    let state_base = config.resolved_state_base().ok_or_else(|| {
        RoomsError::Internal("HOME unset; cannot locate the rooms state base".to_owned())
    })?;
    let provisioning =
        runner::prepare_base_provisioning(args.repo.as_deref(), args.warm.as_deref())?;

    let room_id = firecracker::mint_room_id();
    let me = slot::Claimer::current().ok_or_else(|| {
        RoomsError::Internal("cannot read this process's identity for the slot claim".to_owned())
    })?;
    // The cap is a host fact; --max-pool / ROOMS_MAX_POOL can only lower it.
    let cap = config.effective_max_pool(args.max_pool);
    let claimed = slot::claim(&state_base, &room_id, me, cap, None)?;
    let network = network_config_for(&claimed);
    // A base is always kept alive for a later `rooms snapshot`.
    let descriptor = room::RoomDescriptor {
        command: Some("base-create".to_owned()),
        keep: true,
    };
    let egress_plan = egress::resolve(&egress::Policy::None).map_err(RoomsError::Internal)?;
    let boot_req = firecracker::BootRequest {
        kernel: &kernel,
        rootfs: &args.image,
        network: Some(&network),
        slot: Some(&claimed),
        room_id: &room_id,
        readonly_rootfs: true,
        descriptor: &descriptor,
        witness: false,
        secrets: None,
        provisioning: Some(&provisioning),
        egress: &egress_plan,
        base: true,
    };
    let mut vm = match firecracker::boot(&boot_req, config).await {
        Ok(vm) => vm,
        Err(e) => {
            // boot's guard frees the slot once it owns it; this covers an early
            // failure before that (compare-and-delete makes the double free safe).
            let _ = slot::free(&state_base, claimed.index, &room_id);
            return Err(e.into());
        }
    };

    let Some(delivery) = vm.take_provisioning_delivery() else {
        let _ = vm.shutdown().await;
        return Err(RoomsError::Internal(
            "base boot did not arm its provisioning gate".to_owned(),
        ));
    };
    if let Err(e) = delivery
        .await_quiesced(config.base_provisioning_timeout)
        .await
    {
        let _ = vm.shutdown().await;
        return Err(RoomsError::Internal(format!(
            "base provisioning failed: {e}"
        )));
    }
    let provenance = match vm.seal_base() {
        Ok(provenance) => provenance,
        Err(e) => {
            let _ = vm.shutdown().await;
            return Err(e.into());
        }
    };

    // Hand the base off running: dismiss the guard and forget the VM so neither
    // drop nor kill_on_drop tears it down. `rooms snapshot` freezes it later;
    // `rooms gc` / `rooms kill` reaps it otherwise.
    vm.guard_mut().dismiss();
    std::mem::forget(vm);
    emit_base_created(&room_id, &claimed, provenance, json);
    Ok(0)
}

/// Report a created base — a human line, or a `--json` record carrying the
/// provisioning provenance so a caller can later gate the seal on it.
fn emit_base_created(room_id: &str, slot: &room::Slot, provenance: room::Provenance, json: bool) {
    if json {
        // Serialize the provenance from the enum, not a literal, so `room.json`
        // and this record can never drift on a variant rename.
        let record = serde_json::json!({
            "room_id": room_id,
            "slot": slot.index,
            "guest_ip": slot.guest.to_string(),
            "provenance": provenance,
        });
        match serde_json::to_string(&record) {
            Ok(line) => {
                #[allow(
                    clippy::print_stdout,
                    reason = "base-create --json record; stdout is the documented contract"
                )]
                {
                    println!("{line}");
                }
            }
            Err(e) => warn!(error = %e, "failed to serialize base-create --json record"),
        }
        return;
    }
    #[allow(
        clippy::print_stdout,
        reason = "base-create human summary; stdout is the documented contract"
    )]
    {
        println!(
            "base created: room {room_id} on slot {} (guest {}); provenance=neutral — ready for `rooms snapshot`",
            slot.index, slot.guest
        );
    }
}

/// Bound on the restored guest's hygiene ack. The resume agent polls every
/// ~2s and the nudge itself is a few small frames, so a healthy restore acks
/// within seconds; the bound exists for images predating the agent.
const RESTORE_ACK_TIMEOUT: Duration = Duration::from_mins(1);

/// Clone restore has already received the resume-agent hygiene ack before it
/// enters the SSH barrier, so sshd should be seconds past startup. Polling at
/// the general cold-boot cadence would add up to two avoidable seconds to an
/// otherwise ready batch.
const CLONE_SSH_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Flags for `rooms restore` (a flat mirror of the CLI variant).
#[allow(
    clippy::struct_excessive_bools,
    reason = "a flat DTO mirroring independent, orthogonal CLI flags 1:1"
)]
struct RestoreArgs {
    snapshot_dir: PathBuf,
    image: PathBuf,
    keep: bool,
    command: Option<String>,
    slot: Option<u8>,
    out_dir: Option<PathBuf>,
    witness: bool,
    secrets: Option<vsock::SecretsPayload>,
    egress: egress::Policy,
    max_wall: Option<Duration>,
    json: bool,
}

/// Flags for `rooms clone` (a flat mirror of the CLI variant).
#[allow(
    clippy::struct_excessive_bools,
    reason = "a flat DTO mirroring independent CLI flags 1:1"
)]
struct CloneArgs {
    snapshot_dir: PathBuf,
    image: PathBuf,
    count: u8,
    command: Option<String>,
    out_dir: Option<PathBuf>,
    witness: bool,
    secrets: Option<vsock::SecretsPayload>,
    egress: egress::Policy,
    max_wall: Option<Duration>,
    max_pool: Option<u8>,
    json: bool,
}

/// The process signal that stopped a clone batch before its ownership handoff.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum CloneTermination {
    Interrupt,
    #[cfg(unix)]
    Terminate,
}

impl CloneTermination {
    const fn name(self) -> &'static str {
        match self {
            Self::Interrupt => "SIGINT",
            #[cfg(unix)]
            Self::Terminate => "SIGTERM",
        }
    }

    const fn exit_code(self) -> u8 {
        match self {
            Self::Interrupt => 130,
            #[cfg(unix)]
            Self::Terminate => 143,
        }
    }
}

/// Registers both termination handlers before clone effects begin and retains
/// the first observed signal for every later stage (including command workers).
struct CloneSignalSource {
    receiver: tokio::sync::watch::Receiver<Option<CloneTermination>>,
    terminal_commit: Option<tokio::sync::oneshot::Sender<tokio::sync::oneshot::Sender<()>>>,
    task: tokio::task::JoinHandle<()>,
}

impl CloneSignalSource {
    #[cfg_attr(
        not(unix),
        allow(
            clippy::unnecessary_wraps,
            reason = "the cross-platform call site shares Unix's fallible registration contract"
        )
    )]
    fn arm() -> Result<Self, RoomsError> {
        let (sender, receiver) = tokio::sync::watch::channel(None);
        let (terminal_commit, terminal_commit_receiver) =
            tokio::sync::oneshot::channel::<tokio::sync::oneshot::Sender<()>>();
        #[cfg(unix)]
        let task = {
            let mut interrupt =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()).map_err(
                    |error| RoomsError::Internal(format!("register clone SIGINT handler: {error}")),
                )?;
            let mut terminate = tokio::signal::unix::signal(
                tokio::signal::unix::SignalKind::terminate(),
            )
            .map_err(|error| {
                RoomsError::Internal(format!("register clone SIGTERM handler: {error}"))
            })?;
            tokio::spawn(async move {
                let signal = tokio::select! {
                    biased;
                    _ = interrupt.recv() => Some(CloneTermination::Interrupt),
                    _ = terminate.recv() => Some(CloneTermination::Terminate),
                    commit = terminal_commit_receiver => {
                        if let Ok(acknowledge) = commit {
                            let _ = acknowledge.send(());
                        }
                        None
                    }
                };
                let Some(signal) = signal else {
                    return;
                };
                if sender.send(Some(signal)).is_err() {
                    info!(
                        signal = signal.name(),
                        "clone signal arrived after batch completion"
                    );
                }
            })
        };
        #[cfg(not(unix))]
        let task = tokio::spawn(async move {
            tokio::select! {
                biased;
                result = tokio::signal::ctrl_c() => {
                    if let Err(error) = result {
                        warn!(%error, "clone Ctrl-C listener failed");
                        return;
                    }
                    if sender.send(Some(CloneTermination::Interrupt)).is_err() {
                        info!("clone Ctrl-C arrived after batch completion");
                    }
                }
                commit = terminal_commit_receiver => {
                    if let Ok(acknowledge) = commit {
                        let _ = acknowledge.send(());
                    }
                }
            }
        });
        Ok(Self {
            receiver,
            terminal_commit: Some(terminal_commit),
            task,
        })
    }

    fn receiver(&self) -> tokio::sync::watch::Receiver<Option<CloneTermination>> {
        self.receiver.clone()
    }

    /// Linearize terminal success or a kept-fleet handoff against the signal
    /// listener.
    ///
    /// The listener's biased select acknowledges this only after any already
    /// ready signal. Once acknowledged, later signals are after the custody
    /// commit rather than silently racing the last pre-commit watch sample.
    async fn commit_terminal_handoff(&mut self) -> Result<Option<CloneTermination>, RoomsError> {
        if let Some(signal) = observed_clone_termination(&self.receiver) {
            return Ok(Some(signal));
        }
        let commit = self.terminal_commit.take().ok_or_else(|| {
            RoomsError::Internal("clone terminal handoff was already committed".to_owned())
        })?;
        let (acknowledge, acknowledged) = tokio::sync::oneshot::channel();
        if commit.send(acknowledge).is_err() {
            tokio::task::yield_now().await;
            return observed_clone_termination(&self.receiver)
                .map(Some)
                .ok_or_else(|| {
                    RoomsError::Internal(
                        "clone signal listener exited before terminal handoff".to_owned(),
                    )
                });
        }
        acknowledged.await.map_err(|_| {
            RoomsError::Internal(
                "clone signal listener dropped the terminal-handoff acknowledgement".to_owned(),
            )
        })?;
        Ok(observed_clone_termination(&self.receiver))
    }
}

impl Drop for CloneSignalSource {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn observed_clone_termination(
    receiver: &tokio::sync::watch::Receiver<Option<CloneTermination>>,
) -> Option<CloneTermination> {
    *receiver.borrow()
}

async fn wait_for_clone_termination(
    receiver: &mut tokio::sync::watch::Receiver<Option<CloneTermination>>,
) -> CloneTermination {
    loop {
        let observed = *receiver.borrow_and_update();
        if let Some(signal) = observed {
            return signal;
        }
        if receiver.changed().await.is_err() {
            return std::future::pending::<CloneTermination>().await;
        }
    }
}

fn validate_json_emitted_path(
    path: Option<&Path>,
    json: bool,
    flag: &str,
) -> Result<(), RoomsError> {
    if !json {
        return Ok(());
    }
    let Some(path) = path else {
        return Ok(());
    };
    if path.to_str().is_some() {
        return Ok(());
    }
    Err(RoomsError::Internal(format!(
        "{flag} must be valid UTF-8 when --json is set"
    )))
}

/// A freshly allocated clone network until restore durably takes custody.
///
/// The synchronous drop path is deliberate: allocation precedes the spawned
/// restore task, so cancellation or a panic must still compare-and-release the
/// exact room's namespace claim.
struct AllocatedClone {
    room_id: String,
    network: Option<clonenet::CloneNet>,
    state_base: PathBuf,
}

impl AllocatedClone {
    fn network(&self) -> Result<&clonenet::CloneNet, RoomsError> {
        self.network.as_ref().ok_or_else(|| {
            RoomsError::Internal("clone network custody was already transferred".to_owned())
        })
    }

    fn transfer(&mut self) {
        self.network = None;
    }
}

impl Drop for AllocatedClone {
    fn drop(&mut self) {
        let Some(network) = &self.network else {
            return;
        };
        if let Err(error) = clonenet::free(&self.state_base, network.index, &self.room_id) {
            warn!(
                room = %self.room_id,
                clone_net = network.index,
                %error,
                "failed to release an untransferred clone network; reconciliation will retry"
            );
        }
    }
}

/// One fully restored clone held by the batch orchestrator.
///
/// `Drop` is the unwind/cancellation backstop. Normal paths call
/// [`CloneCustody::teardown`] or [`CloneCustody::preserve`]; an unexpected
/// drop reaps the VM synchronously and completes the durable restore intent.
struct CloneCustody {
    restored: Option<rooms::restore_exec::Restored>,
    out_dir: Option<PathBuf>,
    config: RoomsConfig,
}

impl CloneCustody {
    fn restored(&self) -> Result<&rooms::restore_exec::Restored, RoomsError> {
        self.restored.as_ref().ok_or_else(|| {
            RoomsError::Internal("clone restore custody was already consumed".to_owned())
        })
    }

    fn restored_mut(&mut self) -> Result<&mut rooms::restore_exec::Restored, RoomsError> {
        self.restored.as_mut().ok_or_else(|| {
            RoomsError::Internal("clone restore custody was already consumed".to_owned())
        })
    }

    fn record(
        &self,
        status: &'static str,
        exit_code: Option<u8>,
    ) -> Result<CloneRecord, RoomsError> {
        CloneRecord::from_ready(self.restored()?, self.out_dir.clone(), status, exit_code)
    }

    fn identity(&self) -> Result<(u8, String), RoomsError> {
        let restored = self.restored()?;
        let index = restored.clone_net.as_ref().ok_or_else(|| {
            RoomsError::Internal("clone restore returned no clone network".to_owned())
        })?;
        Ok((index.index, restored.room_id.clone()))
    }

    async fn teardown(mut self) -> Result<(), RoomsError> {
        let restored = self.restored.take().ok_or_else(|| {
            RoomsError::Internal("clone restore custody was already consumed".to_owned())
        })?;
        teardown_restored_clone(restored, &self.config).await
    }

    fn preserve(mut self) -> Result<(), RoomsError> {
        let restored = self.restored.take().ok_or_else(|| {
            RoomsError::Internal("clone restore custody was already consumed".to_owned())
        })?;
        let rooms::restore_exec::Restored {
            mut vm, room_id, ..
        } = restored;
        vm.guard_mut().dismiss();
        std::mem::forget(vm);
        info!(room = %room_id, "clone preserved; lease and namespace held until teardown");
        Ok(())
    }
}

/// Cancellation-safe terminal half of clone teardown.
///
/// This guard is declared before the consuming `BootedVm::shutdown` future,
/// so cancellation drops that future (and therefore the VM guard) first, then
/// runs the exact lease/network/intention finish below. Normal completion
/// disarms it after the same finish succeeds or records a retryable error.
struct CloneTeardownFinalizer {
    config: RoomsConfig,
    room_id: String,
    snapshot_id: String,
    slot_index: u8,
    clone_net_index: Option<u8>,
    armed: bool,
}

impl CloneTeardownFinalizer {
    fn finish(mut self) -> Result<(), RoomsError> {
        let result = rooms::restore_exec::finish_teardown(
            &self.config,
            &self.room_id,
            &self.snapshot_id,
            self.slot_index,
            self.clone_net_index,
        )
        .map_err(|error| {
            RoomsError::Internal(format!(
                "clone {} teardown incomplete: {error}",
                self.room_id
            ))
        });
        self.armed = false;
        result
    }
}

impl Drop for CloneTeardownFinalizer {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Err(error) = rooms::restore_exec::finish_teardown(
            &self.config,
            &self.room_id,
            &self.snapshot_id,
            self.slot_index,
            self.clone_net_index,
        ) {
            warn!(room = %self.room_id, %error, "cancelled clone teardown left residue; `rooms gc` will retry");
        }
    }
}

impl Drop for CloneCustody {
    fn drop(&mut self) {
        let Some(restored) = self.restored.take() else {
            return;
        };
        let rooms::restore_exec::Restored {
            vm,
            slot,
            room_id,
            snapshot_id,
            clone_net,
        } = restored;
        drop(vm);
        if let Err(error) = rooms::restore_exec::finish_teardown(
            &self.config,
            &room_id,
            &snapshot_id,
            slot.index,
            clone_net.as_ref().map(|net| net.index),
        ) {
            warn!(room = %room_id, %error, "clone unwind cleanup incomplete; `rooms gc` will retry");
        }
    }
}

/// Machine-readable identity and terminal state for one member of a clone
/// batch. Records are sorted by clone-network index before emission.
#[derive(Clone, Debug, serde::Serialize)]
struct CloneRecord {
    room_id: String,
    snapshot_id: String,
    slot: u8,
    guest_ip: String,
    clone_net_index: u8,
    namespace: String,
    host_veth: String,
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    exit_code: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    out_dir: Option<PathBuf>,
}

impl CloneRecord {
    fn from_ready(
        restored: &rooms::restore_exec::Restored,
        out_dir: Option<PathBuf>,
        status: &'static str,
        exit_code: Option<u8>,
    ) -> Result<Self, RoomsError> {
        let clone_net = restored.clone_net.as_ref().ok_or_else(|| {
            RoomsError::Internal("clone restore returned no clone network".to_owned())
        })?;
        Ok(Self {
            room_id: restored.room_id.clone(),
            snapshot_id: restored.snapshot_id.clone(),
            slot: restored.slot.index,
            guest_ip: restored.slot.guest.to_string(),
            clone_net_index: clone_net.index,
            namespace: clone_net.netns.clone(),
            host_veth: clone_net.veth_host.clone(),
            status,
            exit_code,
            out_dir,
        })
    }
}

/// A per-member batch failure, keyed independently of task completion order.
struct CloneFailure {
    clone_net_index: u8,
    room_id: String,
    out_dir: Option<PathBuf>,
    error: RoomsError,
}

impl CloneFailure {
    fn new(clone_net_index: u8, room_id: &str, error: RoomsError) -> Self {
        Self {
            clone_net_index,
            room_id: room_id.to_owned(),
            out_dir: None,
            error,
        }
    }

    fn with_output(
        clone_net_index: u8,
        room_id: &str,
        out_dir: Option<PathBuf>,
        error: RoomsError,
    ) -> Self {
        Self {
            clone_net_index,
            room_id: room_id.to_owned(),
            out_dir,
            error,
        }
    }
}

#[derive(Debug, serde::Serialize)]
struct CloneFailureRecord {
    clone_net_index: u8,
    room_id: String,
    status: &'static str,
    error_kind: &'static str,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    out_dir: Option<PathBuf>,
}

impl From<CloneFailure> for CloneFailureRecord {
    fn from(failure: CloneFailure) -> Self {
        Self {
            clone_net_index: failure.clone_net_index,
            room_id: failure.room_id,
            status: "failed",
            error_kind: error_kind(&failure.error),
            message: failure.error.to_string(),
            out_dir: failure.out_dir,
        }
    }
}

struct CloneCommandBatchFailure {
    clones: Vec<CloneRecord>,
    failures: Vec<CloneFailureRecord>,
    task_failures: Vec<String>,
}

impl CloneCommandBatchFailure {
    fn new(
        mut clones: Vec<CloneRecord>,
        mut failures: Vec<CloneFailure>,
        mut task_failures: Vec<String>,
    ) -> Self {
        clones.sort_by_key(|record| record.clone_net_index);
        failures.sort_by_key(|failure| failure.clone_net_index);
        task_failures.sort();
        Self {
            clones,
            failures: failures.into_iter().map(CloneFailureRecord::from).collect(),
            task_failures,
        }
    }

    fn summary(&self) -> String {
        format!(
            "clone command batch failed for {} member(s) and {} task(s)",
            self.failures.len(),
            self.task_failures.len()
        )
    }

    fn into_cancellation(self, signal: CloneTermination) -> CloneCancellation {
        let mut cleanup_failures = self.task_failures;
        cleanup_failures.extend(self.failures.into_iter().map(|failure| {
            format!(
                "net {} room {} did not reach a clean cancellation boundary: {}",
                failure.clone_net_index, failure.room_id, failure.message
            )
        }));
        cleanup_failures.sort();
        CloneCancellation::with_clones(signal, self.clones, cleanup_failures)
    }
}

struct CloneCancellation {
    signal: CloneTermination,
    clones: Vec<CloneRecord>,
    cleanup_failures: Vec<String>,
}

impl CloneCancellation {
    const fn clean(signal: CloneTermination) -> Self {
        Self {
            signal,
            clones: Vec::new(),
            cleanup_failures: Vec::new(),
        }
    }

    fn with_clones(
        signal: CloneTermination,
        mut clones: Vec<CloneRecord>,
        cleanup_failures: Vec<String>,
    ) -> Self {
        for clone in &mut clones {
            if clone.exit_code == Some(signal.exit_code()) {
                clone.status = "cancelled";
            }
        }
        Self {
            signal,
            clones,
            cleanup_failures,
        }
    }

    const fn exit_code(&self) -> u8 {
        if self.cleanup_failures.is_empty() {
            return self.signal.exit_code();
        }
        2
    }
}

enum CloneRunError {
    Standard(RoomsError),
    CommandBatch(CloneCommandBatchFailure),
    Cancelled(CloneCancellation),
}

impl From<RoomsError> for CloneRunError {
    fn from(error: RoomsError) -> Self {
        Self::Standard(error)
    }
}

#[derive(serde::Serialize)]
struct CloneCommandFailureEnvelope<'a> {
    status: &'static str,
    error_kind: &'static str,
    clones: &'a [CloneRecord],
    failures: &'a [CloneFailureRecord],
    task_failures: &'a [String],
}

#[derive(serde::Serialize)]
struct CloneCancellationEnvelope<'a> {
    status: &'static str,
    error_kind: &'static str,
    signal: &'static str,
    exit_code: u8,
    cleanup_complete: bool,
    #[serde(skip_serializing_if = "clone_record_slice_is_empty")]
    clones: &'a [CloneRecord],
    #[serde(skip_serializing_if = "string_slice_is_empty")]
    cleanup_failures: &'a [String],
}

const fn clone_record_slice_is_empty(values: &[CloneRecord]) -> bool {
    values.is_empty()
}

const fn string_slice_is_empty(values: &[String]) -> bool {
    values.is_empty()
}

#[derive(serde::Serialize)]
struct CloneBatchEnvelope<'a> {
    clones: &'a [CloneRecord],
}

fn clone_command_failure_json(
    failure: &CloneCommandBatchFailure,
) -> Result<String, serde_json::Error> {
    serde_json::to_string(&CloneCommandFailureEnvelope {
        status: "failed",
        error_kind: "clone_batch",
        clones: &failure.clones,
        failures: &failure.failures,
        task_failures: &failure.task_failures,
    })
}

fn emit_clone_command_failure(failure: &CloneCommandBatchFailure) -> Result<(), RoomsError> {
    let line = clone_command_failure_json(failure)
        .map_err(|error| RoomsError::Internal(error.to_string()))?;
    #[allow(
        clippy::print_stdout,
        reason = "clone --json terminal failure record; stdout is the documented contract"
    )]
    {
        println!("{line}");
    }
    Ok(())
}

fn emit_clone_cancellation(cancellation: &CloneCancellation) -> Result<(), RoomsError> {
    let line = serde_json::to_string(&CloneCancellationEnvelope {
        status: "cancelled",
        error_kind: "cancelled",
        signal: cancellation.signal.name(),
        exit_code: cancellation.exit_code(),
        cleanup_complete: cancellation.cleanup_failures.is_empty(),
        clones: &cancellation.clones,
        cleanup_failures: &cancellation.cleanup_failures,
    })
    .map_err(|error| RoomsError::Internal(error.to_string()))?;
    #[allow(
        clippy::print_stdout,
        reason = "clone --json cancellation record; stdout is the documented contract"
    )]
    {
        println!("{line}");
    }
    Ok(())
}

fn collapse_clone_failures(stage: &str, mut failures: Vec<CloneFailure>) -> Option<RoomsError> {
    if failures.is_empty() {
        return None;
    }
    failures.sort_by_key(|failure| failure.clone_net_index);
    let evidence = failures
        .into_iter()
        .map(|failure| {
            format!(
                "net {} room {}: {}",
                failure.clone_net_index, failure.room_id, failure.error
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    Some(RoomsError::Internal(format!(
        "clone {stage} failed: {evidence}"
    )))
}

fn clone_join_error_kind(error: &tokio::task::JoinError) -> &'static str {
    if error.is_panic() {
        return "panicked";
    }
    if error.is_cancelled() {
        return "was cancelled";
    }
    "failed"
}

/// `rooms restore`: lease the frozen slot, resume the snapshot, gate on the
/// hygiene ack, then run the requested lifecycle mode. The `--json` error
/// record mirrors `run`.
async fn restore_room(args: RestoreArgs, config: &RoomsConfig) -> Result<u8, RoomsError> {
    let json = args.json;
    let result = restore_room_inner(args, config).await;
    if json {
        if let Err(err) = &result {
            emit_run_error_json(err);
        }
    }
    result
}

async fn restore_room_inner(args: RestoreArgs, config: &RoomsConfig) -> Result<u8, RoomsError> {
    info!(snapshot_dir = ?args.snapshot_dir, image = ?args.image, keep = args.keep, "rooms restore");
    // Exactly one lifecycle mode, refused before any effect: a bare restore
    // would immediately drop a successful guard, which is never what the
    // operator meant.
    if !args.keep && args.command.is_none() {
        return Err(RoomsError::Internal(
            "restore requires exactly one lifecycle mode: --keep or --command".to_owned(),
        ));
    }
    ensure_witness_available(args.witness)?;
    if let Err(remediation) = doctor::ensure_rooms_fwd_installed() {
        return Err(RoomsError::Internal(remediation));
    }
    let egress_plan = egress::resolve(&args.egress).map_err(RoomsError::Internal)?;
    warn_egress_without_witness(&egress_plan, args.witness);
    let key = key_path()?;
    let requested_room_id = firecracker::mint_room_id();

    let restored = rooms::restore_exec::restore(
        config,
        rooms::restore_exec::RestoreRequest {
            room_id: &requested_room_id,
            snapshot_dir: &args.snapshot_dir,
            image: &args.image,
            target_slot: args.slot,
            label: args.command.clone().or_else(|| Some("restore".to_owned())),
            keep: args.keep,
            witness: args.witness,
            egress: &egress_plan,
            secrets: args.secrets,
            out_dir: args.out_dir.as_deref(),
            ack_timeout: RESTORE_ACK_TIMEOUT,
            clone_net: None,
        },
    )
    .await
    .map_err(|e| RoomsError::Internal(e.to_string()))?;

    let rooms::restore_exec::Restored {
        mut vm,
        slot,
        room_id,
        snapshot_id,
        clone_net,
    } = restored;
    let network = network_config_for(&slot);
    emit_restored(&room_id, &snapshot_id, &slot, args.json);

    if args.keep {
        // Hand ownership to the persisted room. The guard dismisses (and never
        // owned the leased tap — see `attach_leased_slot`), so forgetting the
        // VM leaks nothing the lease-aware teardown won't reclaim: the intent
        // tombstone keeps the `@lease`, and `rooms kill` / `rooms gc` both
        // route a restored room through `restore_exec::finish_teardown`, which
        // returns the lease (never `slot::free`) once the room is torn down.
        vm.guard_mut().dismiss();
        std::mem::forget(vm);
        info!(room = %room_id, "--keep: restored room preserved; lease held until teardown");
        return Ok(0);
    }

    let lifecycle = Lifecycle::disabled();
    let env = PostBootEnv {
        network: &network,
        network_namespace: clone_net.as_ref().map(|net| net.netns.as_str()),
        key: &key,
        config,
        max_wall: args.max_wall,
        lifecycle: &lifecycle,
        egress: &egress_plan,
    };
    let command = args
        .command
        .clone()
        .ok_or_else(|| RoomsError::Internal("restore lifecycle lost its command".to_owned()))?;
    let action = Action::Exec(runner::Runner::Command(command));
    // Secrets arrived through the acked resume nudge, not the vsock one-shot,
    // so the workload gate has no delivery to await here.
    let outcome = post_boot(&env, &action, &mut vm, None, None).await;
    collect_run_artifacts(&env, &action, &slot, args.out_dir.as_deref(), &mut vm).await;

    if let Err(e) = vm.shutdown().await {
        warn!(error = %e, "restore shutdown reported an error");
    }
    // The reap-clean gate + lease return: any residue keeps the intent
    // tombstone for `rooms gc` and surfaces here as an error.
    rooms::restore_exec::finish_teardown(
        config,
        &room_id,
        &snapshot_id,
        slot.index,
        clone_net.as_ref().map(|net| net.index),
    )
    .map_err(|e| RoomsError::Internal(format!("restore teardown incomplete: {e}")))?;
    outcome
}

/// `rooms clone`: allocate distinct host-network identities, restore the whole
/// batch concurrently, then either preserve all rooms or execute in parallel.
async fn clone_rooms(args: CloneArgs, config: &RoomsConfig) -> Result<u8, RoomsError> {
    let json = args.json;
    match clone_rooms_inner(args, config).await {
        Ok(code) => Ok(code),
        Err(CloneRunError::Standard(error)) => {
            if json {
                emit_run_error_json(&error);
            }
            Err(error)
        }
        Err(CloneRunError::CommandBatch(failure)) => {
            if json {
                emit_clone_command_failure(&failure)?;
            }
            Err(RoomsError::Internal(failure.summary()))
        }
        Err(CloneRunError::Cancelled(cancellation)) => {
            let code = cancellation.exit_code();
            if json {
                emit_clone_cancellation(&cancellation)?;
            }
            warn!(
                signal = cancellation.signal.name(),
                cleanup_complete = cancellation.cleanup_failures.is_empty(),
                "clone batch cancelled"
            );
            Ok(code)
        }
    }
}

async fn clone_rooms_inner(args: CloneArgs, config: &RoomsConfig) -> Result<u8, CloneRunError> {
    info!(
        snapshot_dir = ?args.snapshot_dir,
        image = ?args.image,
        count = args.count,
        keep = args.command.is_none(),
        "rooms clone"
    );
    validate_json_emitted_path(args.out_dir.as_deref(), args.json, "clone --out")?;
    let mut cancellation = CloneSignalSource::arm()?;
    ensure_witness_available(args.witness)?;
    if let Err(remediation) = doctor::ensure_rooms_fwd_installed() {
        return Err(RoomsError::Internal(remediation).into());
    }
    let egress_plan = egress::resolve(&args.egress).map_err(RoomsError::Internal)?;
    warn_egress_without_witness(&egress_plan, args.witness);
    let key = key_path()?;
    let state_base = config.resolved_state_base().ok_or_else(|| {
        RoomsError::Internal("HOME unset; cannot locate the rooms state base".to_owned())
    })?;
    let cap = config.effective_max_pool(args.max_pool);
    validate_clone_capacity(args.count, cap)?;
    let me = clonenet::Claimer::current().ok_or_else(|| {
        RoomsError::Internal("cannot read this process's identity for clone claims".to_owned())
    })?;
    let (prepared, allocations) =
        prepare_clone_batch(&args, config, &state_base, cap, me, cancellation.receiver()).await?;
    let restored = restore_clone_batch(
        allocations,
        prepared,
        &args,
        &egress_plan,
        config,
        cancellation.receiver(),
    )
    .await;
    if let Some(signal) = restored.cancelled {
        return Err(cancel_clone_custody(restored.ready, signal, restored.failure).await);
    }
    if let Some(error) = restored.failure {
        return Err(cleanup_partial_restore(restored.ready, error).await.into());
    }
    let ready = restored.ready;
    // The barrier applies to BOTH lifecycle modes. A kept batch is not a
    // success until every namespaced SSH channel is usable; if any member
    // fails readiness, exact teardown consumes the complete batch below.
    match wait_clone_batch_ready(&ready, &key, config, cancellation.receiver()).await {
        Ok(()) => {}
        Err(CloneStageStop::Failed(error)) => {
            return Err(cleanup_partial_restore(ready, error).await.into());
        }
        Err(CloneStageStop::Cancelled(signal)) => {
            return Err(cancel_clone_custody(ready, signal, None).await);
        }
    }
    let Some(command) = args.command.clone() else {
        return finish_kept_clone_batch(ready, &mut cancellation, config, args.json).await;
    };
    let command_cancellation = cancellation.receiver();
    let command_result = run_clone_commands(
        ready,
        command,
        key,
        args.max_wall,
        egress_plan,
        command_cancellation,
    )
    .await;
    if let Some(signal) = cancellation.commit_terminal_handoff().await? {
        return Err(match command_result {
            Ok((_, clones)) => {
                CloneRunError::Cancelled(CloneCancellation::with_clones(signal, clones, Vec::new()))
            }
            Err(failure) => CloneRunError::Cancelled(failure.into_cancellation(signal)),
        });
    }
    let (code, records) = command_result.map_err(CloneRunError::CommandBatch)?;
    emit_clone_records(&records, args.json)?;
    Ok(code)
}

async fn finish_kept_clone_batch(
    ready: Vec<CloneCustody>,
    cancellation: &mut CloneSignalSource,
    config: &RoomsConfig,
    json: bool,
) -> Result<u8, CloneRunError> {
    if let Some(signal) = observed_clone_termination(&cancellation.receiver()) {
        return Err(cancel_clone_custody(ready, signal, None).await);
    }
    let mut records = preserve_clone_batch(ready)?;
    match cancellation.commit_terminal_handoff().await {
        Ok(Some(signal)) => {
            return Err(CloneRunError::Cancelled(cancel_preserved_clones(
                &mut records,
                config,
                signal,
            )));
        }
        Ok(None) => {}
        Err(error) => {
            let cleanup_failures = reap_preserved_clones(&records, config);
            if cleanup_failures.is_empty() {
                return Err(error.into());
            }
            return Err(RoomsError::Internal(format!(
                "{error}; kept-fleet rollback incomplete: {}",
                cleanup_failures.join("; ")
            ))
            .into());
        }
    }
    emit_clone_records(&records, json)?;
    Ok(0)
}

fn validate_clone_capacity(count: u8, cap: u8) -> Result<(), RoomsError> {
    if count <= cap {
        return Ok(());
    }
    Err(CloneNetError::PoolFull { cap }.into())
}

async fn prepare_clone_batch(
    args: &CloneArgs,
    config: &RoomsConfig,
    state_base: &Path,
    cap: u8,
    me: clonenet::Claimer,
    mut cancellation: tokio::sync::watch::Receiver<Option<CloneTermination>>,
) -> Result<
    (
        Arc<rooms::restore_exec::PreparedRestoreSource>,
        Vec<AllocatedClone>,
    ),
    CloneRunError,
> {
    if let Some(signal) = observed_clone_termination(&cancellation) {
        return Err(CloneRunError::Cancelled(CloneCancellation::clean(signal)));
    }
    // Source hashing/compatibility and host-network construction are both
    // request-wide blocking work. Own both tasks until they finish so a signal
    // can never detach a late allocation result containing live guards.
    let prepare_config = config.clone();
    let prepare_snapshot = args.snapshot_dir.clone();
    let prepare_image = args.image.clone();
    let prepared_task = tokio::task::spawn_blocking(move || {
        rooms::restore_exec::prepare_restore(&prepare_config, &prepare_snapshot, &prepare_image)
    });
    let allocation_state = state_base.to_path_buf();
    let count = args.count;
    let allocation_task =
        tokio::spawn(
            async move { allocate_clone_networks(&allocation_state, count, cap, me).await },
        );
    let setup = async move {
        let (prepared_join, allocations_join) = tokio::join!(prepared_task, allocation_task);
        let prepared = prepared_join
            .map_err(|error| {
                RoomsError::Internal(format!("clone source preparation task failed: {error}"))
            })?
            .map_err(|error| RoomsError::Internal(error.to_string()))?;
        let allocations = allocations_join.map_err(|error| {
            RoomsError::Internal(format!("clone allocation coordinator failed: {error}"))
        })??;
        Ok::<_, RoomsError>((Arc::new(prepared), allocations))
    };
    tokio::pin!(setup);
    tokio::select! {
        biased;
        signal = wait_for_clone_termination(&mut cancellation) => {
            // Blocking source hashing and host networking are not safely
            // abortable. Await both owned tasks, then drop any returned guards.
            let completed = setup.await;
            drop(completed);
            Err(CloneRunError::Cancelled(CloneCancellation::clean(signal)))
        }
        result = &mut setup => result.map_err(CloneRunError::from),
    }
}

async fn allocate_clone_networks(
    state_base: &Path,
    count: u8,
    cap: u8,
    me: clonenet::Claimer,
) -> Result<Vec<AllocatedClone>, RoomsError> {
    let owner_ids = (0..count)
        .map(|_| firecracker::mint_room_id())
        .collect::<Vec<_>>();
    let task_state = state_base.to_path_buf();
    let batch = tokio::task::spawn_blocking(move || {
        clonenet::allocate_batch(&task_state, &owner_ids, me, cap).map(|batch| {
            batch
                .into_iter()
                .map(|allocation| AllocatedClone {
                    room_id: allocation.owner_id,
                    network: Some(allocation.network),
                    state_base: task_state.clone(),
                })
                .collect::<Vec<_>>()
        })
    })
    .await
    .map_err(|error| {
        RoomsError::Internal(format!("clone network allocation task failed: {error}"))
    })?
    .map_err(|error| {
        error.pool_full_cap().map_or_else(
            || RoomsError::Internal(error.to_string()),
            |cap| RoomsError::CloneNet(CloneNetError::PoolFull { cap }),
        )
    })?;

    Ok(batch)
}

struct CloneRestoreBatch {
    ready: Vec<CloneCustody>,
    failure: Option<RoomsError>,
    cancelled: Option<CloneTermination>,
}

enum CloneStageStop {
    Failed(RoomsError),
    Cancelled(CloneTermination),
}

impl From<RoomsError> for CloneStageStop {
    fn from(error: RoomsError) -> Self {
        Self::Failed(error)
    }
}

async fn restore_clone_batch(
    allocations: Vec<AllocatedClone>,
    prepared: Arc<rooms::restore_exec::PreparedRestoreSource>,
    args: &CloneArgs,
    egress_plan: &egress::Plan,
    config: &RoomsConfig,
    cancellation: tokio::sync::watch::Receiver<Option<CloneTermination>>,
) -> CloneRestoreBatch {
    if let Some(signal) = observed_clone_termination(&cancellation) {
        drop(allocations);
        return CloneRestoreBatch {
            ready: Vec::new(),
            failure: None,
            cancelled: Some(signal),
        };
    }
    let mut tasks = tokio::task::JoinSet::new();
    let mut task_identities = HashMap::new();
    for mut allocation in allocations {
        let task_config = config.clone();
        let snapshot_dir = args.snapshot_dir.clone();
        let image = args.image.clone();
        let command = args.command.clone();
        let witness = args.witness;
        let plan = egress_plan.clone();
        let secrets = args
            .secrets
            .as_ref()
            .map(vsock::SecretsPayload::clone_bytes);
        let out_dir = clone_output_dir(args.out_dir.as_deref(), &allocation.room_id);
        let task_prepared = Arc::clone(&prepared);
        let task_identity = (
            allocation
                .network
                .as_ref()
                .map_or(u8::MAX, |network| network.index),
            allocation.room_id.clone(),
        );
        let handle = tasks.spawn(async move {
            let network = allocation
                .network()
                .map_err(|error| CloneFailure::new(0, &allocation.room_id, error))?
                .clone();
            let clone_net_index = network.index;
            let restored = rooms::restore_exec::restore_prepared(
                &task_config,
                rooms::restore_exec::RestoreRequest {
                    room_id: &allocation.room_id,
                    snapshot_dir: &snapshot_dir,
                    image: &image,
                    target_slot: None,
                    label: command.clone().or_else(|| Some("clone".to_owned())),
                    keep: command.is_none(),
                    witness,
                    egress: &plan,
                    secrets,
                    out_dir: out_dir.as_deref(),
                    ack_timeout: RESTORE_ACK_TIMEOUT,
                    clone_net: Some(network),
                },
                &task_prepared,
            )
            .await
            .map_err(|error| {
                CloneFailure::new(
                    clone_net_index,
                    &allocation.room_id,
                    RoomsError::Internal(error.to_string()),
                )
            })?;
            allocation.transfer();
            Ok::<CloneCustody, CloneFailure>(CloneCustody {
                restored: Some(restored),
                out_dir,
                config: task_config,
            })
        });
        task_identities.insert(handle.id(), task_identity);
    }
    collect_clone_restores(tasks, task_identities, cancellation).await
}

async fn collect_clone_restores(
    mut tasks: tokio::task::JoinSet<Result<CloneCustody, CloneFailure>>,
    mut task_identities: HashMap<tokio::task::Id, (u8, String)>,
    mut cancellation: tokio::sync::watch::Receiver<Option<CloneTermination>>,
) -> CloneRestoreBatch {
    let mut ready = Vec::new();
    let mut failures = Vec::new();
    let mut cancelled = None;
    while !tasks.is_empty() {
        let joined = if cancelled.is_some() {
            tasks.join_next_with_id().await
        } else {
            tokio::select! {
                biased;
                signal = wait_for_clone_termination(&mut cancellation) => {
                    cancelled = Some(signal);
                    // A restore future owns durable intent/lease custody. Do
                    // not abort it: await every worker to either return a
                    // Restored guard or run restore_prepared's normal abort
                    // path, then tear down all returned guards below.
                    continue;
                }
                result = tasks.join_next_with_id() => result,
            }
        };
        let Some(joined) = joined else {
            break;
        };
        match joined {
            Ok((id, Ok(custody))) => {
                task_identities.remove(&id);
                ready.push(custody);
            }
            Ok((id, Err(failure))) => {
                task_identities.remove(&id);
                failures.push(failure);
            }
            Err(error) => {
                let identity = task_identities
                    .remove(&error.id())
                    .unwrap_or_else(|| (u8::MAX, "unknown".to_owned()));
                if cancelled.is_some() && error.is_cancelled() {
                    continue;
                }
                failures.push(CloneFailure::new(
                    identity.0,
                    &identity.1,
                    RoomsError::Internal(format!(
                        "restore worker {}",
                        clone_join_error_kind(&error)
                    )),
                ));
            }
        }
    }
    ready.sort_by_key(|custody| custody.identity().map_or(u8::MAX, |identity| identity.0));
    let failure = collapse_clone_failures("restore", failures);
    CloneRestoreBatch {
        ready,
        failure,
        cancelled,
    }
}

async fn teardown_clone_custody(mut ready: Vec<CloneCustody>) -> Vec<String> {
    ready.sort_by_key(|custody| custody.identity().map_or(u8::MAX, |identity| identity.0));
    let mut cleanup_errors = Vec::new();
    for custody in ready {
        let identity = custody
            .identity()
            .unwrap_or_else(|_| (u8::MAX, "unknown".to_owned()));
        if let Err(error) = custody.teardown().await {
            cleanup_errors.push(format!("net {} room {}: {error}", identity.0, identity.1));
        }
    }
    cleanup_errors
}

async fn cleanup_partial_restore(ready: Vec<CloneCustody>, original: RoomsError) -> RoomsError {
    let cleanup_errors = teardown_clone_custody(ready).await;
    if cleanup_errors.is_empty() {
        return original;
    }
    RoomsError::Internal(format!(
        "{original}; partial clone cleanup incomplete: {}",
        cleanup_errors.join("; ")
    ))
}

async fn cancel_clone_custody(
    ready: Vec<CloneCustody>,
    signal: CloneTermination,
    prior_failure: Option<RoomsError>,
) -> CloneRunError {
    let mut cleanup_failures = teardown_clone_custody(ready).await;
    if let Some(error) = prior_failure {
        cleanup_failures.push(format!(
            "restore worker failed while cancellation was draining: {error}"
        ));
    }
    CloneRunError::Cancelled(CloneCancellation {
        signal,
        clones: Vec::new(),
        cleanup_failures,
    })
}

fn clone_output_dir(root: Option<&Path>, room_id: &str) -> Option<PathBuf> {
    root.map(|dir| dir.join(room_id))
}

fn preserve_clone_batch(ready: Vec<CloneCustody>) -> Result<Vec<CloneRecord>, RoomsError> {
    let mut records = ready
        .iter()
        .map(|custody| custody.record("kept", None))
        .collect::<Result<Vec<_>, _>>()?;
    for custody in ready {
        custody.preserve()?;
    }
    records.sort_by_key(|record| record.clone_net_index);
    Ok(records)
}

fn reap_preserved_clones(records: &[CloneRecord], config: &RoomsConfig) -> Vec<String> {
    let mut failures = Vec::new();
    for record in records {
        match registry::kill(config, &record.room_id) {
            Ok(report) if report.outcomes.is_empty() => {}
            Ok(report) => {
                for outcome in report.outcomes {
                    if outcome.disposition == registry::KillDisposition::Killed {
                        continue;
                    }
                    failures.push(format!(
                        "net {} room {} rollback was {}: {}",
                        record.clone_net_index,
                        record.room_id,
                        outcome.disposition.exit_code(),
                        outcome.reason
                    ));
                }
            }
            Err(error) => failures.push(format!(
                "net {} room {} rollback failed: {error}",
                record.clone_net_index, record.room_id
            )),
        }
    }
    failures.sort();
    failures
}

fn cancel_preserved_clones(
    records: &mut [CloneRecord],
    config: &RoomsConfig,
    signal: CloneTermination,
) -> CloneCancellation {
    let cleanup_failures = reap_preserved_clones(records, config);
    for record in &mut *records {
        record.status = "cancelled";
        record.exit_code = Some(signal.exit_code());
    }
    CloneCancellation::with_clones(signal, records.to_vec(), cleanup_failures)
}

async fn wait_clone_batch_ready(
    ready: &[CloneCustody],
    key: &Path,
    config: &RoomsConfig,
    mut cancellation: tokio::sync::watch::Receiver<Option<CloneTermination>>,
) -> Result<(), CloneStageStop> {
    if let Some(signal) = observed_clone_termination(&cancellation) {
        return Err(CloneStageStop::Cancelled(signal));
    }
    let mut tasks = tokio::task::JoinSet::new();
    let mut task_identities = HashMap::new();
    for custody in ready {
        let restored = custody.restored()?;
        let room_id = restored.room_id.clone();
        let guest_ip = restored.slot.guest.to_string();
        let clone_net = restored.clone_net.as_ref().ok_or_else(|| {
            RoomsError::Internal("clone restore returned no clone network".to_owned())
        })?;
        let clone_net_index = clone_net.index;
        let namespace = clone_net.netns.clone();
        let task_key = key.to_path_buf();
        let mut task_config = config.clone();
        task_config.guest_reach_poll_interval = CLONE_SSH_POLL_INTERVAL;
        let task_identity = (clone_net_index, room_id.clone());
        let handle = tasks.spawn(async move {
            runner::wait_for_ssh(
                runner::GuestTarget::new(&guest_ip, Some(&namespace)),
                &task_key,
                &task_config,
            )
            .await
            .map_err(|error| {
                CloneFailure::new(clone_net_index, &room_id, RoomsError::Firecracker(error))
            })
        });
        task_identities.insert(handle.id(), task_identity);
    }
    let mut failures = Vec::new();
    let mut cancelled = None;
    while !tasks.is_empty() {
        let joined = if cancelled.is_some() {
            tasks.join_next_with_id().await
        } else {
            tokio::select! {
                biased;
                signal = wait_for_clone_termination(&mut cancellation) => {
                    cancelled = Some(signal);
                    tasks.abort_all();
                    continue;
                }
                result = tasks.join_next_with_id() => result,
            }
        };
        let Some(joined) = joined else {
            break;
        };
        match joined {
            Ok((id, Ok(()))) => {
                task_identities.remove(&id);
            }
            Ok((id, Err(failure))) => {
                task_identities.remove(&id);
                failures.push(failure);
            }
            Err(error) => {
                let identity = task_identities
                    .remove(&error.id())
                    .unwrap_or_else(|| (u8::MAX, "unknown".to_owned()));
                if cancelled.is_some() && error.is_cancelled() {
                    continue;
                }
                failures.push(CloneFailure::new(
                    identity.0,
                    &identity.1,
                    RoomsError::Internal(format!(
                        "readiness worker {}",
                        clone_join_error_kind(&error)
                    )),
                ));
            }
        }
    }
    if let Some(signal) = cancelled {
        return Err(CloneStageStop::Cancelled(signal));
    }
    let error = collapse_clone_failures("readiness", failures);
    error.map_or(Ok(()), |error| Err(CloneStageStop::Failed(error)))
}

struct CloneCommandOutcome {
    record: CloneRecord,
    exit_code: u8,
}

async fn run_clone_commands(
    ready: Vec<CloneCustody>,
    command: String,
    key: PathBuf,
    max_wall: Option<Duration>,
    egress_plan: egress::Plan,
    cancellation: tokio::sync::watch::Receiver<Option<CloneTermination>>,
) -> Result<(u8, Vec<CloneRecord>), CloneCommandBatchFailure> {
    let mut tasks = tokio::task::JoinSet::new();
    let mut task_identities = HashMap::new();
    for custody in ready {
        let task_identity = custody
            .identity()
            .unwrap_or_else(|_| (u8::MAX, "unknown".to_owned()));
        let handle = tasks.spawn(execute_clone_command(
            custody,
            command.clone(),
            key.clone(),
            max_wall,
            egress_plan.clone(),
            cancellation.clone(),
        ));
        task_identities.insert(handle.id(), task_identity);
    }
    let mut outcomes = Vec::new();
    let mut failures = Vec::new();
    let mut join_failures = Vec::new();
    while let Some(joined) = tasks.join_next_with_id().await {
        match joined {
            Ok((id, Ok(outcome))) => {
                task_identities.remove(&id);
                outcomes.push(outcome);
            }
            Ok((id, Err(failure))) => {
                task_identities.remove(&id);
                failures.push(failure);
            }
            Err(error) => {
                let identity = task_identities
                    .remove(&error.id())
                    .unwrap_or_else(|| (u8::MAX, "unknown".to_owned()));
                join_failures.push(format!(
                    "net {} room {}: command worker {}",
                    identity.0,
                    identity.1,
                    clone_join_error_kind(&error)
                ));
            }
        }
    }
    outcomes.sort_by_key(|outcome| outcome.record.clone_net_index);
    let code = outcomes
        .iter()
        .find(|outcome| outcome.exit_code != 0)
        .map_or(0, |outcome| outcome.exit_code);
    let records = outcomes.into_iter().map(|outcome| outcome.record).collect();
    if !failures.is_empty() || !join_failures.is_empty() {
        return Err(CloneCommandBatchFailure::new(
            records,
            failures,
            join_failures,
        ));
    }
    Ok((code, records))
}

async fn execute_clone_command(
    custody: CloneCustody,
    command: String,
    key: PathBuf,
    max_wall: Option<Duration>,
    egress_plan: egress::Plan,
    cancellation: tokio::sync::watch::Receiver<Option<CloneTermination>>,
) -> Result<CloneCommandOutcome, CloneFailure> {
    let out_dir = custody.out_dir.clone();
    let restored = custody
        .restored()
        .map_err(|error| CloneFailure::with_output(0, "unknown", out_dir.clone(), error))?;
    let room_id = restored.room_id.clone();
    let clone_net_index = restored.clone_net.as_ref().map_or(0, |net| net.index);
    execute_clone_command_inner(custody, command, key, max_wall, egress_plan, cancellation)
        .await
        .map_err(|error| CloneFailure::with_output(clone_net_index, &room_id, out_dir, error))
}

async fn execute_clone_command_inner(
    mut custody: CloneCustody,
    command: String,
    key: PathBuf,
    max_wall: Option<Duration>,
    egress_plan: egress::Plan,
    cancellation: tokio::sync::watch::Receiver<Option<CloneTermination>>,
) -> Result<CloneCommandOutcome, RoomsError> {
    let cancellation_observer = cancellation.clone();
    let config = custody.config.clone();
    let out_dir = custody.out_dir.clone();
    let lifecycle = Lifecycle::disabled();
    let action = Action::Exec(runner::Runner::Command(command));
    let outcome = {
        let restored = custody.restored_mut()?;
        let network = network_config_for(&restored.slot);
        let env = PostBootEnv {
            network: &network,
            network_namespace: restored.clone_net.as_ref().map(|net| net.netns.as_str()),
            key: &key,
            config: &config,
            max_wall,
            lifecycle: &lifecycle,
            egress: &egress_plan,
        };
        let outcome = post_boot(&env, &action, &mut restored.vm, None, Some(cancellation)).await;
        collect_run_artifacts(
            &env,
            &action,
            &restored.slot,
            out_dir.as_deref(),
            &mut restored.vm,
        )
        .await;
        outcome
    };
    let record = match &outcome {
        Ok(code) => {
            let status = if observed_clone_termination(&cancellation_observer).is_some() {
                "cancelled"
            } else {
                "exited"
            };
            Some(custody.record(status, Some(*code))?)
        }
        Err(_) => None,
    };
    let teardown = custody.teardown().await;
    teardown?;
    let exit_code = outcome?;
    let record = record.ok_or_else(|| {
        RoomsError::Internal("clone command completed without a result record".to_owned())
    })?;
    Ok(CloneCommandOutcome { record, exit_code })
}

async fn teardown_restored_clone(
    restored: rooms::restore_exec::Restored,
    config: &RoomsConfig,
) -> Result<(), RoomsError> {
    let rooms::restore_exec::Restored {
        vm,
        slot,
        room_id,
        snapshot_id,
        clone_net,
    } = restored;
    let finalizer = CloneTeardownFinalizer {
        config: config.clone(),
        room_id: room_id.clone(),
        snapshot_id,
        slot_index: slot.index,
        clone_net_index: clone_net.as_ref().map(|net| net.index),
        armed: true,
    };
    let shutdown = vm.shutdown();
    if let Err(error) = shutdown.await {
        warn!(room = %room_id, %error, "clone shutdown reported an error");
    }
    finalizer.finish()
}

fn clone_records_json(records: &[CloneRecord]) -> Result<String, serde_json::Error> {
    serde_json::to_string(&CloneBatchEnvelope { clones: records })
}

fn emit_clone_records(records: &[CloneRecord], json: bool) -> Result<(), RoomsError> {
    if json {
        let line =
            clone_records_json(records).map_err(|error| RoomsError::Internal(error.to_string()))?;
        #[allow(
            clippy::print_stdout,
            reason = "clone --json batch record; stdout is the documented contract"
        )]
        {
            println!("{line}");
        }
        return Ok(());
    }
    for record in records {
        #[allow(
            clippy::print_stdout,
            reason = "clone human summary; stdout is the documented contract"
        )]
        {
            println!(
                "clone: room {} from {} in {} (guest {}, net {}, host {}) {}{}",
                record.room_id,
                record.snapshot_id,
                record.namespace,
                record.guest_ip,
                record.clone_net_index,
                record.host_veth,
                record.status,
                record
                    .exit_code
                    .map_or_else(String::new, |code| format!(" {code}"))
            );
        }
    }
    Ok(())
}

/// Report a restored room — a human line, or a `--json` record.
fn emit_restored(room_id: &str, snapshot_id: &str, slot: &room::Slot, json: bool) {
    if json {
        let record = serde_json::json!({
            "room_id": room_id,
            "snapshot_id": snapshot_id,
            "slot": slot.index,
            "guest_ip": slot.guest.to_string(),
        });
        match serde_json::to_string(&record) {
            Ok(line) => {
                #[allow(
                    clippy::print_stdout,
                    reason = "restore --json record; stdout is the documented contract"
                )]
                {
                    println!("{line}");
                }
            }
            Err(e) => warn!(error = %e, "failed to serialize restore --json record"),
        }
        return;
    }
    #[allow(
        clippy::print_stdout,
        reason = "restore human summary; stdout is the documented contract"
    )]
    {
        println!(
            "restored: room {room_id} from snapshot {snapshot_id} on slot {} (guest {})",
            slot.index, slot.guest
        );
    }
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
        collect_and_record(env.guest_target(), env.key, action, out_dir, env.lifecycle).await;
    }
    let witnessed = match vm.take_witness() {
        Some(capture) => Some(summarize_witness(capture, slot, env).await),
        None => None,
    };
    let (Some(w), Some(out_dir)) = (&witnessed, out_dir) else {
        return;
    };
    persist_witness(w, out_dir).await;
}

/// A finalized egress witness: the summary plus the raw pcap bytes, held
/// between summarizing (while the staged temp file still exists) and persisting
/// into `--out`. The raw bytes are carried here because the staged pcap is
/// removed right after the read — it lives in the system temp dir (an
/// `AppArmor`-permitted path), not the room dir that teardown reaps.
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
    env: &PostBootEnv<'_>,
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
    // The pcap is staged under the system temp dir (an AppArmor-permitted path),
    // not the room dir, so room teardown won't reap it — remove it now that its
    // bytes are held for persistence into `--out`. Best-effort: a leftover temp
    // pcap is harmless, and `--witness` conflicts with `--keep`, so this is the
    // one summarize path.
    let _ = tokio::fs::remove_file(&pcap_path).await;
    let local = artifacts::GatewayLocal {
        gateway: slot.gateway,
        guest: slot.guest,
    };
    let complete = outcome.complete && readable;
    let mut summary = artifacts::summarize_pcap(&raw, &tap, local, complete);
    // Fold the room's egress policy into the summary: the recorded policy, its
    // permitted set, and the blocked attempts (pcap ∖ permitted). A `none` run
    // with no destinations is the proof-of-absence receipt.
    let (policy, permitted) = egress_record(env.egress);
    artifacts::record_egress(&mut summary, policy, permitted);
    env.lifecycle.emit(&Event::WitnessDone {
        destinations: summary.destinations.len(),
        complete: summary.capture_complete,
    });
    Witnessed { summary, raw }
}

/// Map a resolved [`egress::Plan`] onto the witness record's policy tag and its
/// permitted destination set.
fn egress_record(plan: &egress::Plan) -> (artifacts::EgressPolicy, Vec<String>) {
    match plan {
        egress::Plan::Observe => (artifacts::EgressPolicy::Observe, Vec::new()),
        egress::Plan::None => (artifacts::EgressPolicy::None, Vec::new()),
        egress::Plan::Allowlist(permitted) => {
            (artifacts::EgressPolicy::Allowlist, permitted.clone())
        }
    }
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
    target: runner::GuestTarget<'_>,
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
    let collect = collect_to_host(target, key, out_dir);
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
async fn collect_to_host(
    target: runner::GuestTarget<'_>,
    key: &Path,
    out_dir: &Path,
) -> Result<(), String> {
    let primary = runner::collect_out_to_host(target, key, out_dir).await;
    match runner::collect_changeset_to_host(target, key, out_dir).await {
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
    network_namespace: Option<&'a str>,
    key: &'a Path,
    config: &'a RoomsConfig,
    max_wall: Option<Duration>,
    lifecycle: &'a Lifecycle,
    egress: &'a egress::Plan,
}

impl PostBootEnv<'_> {
    fn guest_target(&self) -> runner::GuestTarget<'_> {
        runner::GuestTarget::new(&self.network.guest_ip, self.network_namespace)
    }
}

async fn post_boot(
    env: &PostBootEnv<'_>,
    action: &Action,
    vm: &mut firecracker::BootedVm,
    secrets: Option<vsock::Delivery>,
    clone_cancellation: Option<tokio::sync::watch::Receiver<Option<CloneTermination>>>,
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
        Action::Exec(run) => exec_workload(env, run, secrets, clone_cancellation).await,
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
/// exit code (130 / 143 / 124) is decided by the arm, not the workload.
#[derive(Debug)]
enum ExecRace {
    /// The workload ran to completion; carries its own result.
    Completed(Result<u8, RoomsError>),
    /// A termination signal fired first; carries its shell-compatible code.
    Cancelled(u8),
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
    race_workload_with_clone_cancellation(work, max_wall, None).await
}

async fn race_workload_with_clone_cancellation<W>(
    work: W,
    max_wall: Option<Duration>,
    clone_cancellation: Option<tokio::sync::watch::Receiver<Option<CloneTermination>>>,
) -> ExecRace
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
    let clone_signal = async move {
        let Some(mut receiver) = clone_cancellation else {
            return std::future::pending::<CloneTermination>().await;
        };
        wait_for_clone_termination(&mut receiver).await
    };
    tokio::pin!(clone_signal);
    tokio::select! {
        biased;
        res = &mut work => ExecRace::Completed(res),
        _ = tokio::signal::ctrl_c() => ExecRace::Cancelled(130),
        signal = &mut clone_signal => ExecRace::Cancelled(signal.exit_code()),
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
    clone_cancellation: Option<tokio::sync::watch::Receiver<Option<CloneTermination>>>,
) -> Result<u8, RoomsError> {
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
    let raced = match clone_cancellation {
        Some(cancellation) => {
            race_workload_with_clone_cancellation(work, env.max_wall, Some(cancellation)).await
        }
        None => race_workload(work, env.max_wall).await,
    };
    match raced {
        ExecRace::Completed(res) => res,
        // The abort outcome is decided the instant the arm fires; the stream
        // records it BEFORE the best-effort guest write below, which can stall
        // up to its grace bound on the very guest that just went unresponsive.
        ExecRace::Cancelled(exit_code) => {
            info!(
                "termination signal received during exec setup or run; aborting and shutting down"
            );
            lifecycle.emit(&Event::WorkloadExited {
                exit_code: i32::from(exit_code),
                status: WorkloadStatus::Cancelled,
            });
            record_aborted_run(
                env.guest_target(),
                env.key,
                i32::from(exit_code),
                RunStatus::Cancelled,
                started_at,
                run.command_argv(),
            )
            .await;
            Ok(exit_code)
        }
        ExecRace::TimedOut => {
            warn!(max_wall = ?env.max_wall, "max wall-clock cap reached during exec; aborting and shutting down");
            lifecycle.emit(&Event::WorkloadExited {
                exit_code: 124,
                status: WorkloadStatus::TimedOut,
            });
            record_aborted_run(
                env.guest_target(),
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
    let outcome = match runner::exec(env.guest_target(), env.key, run).await {
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
        env.guest_target(),
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
                env.guest_target(),
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
    target: runner::GuestTarget<'_>,
    key: &Path,
    exit_code: i32,
    status: RunStatus,
    started_at: DateTime<Utc>,
    command_argv: Vec<String>,
) {
    let write = async move {
        if let Err(err) = runner::ensure_guest_artifact_skeleton(target, key).await {
            warn!(error = %err, "failed to create aborted-run artifact skeleton");
        }
        let result = ResultJson::from_exec(exit_code, status, started_at, Utc::now(), command_argv);
        if let Err(err) = runner::write_guest_result_json(target, key, &result).await {
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

    #[cfg(unix)]
    use std::os::unix::ffi::OsStringExt as _;

    use super::{
        changeset_exit_code, clone_command_failure_json, clone_output_dir, clone_records_json,
        collapse_clone_failures, diff_changeset, exit_code_for_error, harvest_secrets,
        humanize_secs, parse_clone_count, parse_max_pool, parse_max_wall, race_workload,
        race_workload_with_clone_cancellation, resolve_action, run_error_record, truncate_label,
        valid_secret_name, validate_clone_capacity, validate_json_emitted_path, Cli,
        CloneCancellation, CloneCommandBatchFailure, CloneFailure, CloneNetError, CloneRecord,
        CloneSignalSource, CloneTermination, Command, ExecRace, RoomsError, RunArgs, RunnerKind,
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
            ExecRace::Cancelled(_) => panic!("nothing sent Ctrl-C; the race must not cancel"),
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
    fn clone_count_accepts_one_through_eight_only() {
        assert_eq!(parse_clone_count("1"), Ok(1));
        assert_eq!(parse_clone_count("8"), Ok(8));
        for bad in ["0", "9", "abc", ""] {
            assert!(
                parse_clone_count(bad).is_err(),
                "clone count {bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn clone_defaults_to_keep_and_parses_the_required_surface() {
        let cli = Cli::try_parse_from([
            "rooms",
            "clone",
            "/snap",
            "--image",
            "/image",
            "-n",
            "8",
            "--egress",
            "none",
            "--max-pool",
            "8",
            "--json",
        ])
        .expect("clone keep batch parses");
        match cli.command {
            Command::Clone {
                snapshot_dir,
                image,
                count,
                command,
                max_pool,
                json,
                ..
            } => {
                assert_eq!(snapshot_dir, PathBuf::from("/snap"));
                assert_eq!(image, PathBuf::from("/image"));
                assert_eq!(count, 8);
                assert!(command.is_none(), "no command is the keep lifecycle");
                assert_eq!(max_pool, Some(8));
                assert!(json);
            }
            other => panic!("expected Clone, got {other:?}"),
        }
    }

    #[test]
    fn clone_command_accepts_collection_witness_and_wall_cap() {
        let cli = Cli::try_parse_from([
            "rooms",
            "clone",
            "/snap",
            "--image",
            "/image",
            "-n",
            "4",
            "--command",
            "hostname",
            "--out",
            "/out",
            "--witness",
            "--max-wall",
            "30s",
        ])
        .expect("clone command batch parses");
        match cli.command {
            Command::Clone {
                count,
                command,
                out_dir,
                witness,
                max_wall,
                ..
            } => {
                assert_eq!(count, 4);
                assert_eq!(command.as_deref(), Some("hostname"));
                assert_eq!(out_dir, Some(PathBuf::from("/out")));
                assert!(witness);
                assert_eq!(max_wall, Some(Duration::from_secs(30)));
            }
            other => panic!("expected Clone, got {other:?}"),
        }
    }

    #[test]
    fn clone_collection_options_require_command_mode() {
        for flag in ["--out", "--max-wall"] {
            let value = if flag == "--out" { "/out" } else { "30s" };
            let parsed = Cli::try_parse_from([
                "rooms", "clone", "/snap", "--image", "/image", "-n", "2", flag, value,
            ]);
            assert!(parsed.is_err(), "{flag} without --command must fail");
        }
        assert!(
            Cli::try_parse_from([
                "rooms",
                "clone",
                "/snap",
                "--image",
                "/image",
                "-n",
                "2",
                "--witness"
            ])
            .is_err(),
            "--witness without --out must fail"
        );
    }

    #[test]
    fn clone_capacity_is_refused_before_partial_allocation() {
        assert!(validate_clone_capacity(4, 4).is_ok());
        match validate_clone_capacity(5, 4) {
            Err(RoomsError::CloneNet(CloneNetError::PoolFull { cap })) => assert_eq!(cap, 4),
            other => panic!("expected clone-network PoolFull, got {other:?}"),
        }
    }

    #[test]
    fn clone_outputs_are_partitioned_by_room_identity() {
        let root = Path::new("/out");
        assert_eq!(
            clone_output_dir(Some(root), "room-a"),
            Some(PathBuf::from("/out/room-a"))
        );
        assert_eq!(clone_output_dir(None, "room-a"), None);
    }

    #[cfg(unix)]
    #[test]
    fn json_emitted_output_path_must_be_utf8_before_effects() {
        let invalid = PathBuf::from(std::ffi::OsString::from_vec(vec![b'/', b'o', 0xff]));
        let error = validate_json_emitted_path(Some(&invalid), true, "clone --out")
            .expect_err("non-UTF-8 JSON output path must fail admission");
        assert!(
            error
                .to_string()
                .contains("clone --out must be valid UTF-8"),
            "error should identify the preflight boundary: {error}"
        );
        assert!(
            validate_json_emitted_path(Some(&invalid), false, "clone --out").is_ok(),
            "a human-only filesystem path is not a JSON value"
        );
    }

    #[cfg(unix)]
    #[test]
    fn clone_batch_json_propagates_non_utf8_instead_of_panicking() {
        let invalid = PathBuf::from(std::ffi::OsString::from_vec(vec![b'/', b'o', 0xff]));
        let record = CloneRecord {
            room_id: "room-one".to_owned(),
            snapshot_id: "snapshot".to_owned(),
            slot: 4,
            guest_ip: "172.16.0.18".to_owned(),
            clone_net_index: 1,
            namespace: "rooms-c1".to_owned(),
            host_veth: "veth-h1".to_owned(),
            status: "exited",
            exit_code: Some(0),
            out_dir: Some(invalid),
        };
        assert!(
            clone_records_json(&[record]).is_err(),
            "typed serialization must return the invalid-path error"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn clone_sigterm_uses_the_existing_command_cancellation_arm() {
        let (sender, receiver) = tokio::sync::watch::channel(None);
        sender
            .send(Some(CloneTermination::Terminate))
            .expect("command worker still owns the receiver");
        let work = std::future::pending::<Result<u8, RoomsError>>();
        assert!(matches!(
            race_workload_with_clone_cancellation(work, None, Some(receiver)).await,
            ExecRace::Cancelled(143)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn clone_signal_exit_codes_distinguish_interrupt_and_terminate() {
        assert_eq!(
            CloneCancellation::clean(CloneTermination::Interrupt).exit_code(),
            130
        );
        assert_eq!(
            CloneCancellation::clean(CloneTermination::Terminate).exit_code(),
            143
        );
        let incomplete = CloneCancellation {
            signal: CloneTermination::Terminate,
            clones: Vec::new(),
            cleanup_failures: vec!["residue".to_owned()],
        };
        assert_eq!(incomplete.exit_code(), 2);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn clone_terminal_handoff_is_acknowledged_by_the_signal_owner() {
        let mut source = CloneSignalSource::arm().expect("register signal owner");
        assert_eq!(source.commit_terminal_handoff().await.unwrap(), None);
    }

    #[test]
    fn clone_failures_are_reported_in_network_order_not_completion_order() {
        let error = collapse_clone_failures(
            "restore",
            vec![
                CloneFailure::new(7, "room-seven", RoomsError::Internal("late".to_owned())),
                CloneFailure::new(2, "room-two", RoomsError::Internal("early".to_owned())),
            ],
        )
        .expect("failures collapse");
        let message = error.to_string();
        let two = message.find("net 2").expect("net 2 evidence");
        let seven = message.find("net 7").expect("net 7 evidence");
        assert!(two < seven, "failure evidence must be canonical: {message}");
    }

    #[test]
    fn clone_command_failure_json_retains_successes_outputs_and_all_failures() {
        let success = CloneRecord {
            room_id: "room-three".to_owned(),
            snapshot_id: "snapshot".to_owned(),
            slot: 4,
            guest_ip: "172.16.0.18".to_owned(),
            clone_net_index: 3,
            namespace: "rooms-c3".to_owned(),
            host_veth: "veth-h3".to_owned(),
            status: "exited",
            exit_code: Some(0),
            out_dir: Some(PathBuf::from("/out/room-three")),
        };
        let batch = CloneCommandBatchFailure::new(
            vec![success],
            vec![
                CloneFailure::with_output(
                    7,
                    "room-seven",
                    Some(PathBuf::from("/out/room-seven")),
                    RoomsError::Internal("late".to_owned()),
                ),
                CloneFailure::with_output(
                    2,
                    "room-two",
                    Some(PathBuf::from("/out/room-two")),
                    RoomsError::Internal("early".to_owned()),
                ),
            ],
            vec!["task-z".to_owned(), "task-a".to_owned()],
        );
        let raw = clone_command_failure_json(&batch).expect("serialize batch failure");
        let json: serde_json::Value = serde_json::from_str(&raw).expect("parse batch failure");
        assert_eq!(json["status"], "failed");
        assert_eq!(json["error_kind"], "clone_batch");
        assert_eq!(json["clones"][0]["room_id"], "room-three");
        assert_eq!(json["clones"][0]["out_dir"], "/out/room-three");
        assert_eq!(json["failures"][0]["clone_net_index"], 2);
        assert_eq!(json["failures"][0]["out_dir"], "/out/room-two");
        assert_eq!(json["failures"][1]["clone_net_index"], 7);
        assert_eq!(json["failures"][1]["out_dir"], "/out/room-seven");
        assert_eq!(json["task_failures"][0], "task-a");
        assert_eq!(json["task_failures"][1], "task-z");
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
        assert_eq!(
            exit_code_for_error(&RoomsError::CloneNet(CloneNetError::PoolFull { cap: 8 })),
            4,
            "clone-network pool full keeps the same machine contract"
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

        let clone_record =
            run_error_record(&RoomsError::CloneNet(CloneNetError::PoolFull { cap: 6 }));
        assert_eq!(clone_record.error_kind, "pool_full");
        assert_eq!(clone_record.cap, Some(6));
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
            egress: crate::egress::Policy::Observe,
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
    fn snapshot_verbs_parse_output_json_and_optional_recovery_id() {
        let id = "01abcdefghijklmnopqrstuvwx";
        let create =
            Cli::try_parse_from(["rooms", "snapshot", id, "--out", "/tmp/snapshot", "--json"])
                .expect("snapshot parses");
        match create.command {
            Command::Snapshot { id: got, out, json } => {
                assert_eq!(got, id);
                assert_eq!(out, Some(PathBuf::from("/tmp/snapshot")));
                assert!(json);
            }
            other => panic!("expected Snapshot, got {other:?}"),
        }
        let list = Cli::try_parse_from(["rooms", "snapshot-recover", "--json"])
            .expect("recovery list parses");
        match list.command {
            Command::SnapshotRecover { id, json } => {
                assert!(id.is_none());
                assert!(json);
            }
            other => panic!("expected SnapshotRecover, got {other:?}"),
        }
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
