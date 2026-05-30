//! `rooms` — disposable Firecracker microVMs with specified deps.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use chrono::Utc;
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
                },
                &config,
            )
            .await
        }
        Command::Collect { from } => collect_artifacts(from).await,
        Command::Doctor { image, json } => run_doctor_cmd(image.as_deref(), json, &config),
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
            let status = if check.ok { "ok" } else { "FAIL" };
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
    let mut vm = firecracker::boot(&kernel, &args.image, Some(&network), config).await?;

    if args.keep {
        vm.guard_mut().set_suppress_cleanup(true);
    }

    let outcome = post_boot(&network, &key, &action, &mut vm, config).await;
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
            Ok(Action::Exec(runner::Runner::Cursor(
                runner::CursorRequest {
                    repo_url,
                    task_md,
                    meta: runner::CursorMeta { base_sha, model_id },
                },
            )))
        }
        RunnerKind::Command => {
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
            // the success path; this outer one only surfaces in the cancel
            // branch below, where the guest command may never have begun.
            let started_at = Utc::now();
            tokio::pin!(work);
            tokio::select! {
                res = &mut work => res,
                _ = tokio::signal::ctrl_c() => {
                    info!("ctrl-c received during exec setup or run; aborting and shutting down");
                    // Ensure the artifact dir + empty log files exist before
                    // writing result.json so `rooms collect` validation still
                    // passes for a cancelled run.
                    if let Err(err) = runner::ensure_guest_artifact_skeleton(&guest_ip, key).await {
                        warn!(error = %err, "failed to create cancelled-run artifact skeleton");
                    }
                    let result = ResultJson::from_exec(
                        130,
                        RunStatus::Cancelled,
                        started_at,
                        Utc::now(),
                        run.command_argv(),
                    );
                    if let Err(err) = runner::write_guest_result_json(&guest_ip, key, &result).await {
                        warn!(error = %err, "failed to write cancelled result.json");
                    }
                    Ok(130)
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

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        reason = "test module"
    )]

    use super::{Cli, Command, RunnerKind};
    use clap::{CommandFactory, Parser};

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
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
}
