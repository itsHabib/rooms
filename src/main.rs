//! `rooms` — disposable Firecracker microVMs with specified deps.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use rooms::{config::RoomsConfig, doctor, error::RoomsError, firecracker, rootfs, runner};
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
        /// Mutually exclusive with `--command`. Suppresses cleanup for debugging.
        #[arg(long, conflicts_with = "command")]
        keep: bool,
        /// Run a single command in the guest via SSH, capture its stdout/stderr on
        /// host stdout/stderr, propagate its exit code, then shut down.
        #[arg(long, conflicts_with = "keep", value_parser = non_empty_command)]
        command: Option<String>,
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
        } => run_room(image, keep, command, &config).await,
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

async fn run_room(
    image: PathBuf,
    keep: bool,
    command: Option<String>,
    config: &RoomsConfig,
) -> Result<u8, RoomsError> {
    info!(?image, keep, command = ?command.as_deref(), "rooms run");

    let kernel = image
        .parent()
        .map(|p| p.join("vmlinux.bin"))
        .ok_or_else(|| {
            RoomsError::Internal(format!(
                "--image has no parent directory: {}",
                image.display()
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
    let mut vm = firecracker::boot(&kernel, &image, Some(&network), config).await?;

    if keep {
        vm.guard_mut().set_suppress_cleanup(true);
    }

    let outcome = post_boot(&network, &key, keep, command, &mut vm, config).await;
    if keep {
        vm.guard_mut().dismiss();
        // Prevent kill_on_drop from terminating the microVM — operator inspects manually.
        std::mem::forget(vm);
        info!("--keep: cleanup suppressed; firecracker process and state dir preserved");
    } else if let Err(e) = vm.shutdown().await {
        warn!(error = %e, "shutdown reported an error after post-boot");
    }
    outcome
}

async fn post_boot(
    network: &firecracker::NetworkConfig,
    key: &Path,
    keep: bool,
    command: Option<String>,
    vm: &mut firecracker::BootedVm,
    config: &RoomsConfig,
) -> Result<u8, RoomsError> {
    match (keep, command) {
        (true, _) => {
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
        (false, Some(cmd)) => {
            let work = async {
                runner::wait_for_ssh(&network.guest_ip, key, config).await?;
                runner::seed_entropy(&network.guest_ip, key)
                    .await
                    .map_err(RoomsError::Runner)?;
                let code = runner::exec_in_guest(&network.guest_ip, key, &cmd)
                    .await
                    .map_err(RoomsError::Runner)?;
                Ok::<u8, RoomsError>(u8::try_from(code).unwrap_or(2))
            };
            tokio::pin!(work);
            tokio::select! {
                res = &mut work => res,
                _ = tokio::signal::ctrl_c() => {
                    info!("ctrl-c received during exec setup or run; aborting and shutting down");
                    Ok(130)
                }
            }
        }
        (false, None) => {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
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

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        reason = "test module"
    )]

    use super::Cli;
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
}
