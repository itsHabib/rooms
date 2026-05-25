//! `rooms` — disposable Firecracker microVMs with specified deps.
//!
//! Substrate for spawning a clean microVM, running a command in it, collecting
//! artifacts, and tearing it down. See `docs/features/rooms-v0/spec.md`.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use rooms::{firecracker, runner};
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
        /// Mutually exclusive with `--command`.
        #[arg(long, conflicts_with = "command")]
        keep: bool,
        /// Run a single command in the guest via SSH, capture its stdout/stderr on
        /// host stdout/stderr, propagate its exit code, then shut down.
        /// Mutually exclusive with `--keep`.
        #[arg(long, conflicts_with = "keep", value_parser = non_empty_command)]
        command: Option<String>,
    },
    /// Check the host environment (KVM, Firecracker, image, etc.).
    Doctor,
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

async fn dispatch(cli: Cli) -> Result<u8> {
    match cli.command {
        Command::Run {
            image,
            keep,
            command,
        } => run_room(image, keep, command).await,
        Command::Doctor => {
            info!("rooms doctor");
            anyhow::bail!("doctor: not yet implemented (POC in flight)")
        }
    }
}

async fn run_room(image: PathBuf, keep: bool, command: Option<String>) -> Result<u8> {
    info!(?image, keep, command = ?command.as_deref(), "rooms run");

    let kernel = image
        .parent()
        .ok_or_else(|| anyhow::anyhow!("--image has no parent directory: {}", image.display()))?
        .join("vmlinux.bin");
    anyhow::ensure!(
        kernel.exists(),
        "kernel not found at {}; expected sibling of --image",
        kernel.display()
    );

    let network = firecracker::NetworkConfig {
        tap_name: "tap-fc0".to_owned(),
        guest_ip: "172.16.0.2".to_owned(),
        gateway_ip: "172.16.0.1".to_owned(),
        netmask: "255.255.255.0".to_owned(),
    };
    let key = key_path()?;
    let mut vm = firecracker::boot(&kernel, &image, Some(&network)).await?;

    // Always run shutdown, whatever post_boot returns. `post_boot` is a separate
    // function so its internal `?` returns from itself, NOT from run_room — that's
    // what guarantees the shutdown call below runs on the error paths.
    let outcome = post_boot(&network, &key, keep, command, &mut vm).await;
    if let Err(e) = vm.shutdown().await {
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
) -> Result<u8> {
    match (keep, command) {
        (true, _) => {
            info!(
                guest_ip = %network.guest_ip,
                "microVM is up; Ctrl-C to shut down (try `ping {}` from another shell)",
                network.guest_ip,
            );
            tokio::signal::ctrl_c()
                .await
                .context("waiting for Ctrl-C")?;
            Ok(0)
        }
        (false, Some(cmd)) => {
            runner::wait_for_ssh(&network.guest_ip, key, Duration::from_mins(1)).await?;
            // Seed the guest's CRNG before any `--command` runs. The bundled
            // bionic kernel has no entropy source and openssl's TLS handshake
            // would otherwise hang indefinitely on getrandom(). See
            // runner::seed_entropy for the gory details.
            runner::seed_entropy(&network.guest_ip, key).await?;
            // tokio::select! between exec and ctrl_c so a Ctrl-C during the guest
            // command drops the exec future (kill_on_drop SIGKILLs the ssh child),
            // returns Ok(130), and run_room's vm.shutdown() runs cleanly. Without
            // this, the default SIGINT terminates rooms before shutdown can fire.
            let exec_fut = runner::exec_in_guest(&network.guest_ip, key, &cmd);
            tokio::pin!(exec_fut);
            tokio::select! {
                res = &mut exec_fut => {
                    let code = res?;
                    Ok(u8::try_from(code).unwrap_or(2))
                }
                _ = tokio::signal::ctrl_c() => {
                    info!("ctrl-c received during command; aborting and shutting down");
                    Ok(130)
                }
            }
        }
        (false, None) => {
            tokio::time::sleep(Duration::from_secs(3)).await;
            if vm.is_alive()? {
                info!("microVM is up; shutting down (POC: no exec yet)");
                Ok(0)
            } else {
                anyhow::bail!("firecracker exited prematurely; check serial output")
            }
        }
    }
}

fn key_path() -> Result<PathBuf> {
    // Convention: same dedicated key m3's bake script creates / reuses.
    // No env-var override at the m4 layer; --key-path lands in productionization
    // when per-room dynamic keys become a thing.
    //
    // Bail (don't fall back to "/root") if HOME is unset — silent /root fallback
    // would mask "you ran with sudo" footguns, where the key actually lives in
    // the operator's home. The bake script itself refuses to run under sudo for
    // the same reason.
    let home = std::env::var("HOME")
        .context("HOME env var unset; rooms needs it to locate ~/.ssh/id_rooms")?;
    Ok(PathBuf::from(home).join(".ssh/id_rooms"))
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        reason = "test module: panicky lints are noise in tests"
    )]

    use super::Cli;
    use clap::{CommandFactory, Parser};

    #[test]
    fn cli_definition_is_valid() {
        // clap's `debug_assert` validates the derived CLI shape at runtime.
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
