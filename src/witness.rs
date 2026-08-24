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
//!
//! The capture is bounded by a hard total cap: a host-side watcher stops
//! `tcpdump` once `witness.pcap` reaches [`CAPTURE_CAP_BYTES`], so a runaway or
//! malicious guest cannot fill the host disk, and everything captured up to the
//! cap (the earliest contacts — the highest-value evidence) is preserved. A
//! capped capture is reported `capture_complete: false` like any other
//! truncation.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::process::{Child, Command};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

/// The `tcpdump` binary, resolved on `PATH`.
const TCPDUMP: &str = "tcpdump";

/// The `ip` binary used to enter a clone's network namespace.
const IP: &str = "ip";

/// Prefix for the staged raw-capture file under the AppArmor-safe staging dir
/// (`/tmp` on unix — see [`staging_dir`]); the full name is
/// `{STAGED_PCAP_PREFIX}{room-id}.pcap`. tcpdump writes here rather than into the
/// room work dir because Ubuntu's `AppArmor` `usr.bin.tcpdump` profile denies
/// writes under any dot-directory in `$HOME` (`audit deny @{HOME}/.*/**`) —
/// exactly where the room dir lives (`~/.local/state/rooms/<id>/`) — so staging
/// there fails `--witness` closed (`tcpdump exited immediately`) on a stock
/// (profile-enforcing) host. `/tmp` is permitted by the profile's
/// `abstractions/user-tmp` include and its global `/**.pcap rw` rule, so the
/// witness works with zero host provisioning. The bytes are read back and
/// persisted into `--out`, then the staged file is removed.
const STAGED_PCAP_PREFIX: &str = "rooms-witness-";

/// Hard total cap for the raw capture, in bytes. The watcher stops the capture
/// at the cap rather than letting it grow (or rotate) without bound — a runaway
/// egress can't fill the host disk, and the truncation is visible in the
/// summary (`capture_complete: false`).
const CAPTURE_CAP_BYTES: u64 = 64 * 1024 * 1024;

/// How often the watcher checks the capture file against the cap. The cap can
/// overshoot by at most one interval's worth of traffic.
const WATCH_INTERVAL: Duration = Duration::from_secs(1);

/// How long to watch a freshly-spawned `tcpdump` for an immediate death before
/// trusting it came up. tcpdump exits fast on a bad interface or a permission
/// error; a survivor past this window is treated as live.
const START_SETTLE: Duration = Duration::from_millis(300);

/// Fail-closed preflight for `--witness`: error unless an executable `tcpdump`
/// is on `PATH`.
///
/// Called before the VMM starts so a host without `tcpdump` never boots a room
/// that would run unwitnessed — the whole point of an opt-in witness is that
/// asking for it and silently not getting it is worse than not asking.
pub fn ensure_tcpdump_available() -> Result<(), String> {
    which_tcpdump().map(|_| ()).ok_or_else(|| {
        format!("--witness requested but no executable `{TCPDUMP}` was found on PATH; install it or drop --witness")
    })
}

/// Resolve `tcpdump` on `PATH`, returning its absolute path. Only an executable
/// candidate counts — a stray non-executable file named `tcpdump` on `PATH`
/// must not pass the fail-closed preflight and then die at spawn time.
fn which_tcpdump() -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    std::env::split_paths(&path_var)
        .map(|dir| dir.join(TCPDUMP))
        .find(|candidate| is_executable_file(candidate))
}

/// True when `path` is a regular file the current user can execute. On
/// non-unix hosts (tests on Windows CI) existence is the best available check.
#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable_file(path: &Path) -> bool {
    // Not const: `Path::is_file` does real I/O; the cfg twin can't be const
    // either, so the signatures stay aligned.
    path.is_file()
}

/// A running egress capture bound to one room's tap.
///
/// Owns the `tcpdump` child, the tap/pcap it writes, and the cap watcher that
/// stops it at [`CAPTURE_CAP_BYTES`]. Dropping it without [`Self::stop`] kills
/// the child, aborts the watcher, and unlinks the staged pcap so a panicking run
/// never leaks capture state; the normal path calls [`Self::stop`] to flush
/// cleanly and hands the pcap path back to its caller.
#[derive(Debug)]
pub struct Capture {
    tap: String,
    pcap_path: PathBuf,
    child: Child,
    /// Set by the watcher when it stopped the capture at the size cap, so
    /// [`Self::stop`] reports the truncation rather than a mystery death.
    capped: Arc<AtomicBool>,
    watcher: JoinHandle<()>,
    /// Cleared by [`Self::stop`], whose caller owns reading/removing the pcap.
    /// An unexpected drop keeps this true and removes the staging file.
    remove_pcap_on_drop: bool,
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
    /// Spawns `tcpdump -i <tap> -w <pcap>` with an unbuffered writer, then
    /// watches [`START_SETTLE`] for an immediate exit (bad interface, missing
    /// privilege). A capture that dies in that window is a start failure — the
    /// caller decides whether that is fatal (it is, on the initial `--witness`
    /// start; see the module docs). A survivor is returned live, with the cap
    /// watcher running beside it.
    pub async fn start(tap: &str, room_dir: &Path) -> Result<Self, String> {
        Self::start_for(CaptureTarget::Host, tap, room_dir).await
    }

    /// Start capturing on `tap` inside `namespace`.
    ///
    /// The namespace is passed as one direct argv token to
    /// `ip netns exec <namespace> tcpdump ...`; no shell parses it. Capture
    /// startup and teardown otherwise have the same fail-closed and bounded
    /// behavior as [`Self::start`].
    pub async fn start_in_namespace(
        namespace: &str,
        tap: &str,
        room_dir: &Path,
    ) -> Result<Self, String> {
        Self::start_for(CaptureTarget::Namespace(namespace), tap, room_dir).await
    }

    async fn start_for(
        target: CaptureTarget<'_>,
        tap: &str,
        room_dir: &Path,
    ) -> Result<Self, String> {
        let pcap_path = staging_pcap_path(room_dir);
        let mut child = spawn_tcpdump(target, tap, &pcap_path)?;
        // A tap that doesn't exist, or a tcpdump lacking capture privilege,
        // exits within milliseconds. Watch briefly so start-failure is caught
        // here (fail-closed) rather than surfacing as an empty pcap later.
        tokio::time::sleep(START_SETTLE).await;
        let status = match child.try_wait() {
            Ok(status) => status,
            Err(error) => {
                let _ = std::fs::remove_file(&pcap_path);
                return Err(format!("probe tcpdump liveness: {error}"));
            }
        };
        if let Some(status) = status {
            let _ = std::fs::remove_file(&pcap_path);
            return Err(format!(
                "tcpdump exited immediately ({status}) capturing {tap}; capture did not start"
            ));
        }
        let capped = Arc::new(AtomicBool::new(false));
        let watcher = spawn_cap_watcher(pcap_path.clone(), child.id(), Arc::clone(&capped));
        info!(tap, pcap = %pcap_path.display(), "witness capture started");
        Ok(Self {
            tap: tap.to_owned(),
            pcap_path,
            child,
            capped,
            watcher,
            remove_pcap_on_drop: true,
        })
    }

    /// The tap this capture is bound to.
    #[must_use]
    pub fn tap(&self) -> &str {
        &self.tap
    }

    /// The staged raw-capture path (under the system temp dir, not the room work
    /// dir — see [`STAGED_PCAP_PREFIX`]). Read back into `--out`, then removed by
    /// the caller after the summary.
    #[must_use]
    pub fn pcap_path(&self) -> &Path {
        &self.pcap_path
    }

    /// Stop the capture, flushing buffered packets, and report completeness.
    ///
    /// Sends `SIGTERM` (which makes tcpdump flush and exit 0) and waits for the
    /// child. Completeness is false if the capture reached the size cap (by the
    /// watcher or the synchronous final check), if the child already died on its
    /// own (a mid-run failure), or if it exited non-zero. A best-effort `SIGKILL`
    /// fallback bounds a stuck child so teardown never hangs.
    pub async fn stop(mut self) -> CaptureOutcome {
        self.watcher.abort();
        // A normal stop deliberately transfers the staged pcap to the caller,
        // which reads and removes it after this future returns.
        self.remove_pcap_on_drop = false;
        // The watcher ticks on an interval, so a burst that exceeds the cap and
        // exits before the next tick would never trip the flag. Re-check the
        // final size synchronously here so the cap is enforced at stop time too,
        // not only on a watcher tick.
        let over_cap = tokio::fs::metadata(&self.pcap_path)
            .await
            .is_ok_and(|m| m.len() >= CAPTURE_CAP_BYTES);
        let capped = self.capped.load(Ordering::Relaxed) || over_cap;
        // Already dead before we asked? Either the watcher capped it (visible
        // truncation) or it died mid-run — the run went on, per the failure
        // posture, but the pcap is partial either way.
        if matches!(self.child.try_wait(), Ok(Some(_)) | Err(_)) {
            if capped {
                warn!(tap = %self.tap, "witness capture stopped at the size cap; summary will be incomplete");
            } else {
                warn!(tap = %self.tap, "witness capture died before teardown; summary will be incomplete");
            }
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
        let complete = clean_exit && !capped;
        debug!(tap = %self.tap, complete, capped, "witness capture stopped");
        CaptureOutcome { complete }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.watcher.abort();
        let _ = self.child.start_kill();
        if self.remove_pcap_on_drop {
            let _ = std::fs::remove_file(&self.pcap_path);
        }
    }
}

/// Where the capture process must observe its tap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CaptureTarget<'a> {
    Host,
    Namespace(&'a str),
}

/// A direct executable-and-argv plan. Keeping command construction separate
/// from spawning makes the no-shell boundary testable.
#[derive(Debug, PartialEq, Eq)]
struct CaptureCommand {
    program: &'static str,
    args: Vec<OsString>,
}

/// Spawn the watcher that enforces the total capture cap: poll the pcap size
/// every [`WATCH_INTERVAL`] and stop `tcpdump` once it reaches
/// [`CAPTURE_CAP_BYTES`]. The flag records that the stop was a deliberate cap,
/// not a mystery death, so [`Capture::stop`] reports it honestly. A missing
/// file (capture not yet flushed) just means "keep waiting".
fn spawn_cap_watcher(
    pcap_path: PathBuf,
    pid: Option<u32>,
    capped: Arc<AtomicBool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let Some(pid) = pid else {
            return;
        };
        loop {
            tokio::time::sleep(WATCH_INTERVAL).await;
            let size = tokio::fs::metadata(&pcap_path).await.map_or(0, |m| m.len());
            if size < CAPTURE_CAP_BYTES {
                continue;
            }
            capped.store(true, Ordering::Relaxed);
            warn!(pcap = %pcap_path.display(), size, cap = CAPTURE_CAP_BYTES, "witness capture hit the size cap; stopping tcpdump");
            send_sigterm(pid);
            return;
        }
    })
}

/// The staged raw-capture path for the room, `{STAGED_PCAP_PREFIX}{room-id}.pcap`
/// under the system temp dir. Keyed on the room id (the work dir's basename) so
/// concurrent rooms never collide on the shared, tap-reused capture name. See
/// [`STAGED_PCAP_PREFIX`] for why staging lives in the temp dir, not the room dir.
fn staging_pcap_path(room_dir: &Path) -> PathBuf {
    let id = room_dir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("room");
    staging_dir().join(format!("{STAGED_PCAP_PREFIX}{id}.pcap"))
}

/// The AppArmor-safe staging directory. Hardcoded `/tmp` on unix rather than
/// `std::env::temp_dir()`: the latter honors `TMPDIR`, which (preserved through
/// `sudo -E`, or set by a service unit) could point back under a `$HOME`
/// dot-directory the tcpdump profile denies — silently reintroducing the very
/// fail-closed this staging fixes. `/tmp` is always profile-permitted
/// (`abstractions/user-tmp`) and never under `$HOME`.
#[cfg(unix)]
fn staging_dir() -> PathBuf {
    PathBuf::from("/tmp")
}

/// Non-unix has no tcpdump/`AppArmor` to appease (the capture never really runs
/// there); the system temp dir keeps the cross-platform compile honest.
#[cfg(not(unix))]
fn staging_dir() -> PathBuf {
    std::env::temp_dir()
}

/// Spawn the `tcpdump` capture process on `tap`, writing to `pcap_path`.
///
/// `-i <tap>` binds to the room's own interface; `-w` writes raw pcap; `-U`
/// packet-buffers so a SIGTERM flush loses nothing; `-s 0` captures full frames
/// (DNS names live past the snaplen a default would impose); `-n` skips name
/// resolution (no host DNS lookups from the capture itself). The total size
/// bound lives in [`spawn_cap_watcher`], not in tcpdump flags: `-C` alone
/// rotates without limit and `-C` + `-W` overwrites the earliest evidence, so
/// the single-file capture with a host-side stop is both bounded and honest.
fn spawn_tcpdump(target: CaptureTarget<'_>, tap: &str, pcap_path: &Path) -> Result<Child, String> {
    let plan = capture_command(target, tap, pcap_path);
    Command::new(plan.program)
        .args(plan.args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| spawn_error(target, tap, &e))
}

fn capture_command(target: CaptureTarget<'_>, tap: &str, pcap_path: &Path) -> CaptureCommand {
    let tcpdump_args = [
        OsString::from("-i"),
        OsString::from(tap),
        OsString::from("-w"),
        pcap_path.as_os_str().to_owned(),
        OsString::from("-U"),
        OsString::from("-s"),
        OsString::from("0"),
        OsString::from("-n"),
    ];
    match target {
        CaptureTarget::Host => CaptureCommand {
            program: TCPDUMP,
            args: tcpdump_args.into(),
        },
        CaptureTarget::Namespace(namespace) => CaptureCommand {
            program: IP,
            args: [
                OsString::from("netns"),
                OsString::from("exec"),
                OsString::from(namespace),
                OsString::from(TCPDUMP),
            ]
            .into_iter()
            .chain(tcpdump_args)
            .collect(),
        },
    }
}

fn spawn_error(target: CaptureTarget<'_>, tap: &str, error: &std::io::Error) -> String {
    match target {
        CaptureTarget::Host => format!("spawn {TCPDUMP} on {tap}: {error}"),
        CaptureTarget::Namespace(namespace) => {
            format!("spawn {TCPDUMP} on {tap} in network namespace {namespace}: {error}")
        }
    }
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
// clippy wants the no-op stub const; the unix twin can't be, keep them aligned.
#[allow(clippy::missing_const_for_fn, reason = "cfg twin of a non-const fn")]
fn send_sigterm(_pid: u32) {}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        reason = "test module"
    )]

    use std::ffi::OsString;
    use std::path::Path;
    use std::sync::Mutex;

    use super::{
        capture_command, ensure_tcpdump_available, staging_pcap_path, which_tcpdump,
        CaptureCommand, CaptureTarget,
    };

    /// Serializes the PATH-mutating tests: `set_var` is process-global and Rust
    /// runs tests in parallel, so every test that touches PATH must hold this.
    static PATH_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn host_capture_command_runs_tcpdump_directly() {
        let plan = capture_command(
            CaptureTarget::Host,
            "tap-fc0",
            Path::new("/tmp/witness.pcap"),
        );
        assert_eq!(
            plan,
            CaptureCommand {
                program: "tcpdump",
                args: vec![
                    OsString::from("-i"),
                    OsString::from("tap-fc0"),
                    OsString::from("-w"),
                    OsString::from("/tmp/witness.pcap"),
                    OsString::from("-U"),
                    OsString::from("-s"),
                    OsString::from("0"),
                    OsString::from("-n"),
                ],
            }
        );
    }

    #[test]
    fn namespaced_capture_command_keeps_namespace_as_one_argv_token() {
        let hostile_namespace = "rooms-clone; touch /tmp/not-run";
        let plan = capture_command(
            CaptureTarget::Namespace(hostile_namespace),
            "tap-fc0",
            Path::new("/tmp/witness.pcap"),
        );
        assert_eq!(
            plan,
            CaptureCommand {
                program: "ip",
                args: vec![
                    OsString::from("netns"),
                    OsString::from("exec"),
                    OsString::from(hostile_namespace),
                    OsString::from("tcpdump"),
                    OsString::from("-i"),
                    OsString::from("tap-fc0"),
                    OsString::from("-w"),
                    OsString::from("/tmp/witness.pcap"),
                    OsString::from("-U"),
                    OsString::from("-s"),
                    OsString::from("0"),
                    OsString::from("-n"),
                ],
            }
        );
    }

    #[test]
    fn staging_pcap_path_is_apparmor_safe_and_keyed_on_room_id() {
        // The AppArmor fix: tcpdump must write outside the room dir (a HOME
        // dot-dir the tcpdump profile denies). The staged path carries the room
        // id so concurrent rooms don't collide, and never lands under $HOME.
        let staged = staging_pcap_path(Path::new("/home/mh/.local/state/rooms/01ABCDEF"));
        assert_eq!(
            staged.file_name().and_then(|n| n.to_str()),
            Some("rooms-witness-01ABCDEF.pcap"),
            "keyed on the room id, with a .pcap suffix the profile permits"
        );
        assert!(
            !staged.starts_with("/home/mh/.local"),
            "must never stage under the denied HOME dot-dir path: {}",
            staged.display()
        );
        // On unix the staging dir is the AppArmor-permitted `/tmp`, independent
        // of `TMPDIR` (which could point back under a denied HOME dot-dir).
        #[cfg(unix)]
        assert!(
            staged.starts_with("/tmp"),
            "unix staging is the hardcoded, profile-permitted /tmp: {}",
            staged.display()
        );
    }

    #[test]
    fn staging_pcap_path_falls_back_without_a_basename() {
        // A pathological room dir with no basename still yields a valid staged
        // path rather than panicking.
        let staged = staging_pcap_path(Path::new("/"));
        assert_eq!(
            staged.file_name().and_then(|n| n.to_str()),
            Some("rooms-witness-room.pcap")
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
    fn resolves_an_executable_tcpdump_on_path() {
        // A dir holding an executable-named file resolves as the binary; this
        // exercises the PATH walk without depending on a real tcpdump install.
        let dir = tempfile::tempdir().expect("tempdir");
        let stub = dir.path().join("tcpdump");
        std::fs::write(&stub, b"#!/bin/sh\n").expect("stub binary");
        make_executable(&stub);
        temp_env_path(dir.path(), || {
            let found = which_tcpdump().expect("stub tcpdump resolves");
            assert_eq!(found, dir.path().join("tcpdump"));
            assert!(ensure_tcpdump_available().is_ok());
        });
    }

    #[cfg(unix)]
    #[test]
    fn non_executable_tcpdump_fails_the_preflight() {
        // A file named tcpdump without the executable bit must not pass the
        // fail-closed check — it would only die later, at spawn time.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("tcpdump"), b"not a binary").expect("stub file");
        temp_env_path(dir.path(), || {
            assert!(
                ensure_tcpdump_available().is_err(),
                "a non-executable tcpdump must fail the preflight"
            );
        });
    }

    /// Mark `path` executable on unix; a no-op elsewhere (Windows executes by
    /// extension, and the non-unix resolver only checks existence).
    fn make_executable(path: &Path) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(path).expect("stat stub").permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(path, perms).expect("chmod stub");
        }
        #[cfg(not(unix))]
        {
            let _ = path;
        }
    }

    /// Run `f` with `PATH` set to `dir` alone, restoring the prior value after.
    /// Holds [`PATH_LOCK`] for the duration so parallel tests never observe the
    /// mutated PATH.
    fn temp_env_path(dir: &Path, f: impl FnOnce()) {
        let _guard = PATH_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let prev = std::env::var_os("PATH");
        std::env::set_var("PATH", dir);
        f();
        match prev {
            Some(p) => std::env::set_var("PATH", p),
            None => std::env::remove_var("PATH"),
        }
    }
}
