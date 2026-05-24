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
    /// Boot a microVM in a fresh room. POC scope: boot + shutdown only.
    /// `--command` / `--task` / repo transport / agent runner land in later
    /// milestones (m3 = SSH access, m4 = curl Anthropic from inside, then
    /// the cursor-sdk-runner task).
    Run {
        /// Path to the rootfs image (ext4).
        #[arg(long)]
        image: PathBuf,
        /// Keep the room alive until Ctrl-C instead of the default 3s auto-shutdown.
        #[arg(long)]
        keep: bool,
        // Intentionally absent in this PR: --command, --task, --repo. Land in m3/m4
        // and repo-transport milestones. DO NOT add them speculatively here.
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

async fn dispatch(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Run { image, keep } => {
            info!(?image, keep, "rooms run");
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

            // Capture the post-boot outcome separately so we ALWAYS run
            // `vm.shutdown()` afterward — even on the ensure! failure path.
            // Without this, an early bail leaked the room dir (reviewer PR #1
            // round 3): `kill_on_drop` would reap the child but only
            // shutdown() removes the per-room state dir.
            let post_boot: Result<()> = if keep {
                info!(
                    guest_ip = %network.guest_ip,
                    "microVM is up; Ctrl-C to shut down (try `ping {}` from another shell)",
                    network.guest_ip
                );
                tokio::signal::ctrl_c().await.context("waiting for Ctrl-C")
            } else {
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                if vm.is_alive()? {
                    info!("microVM is up; shutting down (POC: no exec yet)");
                    Ok(())
                } else {
                    Err(anyhow::anyhow!(
                        "firecracker exited prematurely; check serial output"
                    ))
                }
            };

            // Always shut down; report any shutdown error after the post-boot outcome.
            if let Err(e) = vm.shutdown().await {
                warn!(error = %e, "shutdown reported an error after post-boot");
            }
            post_boot
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
