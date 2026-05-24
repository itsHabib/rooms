//! `rooms` — disposable Firecracker microVMs with specified deps.
//!
//! Substrate for spawning a clean microVM, running a command in it, collecting
//! artifacts, and tearing it down. See `docs/features/rooms-v0/spec.md`.
//!
//! v0 scaffold: the CLI surface is wired; subcommand bodies are stubs that
//! exit with a non-zero status until the POC fills them in.

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use rooms::firecracker;
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
    /// Create a new room from an image, with a repo bundled in.
    Create {
        /// Path to the rootfs image (ext4).
        #[arg(long)]
        image: PathBuf,
        /// Path to the source repo to bundle into the room.
        #[arg(long)]
        repo: PathBuf,
        /// Keep the room alive until Ctrl-C instead of the default 3s auto-shutdown.
        /// Useful for poking from another shell (`ping 172.16.0.2`, future `ssh ...`).
        #[arg(long)]
        keep: bool,
    },
    /// Execute a command inside a room.
    Exec {
        /// Room id (from `rooms create`).
        room_id: String,
        /// Command + args to run inside the room (after `--`).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        argv: Vec<String>,
    },
    /// Collect artifacts from a room to a host directory.
    Collect {
        /// Room id.
        room_id: String,
        /// Host directory to copy /workspace/out into.
        #[arg(long)]
        to: PathBuf,
    },
    /// Destroy a room: kill firecracker, release resources, remove work dir.
    Destroy {
        /// Room id.
        room_id: String,
        /// Skip cleanup; leave the room alive for inspection.
        #[arg(long)]
        keep: bool,
    },
    /// Run a task end-to-end: create + exec + collect + destroy.
    Run {
        /// Source repo to bundle in.
        #[arg(long)]
        repo: PathBuf,
        /// Path to the task spec markdown.
        #[arg(long)]
        task: PathBuf,
        /// Rootfs image; defaults to the configured node-dev image.
        #[arg(long)]
        image: Option<PathBuf>,
    },
    /// Check the host environment (KVM, Firecracker, image, etc.).
    Doctor,
}

#[tokio::main]
async fn main() -> ExitCode {
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(env_filter).init();

    let cli = Cli::parse();
    match dispatch(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            warn!(error = %err, "command failed");
            ExitCode::from(2)
        }
    }
}

#[allow(
    clippy::unused_async,
    reason = "scaffold: bodies become async once Firecracker control + async I/O are wired (task #2)"
)]
async fn dispatch(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Create { image, repo, keep } => {
            info!(?image, ?repo, keep, "rooms create");
            // POC: derive the kernel as a sibling of the rootfs image
            // (matches the layout setup-rooms-host.sh creates).
            let kernel = image
                .parent()
                .ok_or_else(|| {
                    anyhow::anyhow!("--image has no parent directory: {}", image.display())
                })?
                .join("vmlinux.bin");
            anyhow::ensure!(
                kernel.exists(),
                "kernel not found at {}; expected sibling of --image",
                kernel.display()
            );
            // POC: hardcoded single-room network. TAP must already exist
            // (`bash scripts/setup-tap.sh`). Per-room dynamic TAPs are task #2.
            let network = firecracker::NetworkConfig {
                tap_name: "tap-fc0".to_owned(),
                guest_ip: "172.16.0.2".to_owned(),
                gateway_ip: "172.16.0.1".to_owned(),
                netmask: "255.255.255.0".to_owned(),
            };
            let mut vm = firecracker::boot(&kernel, &image, Some(&network)).await?;

            if keep {
                info!(
                    guest_ip = %network.guest_ip,
                    "microVM is up; Ctrl-C to shut down (try `ping {}` from another shell)",
                    network.guest_ip
                );
                tokio::signal::ctrl_c()
                    .await
                    .context("waiting for Ctrl-C")?;
                info!("Ctrl-C received; shutting down");
            } else {
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                anyhow::ensure!(
                    vm.is_alive()?,
                    "firecracker exited prematurely; check serial output"
                );
                info!("microVM is up; shutting down (POC: no exec yet)");
            }

            vm.shutdown().await?;
            Ok(())
        }
        Command::Exec { room_id, argv } => {
            info!(%room_id, ?argv, "rooms exec");
            anyhow::bail!("exec: not yet implemented (POC in flight)")
        }
        Command::Collect { room_id, to } => {
            info!(%room_id, ?to, "rooms collect");
            anyhow::bail!("collect: not yet implemented (POC in flight)")
        }
        Command::Destroy { room_id, keep } => {
            info!(%room_id, keep, "rooms destroy");
            anyhow::bail!("destroy: not yet implemented (POC in flight)")
        }
        Command::Run { repo, task, image } => {
            info!(?repo, ?task, ?image, "rooms run");
            anyhow::bail!("run: not yet implemented (POC in flight)")
        }
        Command::Doctor => {
            info!("rooms doctor");
            anyhow::bail!("doctor: not yet implemented (POC in flight)")
        }
    }
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
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        // clap's `debug_assert` validates the derived CLI shape at runtime.
        Cli::command().debug_assert();
    }
}
