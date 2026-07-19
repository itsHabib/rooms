//! Host-side egress witness — an unforgeable record of a room's network traffic.
//!
//! The one room surface the guest cannot reach is its network tap: every packet
//! the guest sends physically transits `tap-fc<k>` on the host. This module owns
//! the *capture* mechanism — spawn `tcpdump` on that tap before the guest can
//! transmit, stop it at teardown — beside the other host-process mechanisms.
//! The summary that turns the raw `witness.pcap` into `witness.json` is pure
//! parsing and lives in [`crate::artifacts`]; this layer is dumb plumbing.
//!
//! Failure posture is asymmetric on purpose. The *initial* start fails closed
//! (a missing `tcpdump` under `--witness` is a hard error — never run
//! unwitnessed), but once capture is running a mid-run death does not kill the
//! workload: the run continues and the summary records `capture_complete:
//! false`. Truncation is always visible, never silent.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::process::{Child, Command};
use tracing::{debug, info, warn};

/// The `tcpdump` binary, resolved on `PATH`.
const TCPDUMP: &str = "tcpdump";

/// Raw-capture file name inside the room work dir (staged, then copied to `out/`).
pub const PCAP_FILE: &str = "witness.pcap";

/// Size cap for the raw capture, in megabytes. A capture that reaches the cap is
/// reported truncated (`capture_complete: false`) rather than growing without
/// bound — a runaway egress can't fill the host disk, and the truncation is
/// visible in the summary.
const CAPTURE_CAP_MB: u64 = 64;

/// How long to watch a freshly-spawned `tcpdump` for an immediate death before
/// trusting it came up. tcpdump exits fast on a bad interface or a permission
/// error; a survivor past this window is treated as live.
const START_SETTLE: Duration = Duration::from_millis(300);

/// Fail-closed preflight for `--witness`: error unless `tcpdump` is on `PATH`.
///
/// Called before the VMM starts so a host without `tcpdump` never boots a room
/// that would run unwitnessed — the whole point of an opt-in witness is that
/// asking for it and silently not getting it is worse than not asking.
pub fn ensure_tcpdump_available() -> Result<(), String> {
    which_tcpdump().map(|_| ()).ok_or_else(|| {
        format!("--witness requested but `{TCPDUMP}` was not found on PATH; install it or drop --witness")
    })
}

/// Resolve `tcpdump` on `PATH`, returning its absolute path.
fn which_tcpdump() -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    std::env::split_paths(&path_var)
        .map(|dir| dir.join(TCPDUMP))
        .find(|candidate| candidate.is_file())
}

/// A running egress capture bound to one room's tap.
///
/// Owns the `tcpdump` child and the tap/pcap it writes. Dropping it without
/// [`Self::stop`] kills the child (kill-on-drop) so a panicking run never leaks
/// a capture process; the normal path calls [`Self::stop`] to flush cleanly.
#[derive(Debug)]
pub struct Capture {
    tap: String,
    pcap_path: PathBuf,
    child: Child,
    /// Set once a start-time failure is known, so the eventual summary can be
    /// marked incomplete even if `stop` later sees a clean-looking exit.
    started_clean: bool,
}

/// The outcome of stopping a capture — the completeness bit the summary needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureOutcome {
    /// True only when capture ran uninterrupted end to end and did not hit the
    /// size cap. False whenever the raw pcap may be partial.
    pub complete: bool,
}

impl Capture {
    /// Start capturing on `tap`, writing the raw pcap under `room_dir`.
    ///
    /// Spawns `tcpdump -i <tap> -w <pcap>` with an unbuffered writer and the
    /// size cap, then watches [`START_SETTLE`] for an immediate exit (bad
    /// interface, missing privilege). A capture that dies in that window is a
    /// start failure — the caller decides whether that is fatal (it is, on the
    /// initial `--witness` start; see the module docs). A survivor is returned
    /// live.
    pub async fn start(tap: &str, room_dir: &Path) -> Result<Self, String> {
        let pcap_path = room_dir.join(PCAP_FILE);
        let mut child = spawn_tcpdump(tap, &pcap_path)?;
        // A tap that doesn't exist, or a tcpdump lacking capture privilege,
        // exits within milliseconds. Watch briefly so start-failure is caught
        // here (fail-closed) rather than surfacing as an empty pcap later.
        tokio::time::sleep(START_SETTLE).await;
        if let Some(status) = child
            .try_wait()
            .map_err(|e| format!("probe tcpdump liveness: {e}"))?
        {
            return Err(format!(
                "tcpdump exited immediately ({status}) capturing {tap}; capture did not start"
            ));
        }
        info!(tap, pcap = %pcap_path.display(), "witness capture started");
        Ok(Self {
            tap: tap.to_owned(),
            pcap_path,
            child,
            started_clean: true,
        })
    }

    /// The tap this capture is bound to.
    #[must_use]
    pub fn tap(&self) -> &str {
        &self.tap
    }

    /// The staged raw-capture path (the first file) in the room work dir.
    #[must_use]
    pub fn pcap_path(&self) -> &Path {
        &self.pcap_path
    }

    /// Every capture file that exists, oldest first: the base `witness.pcap`
    /// plus any rotation parts (`witness.pcap1`, `witness.pcap2`, …) tcpdump
    /// wrote once the size cap was hit. The summary must read all of them so a
    /// guest can't hide egress by flooding the base file past the cap.
    #[must_use]
    pub fn capture_files(&self) -> Vec<PathBuf> {
        let mut files = Vec::new();
        if self.pcap_path.is_file() {
            files.push(self.pcap_path.clone());
        }
        for part in rotation_parts(&self.pcap_path) {
            files.push(part);
        }
        files
    }

    /// Stop the capture, flushing buffered packets, and report completeness.
    ///
    /// Sends `SIGTERM` (which makes tcpdump flush and exit 0) and waits for the
    /// child. Completeness is false if the capture never started clean, if the
    /// child already died on its own (a mid-run failure), if it exited non-zero,
    /// or if it rotated past the size cap (visible truncation). A best-effort
    /// `SIGKILL` fallback bounds a stuck child so teardown never hangs.
    pub async fn stop(mut self) -> CaptureOutcome {
        // Already dead before we asked? A mid-run failure — the run went on, per
        // the failure posture, but the pcap is partial.
        if matches!(self.child.try_wait(), Ok(Some(_)) | Err(_)) {
            warn!(tap = %self.tap, "witness capture died before teardown; summary will be incomplete");
            return CaptureOutcome { complete: false };
        }
        let Some(pid) = self.child.id() else {
            return CaptureOutcome { complete: false };
        };
        send_sigterm(pid);
        let clean_exit = match tokio::time::timeout(START_SETTLE, self.child.wait()).await {
            Ok(Ok(status)) => status.success(),
            Ok(Err(e)) => {
                warn!(tap = %self.tap, error = %e, "waiting on tcpdump failed");
                false
            }
            Err(_) => {
                // tcpdump ignored SIGTERM within the grace; force it so teardown
                // (which deletes the tap next) never blocks on a live capture.
                let _ = self.child.kill().await;
                warn!(tap = %self.tap, "tcpdump did not exit on SIGTERM; killed");
                false
            }
        };
        // Rotation means the size cap was hit — truncation the summary must show.
        let rotated = !rotation_parts(&self.pcap_path).is_empty();
        let complete = self.started_clean && clean_exit && !rotated;
        debug!(tap = %self.tap, complete, rotated, "witness capture stopped");
        CaptureOutcome { complete }
    }
}

/// The rotation files tcpdump wrote past the size cap, in order: `<pcap>1`,
/// `<pcap>2`, … tcpdump appends the index directly to the `-w` name (no
/// separator). Stops at the first gap, so the returned list is contiguous.
fn rotation_parts(pcap_path: &Path) -> Vec<PathBuf> {
    let base = pcap_path.as_os_str();
    let mut parts = Vec::new();
    for n in 1u32.. {
        let mut name = base.to_owned();
        name.push(n.to_string());
        let part = PathBuf::from(name);
        if !part.is_file() {
            break;
        }
        parts.push(part);
    }
    parts
}

/// Spawn the `tcpdump` capture process on `tap`, writing to `pcap_path`.
///
/// `-i <tap>` binds to the room's own interface; `-w` writes raw pcap; `-U`
/// packet-buffers so a SIGTERM flush loses nothing; `-s 0` captures full frames
/// (DNS names live past the snaplen a default would impose); `-n` skips name
/// resolution (no host DNS lookups from the capture itself); `-C <cap>` bounds
/// each file. At the cap tcpdump rotates to `witness.pcap1`, `witness.pcap2`, …;
/// [`Capture::stop`] treats a rotation as visible truncation (`complete: false`)
/// and [`Capture::capture_files`] enumerates every part so no egress is hidden.
fn spawn_tcpdump(tap: &str, pcap_path: &Path) -> Result<Child, String> {
    Command::new(TCPDUMP)
        .arg("-i")
        .arg(tap)
        .arg("-w")
        .arg(pcap_path)
        .arg("-U")
        .arg("-s")
        .arg("0")
        .arg("-n")
        .arg("-C")
        .arg(CAPTURE_CAP_MB.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("spawn {TCPDUMP} on {tap}: {e}"))
}

/// Send `SIGTERM` to `pid` so tcpdump flushes its buffer and exits cleanly.
/// Best-effort: a failure just means the wait below falls through to the kill.
#[cfg(unix)]
fn send_sigterm(pid: u32) {
    let _ = std::process::Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .output();
}

#[cfg(not(unix))]
fn send_sigterm(_pid: u32) {}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        reason = "test module"
    )]

    use super::{ensure_tcpdump_available, rotation_parts, which_tcpdump, PCAP_FILE};

    #[test]
    fn rotation_parts_are_contiguous_and_ordered() {
        // tcpdump appends the index directly to the -w name: witness.pcap1, …
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path().join(PCAP_FILE);
        std::fs::write(&base, b"").expect("base");
        std::fs::write(dir.path().join("witness.pcap1"), b"").expect("part 1");
        std::fs::write(dir.path().join("witness.pcap2"), b"").expect("part 2");
        // A gap at 3: part 4 exists but must not be picked up past the gap.
        std::fs::write(dir.path().join("witness.pcap4"), b"").expect("part 4");

        let parts = rotation_parts(&base);
        assert_eq!(
            parts,
            vec![
                dir.path().join("witness.pcap1"),
                dir.path().join("witness.pcap2"),
            ],
            "contiguous from 1, stopping at the first gap"
        );
    }

    #[test]
    fn no_rotation_yields_no_parts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path().join(PCAP_FILE);
        std::fs::write(&base, b"").expect("base");
        assert!(
            rotation_parts(&base).is_empty(),
            "a capture that never hit the cap has no rotation parts"
        );
    }

    #[test]
    fn missing_tcpdump_is_a_clear_error() {
        // Point PATH at an empty dir so the resolver finds no tcpdump; the error
        // must name the flag and the binary so an operator knows the fix.
        let empty = tempfile::tempdir().expect("tempdir");
        temp_env_path(empty.path(), || {
            let err = ensure_tcpdump_available().expect_err("no tcpdump on an empty PATH");
            assert!(err.contains("--witness"), "names the flag: {err}");
            assert!(err.contains("tcpdump"), "names the binary: {err}");
        });
    }

    #[test]
    fn resolves_a_tcpdump_on_path() {
        // A dir holding an executable-named file resolves as the binary; this
        // exercises the PATH walk without depending on a real tcpdump install.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("tcpdump"), b"#!/bin/sh\n").expect("stub binary");
        temp_env_path(dir.path(), || {
            let found = which_tcpdump().expect("stub tcpdump resolves");
            assert_eq!(found, dir.path().join("tcpdump"));
            assert!(ensure_tcpdump_available().is_ok());
        });
    }

    /// Run `f` with `PATH` set to `dir` alone, restoring the prior value after.
    /// `set_var` is process-global, so these tests must not run concurrently
    /// with other PATH-sensitive code; they're the only PATH mutators here.
    fn temp_env_path(dir: &std::path::Path, f: impl FnOnce()) {
        let prev = std::env::var_os("PATH");
        std::env::set_var("PATH", dir);
        f();
        match prev {
            Some(p) => std::env::set_var("PATH", p),
            None => std::env::remove_var("PATH"),
        }
    }
}
