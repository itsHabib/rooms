//! Machine-readable lifecycle stream — append-only NDJSON on the host.
//!
//! `rooms run --lifecycle <path>` selects the stream. Each externally visible
//! transition appends one JSON line — monotonic `seq` from 1, RFC 3339 `ts`,
//! the `room_id`, and an `event` tag with that kind's fields — and is flushed
//! and synced before the run proceeds, so a consumer tailing the file sees
//! every transition by the time its successor is under way and a crash never
//! loses an already-visible one.
//!
//! Events are rooms-native: they name what the substrate did (slot claimed,
//! VMM started, guest answered, workload exited), never any consumer's state
//! machine. A consumer maps them onto its own phases.

use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use serde::Serialize;
use tracing::warn;

/// One lifecycle transition, tagged by `event` in the serialized line.
///
/// The kinds separate what a single `running` state would collapse: a started
/// VMM ([`Event::VmmStarted`]) is not a booted kernel ([`Event::GuestReady`]),
/// which is not a usable workload channel ([`Event::SshReady`]). Terminal
/// failures are distinct kinds — admission ([`Event::PoolFull`]), boot
/// ([`Event::BootFailed`]), readiness ([`Event::GuestUnreachable`]), workload
/// ([`Event::WorkloadFailed`]), collection ([`Event::CollectionFailed`]), and
/// cleanup ([`Event::CleanupFailed`]) — so a consumer branches on the tag,
/// never on a message string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// A pool slot was claimed; the room owns its /30 and tap from here on.
    SlotAllocated { slot: u8, tap: String },
    /// Admission rejected: every slot up to the effective cap is claimed.
    PoolFull { cap: u8 },
    /// The firecracker process is up, its API answered, and the instance was
    /// started. Not readiness — the guest kernel may still fail to boot.
    /// `pid` is typed `Option` only because the process handle reports a
    /// reaped child as `None`; on the emit path the process was just spawned,
    /// so consumers can expect it present.
    VmmStarted { pid: Option<u32> },
    /// Boot never reached a started VMM (jail prep, tap create, API timeout).
    BootFailed { error: String },
    /// The guest kernel is up: the guest answered on its network.
    GuestReady,
    /// The workload channel is usable: sshd accepted a pubkey connection.
    SshReady,
    /// The guest never became usable within the reachability timeout.
    GuestUnreachable { error: String },
    /// Every requested secret was staged in the guest and acked over vsock.
    /// Emitted only when `--secret` was requested, after readiness and before
    /// [`Event::WorkloadStarted`] — the workload gate this event records.
    SecretsDelivered,
    /// The vsock secrets hand-off failed or timed out. Terminal for the run:
    /// no secret ⇒ the workload never starts (fail closed).
    SecretsFailed { error: String },
    /// The workload command was handed to the guest.
    WorkloadStarted { command: Vec<String> },
    /// The workload finished — or was aborted — with this exit code. May
    /// appear without a prior [`Event::WorkloadStarted`] when a cancel or a
    /// wall-clock cap fires during the readiness wait (mirrors the
    /// `result.json` an aborted run records).
    WorkloadExited {
        exit_code: i32,
        status: WorkloadStatus,
    },
    /// The exec machinery failed. Usually no guest exit code exists; when the
    /// workload itself finished first and a post-run step failed (e.g. the
    /// requested branch push), a [`Event::WorkloadExited`] with the real exit
    /// precedes this.
    WorkloadFailed { error: String },
    /// Artifact collection into the host `--out` directory began.
    CollectionStarted,
    /// Artifact collection finished.
    CollectionDone,
    /// Artifact collection failed or timed out; the run's own outcome stands.
    CollectionFailed { error: String },
    /// Host-side egress capture began on the room's own tap, before the guest
    /// could transmit. Emitted only under `--witness`.
    WitnessStarted { tap: String },
    /// Host-side egress capture finished and the summary was derived: how many
    /// distinct destinations the guest contacted, and whether the raw capture
    /// was complete (false if it started late, died early, or hit the size cap).
    /// Emitted only under `--witness`.
    WitnessDone { destinations: usize, complete: bool },
    /// The room was torn down: firecracker reaped, workdir removed, slot freed.
    CleanupDone,
    /// Teardown reported an error; `rooms gc` owns the retry.
    CleanupFailed { error: String },
}

/// How a workload reached its exit code — the same vocabulary `result.json`'s
/// `status` field uses, so the two surfaces never disagree about one run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkloadStatus {
    Succeeded,
    Failed,
    TimedOut,
    Cancelled,
}

/// The envelope every line carries around its event.
#[derive(Serialize)]
struct Record<'a> {
    seq: u64,
    ts: DateTime<Utc>,
    room_id: &'a str,
    #[serde(flatten)]
    event: &'a Event,
}

/// The `--lifecycle` sink: a no-op when the flag is absent, an NDJSON appender
/// otherwise.
///
/// Emission is infallible at the call site — once the stream file exists, a
/// write failure is logged and the run continues, so observation can never
/// break a workload. Interior locking keeps `emit` a shared-reference call;
/// the lock is held only across one synchronous line write.
#[derive(Debug)]
pub struct Lifecycle(Option<Mutex<Writer>>);

impl Lifecycle {
    /// The disabled sink: every [`Self::emit`] is a no-op.
    #[must_use]
    pub const fn disabled() -> Self {
        Self(None)
    }

    /// Create the stream file — truncating a stale one, the stream is per-run —
    /// and bind every subsequent record to `room_id`. The parent directory is
    /// synced so the file's existence is itself durable, not just its lines.
    pub fn create(path: &Path, room_id: &str) -> std::io::Result<Self> {
        let file = File::create(path)?;
        sync_parent_dir(path)?;
        let writer = Writer {
            file,
            seq: 0,
            room_id: room_id.to_owned(),
        };
        Ok(Self(Some(Mutex::new(writer))))
    }

    /// Whether a stream is attached — callers can skip observation-only work
    /// (extra probes) when nothing consumes it.
    #[must_use]
    pub const fn is_enabled(&self) -> bool {
        self.0.is_some()
    }

    /// Append one event, flushed and synced before returning. A failure is
    /// logged, never propagated: the stream goes incomplete, the run goes on.
    pub fn emit(&self, event: &Event) {
        let Some(writer) = &self.0 else {
            return;
        };
        let Ok(mut writer) = writer.lock() else {
            warn!("lifecycle writer lock poisoned; dropping event");
            return;
        };
        if let Err(e) = writer.append(event) {
            warn!(error = %e, "failed to append lifecycle event; stream is now incomplete");
        }
    }
}

/// The open stream file plus the monotonic sequence it stamps.
#[derive(Debug)]
struct Writer {
    file: File,
    seq: u64,
    room_id: String,
}

impl Writer {
    /// Serialize, append, flush, and sync one record. `seq` advances only
    /// after the line is durable, so a failed write retries the same number
    /// with the next event instead of leaving a gap in the contiguous stream.
    fn append(&mut self, event: &Event) -> std::io::Result<()> {
        let record = Record {
            seq: self.seq + 1,
            ts: Utc::now(),
            room_id: &self.room_id,
            event,
        };
        let line = serde_json::to_string(&record)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        writeln!(self.file, "{line}")?;
        self.file.flush()?;
        self.file.sync_data()?;
        self.seq += 1;
        Ok(())
    }
}

/// Sync the directory holding `path`, making the just-created file's directory
/// entry durable. A relative bare filename syncs the cwd. No-op off unix,
/// where a directory cannot be opened for syncing.
#[cfg(unix)]
fn sync_parent_dir(path: &Path) -> std::io::Result<()> {
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    File::open(parent)?.sync_data()
}

#[cfg(not(unix))]
#[allow(
    clippy::missing_const_for_fn,
    clippy::unnecessary_wraps,
    reason = "the signature must match the unix variant, which does fallible I/O"
)]
fn sync_parent_dir(_path: &Path) -> std::io::Result<()> {
    Ok(())
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

    use super::{Event, Lifecycle, WorkloadStatus};
    use tempfile::tempdir;

    fn read_lines(path: &std::path::Path) -> Vec<serde_json::Value> {
        std::fs::read_to_string(path)
            .expect("read stream")
            .lines()
            .map(|l| serde_json::from_str(l).expect("each line is standalone JSON"))
            .collect()
    }

    #[test]
    fn stream_is_contiguous_ndjson_with_envelope() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("lifecycle.ndjson");
        let lc = Lifecycle::create(&path, "room-1").expect("create");
        lc.emit(&Event::SlotAllocated {
            slot: 3,
            tap: "tap-fc3".to_owned(),
        });
        lc.emit(&Event::VmmStarted { pid: Some(42) });
        lc.emit(&Event::GuestReady);

        let lines = read_lines(&path);
        assert_eq!(lines.len(), 3);
        for (i, line) in lines.iter().enumerate() {
            assert_eq!(line["seq"], i as u64 + 1, "seq contiguous from 1");
            assert_eq!(line["room_id"], "room-1");
            assert!(line["ts"].is_string(), "timestamp present");
        }
        assert_eq!(lines[0]["event"], "slot_allocated");
        assert_eq!(lines[0]["slot"], 3);
        assert_eq!(lines[0]["tap"], "tap-fc3");
        assert_eq!(lines[1]["event"], "vmm_started");
        assert_eq!(lines[1]["pid"], 42);
        assert_eq!(lines[2]["event"], "guest_ready");
    }

    #[test]
    fn pool_full_carries_the_walked_cap() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("lifecycle.ndjson");
        let lc = Lifecycle::create(&path, "room-2").expect("create");
        lc.emit(&Event::PoolFull { cap: 8 });

        let lines = read_lines(&path);
        assert_eq!(lines[0]["event"], "pool_full");
        assert_eq!(lines[0]["cap"], 8);
    }

    #[test]
    fn workload_exited_serializes_status_snake_case() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("lifecycle.ndjson");
        let lc = Lifecycle::create(&path, "room-3").expect("create");
        lc.emit(&Event::WorkloadExited {
            exit_code: 124,
            status: WorkloadStatus::TimedOut,
        });
        lc.emit(&Event::WorkloadExited {
            exit_code: 0,
            status: WorkloadStatus::Succeeded,
        });

        let lines = read_lines(&path);
        assert_eq!(lines[0]["event"], "workload_exited");
        assert_eq!(lines[0]["exit_code"], 124);
        assert_eq!(lines[0]["status"], "timed_out");
        assert_eq!(lines[1]["status"], "succeeded");
    }

    #[test]
    fn create_truncates_a_stale_stream() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("lifecycle.ndjson");
        std::fs::write(&path, "stale line from a previous run\n").expect("seed stale file");
        let lc = Lifecycle::create(&path, "room-4").expect("create");
        lc.emit(&Event::CleanupDone);

        let lines = read_lines(&path);
        assert_eq!(lines.len(), 1, "stale content gone");
        assert_eq!(lines[0]["seq"], 1);
    }

    #[test]
    fn disabled_sink_is_a_no_op() {
        let lc = Lifecycle::disabled();
        assert!(!lc.is_enabled());
        // Must not panic or touch the filesystem.
        lc.emit(&Event::CleanupDone);
    }

    #[test]
    fn terminal_failure_kinds_are_distinct_tags() {
        // The adapter contract: five failure classes distinguishable by tag
        // alone. Serialize one of each and assert the tags never collide.
        let events = [
            Event::PoolFull { cap: 1 },
            Event::BootFailed {
                error: "x".to_owned(),
            },
            Event::GuestUnreachable {
                error: "x".to_owned(),
            },
            Event::WorkloadFailed {
                error: "x".to_owned(),
            },
            Event::CollectionFailed {
                error: "x".to_owned(),
            },
            Event::CleanupFailed {
                error: "x".to_owned(),
            },
        ];
        let tags: Vec<String> = events
            .iter()
            .map(|e| {
                let v: serde_json::Value =
                    serde_json::from_str(&serde_json::to_string(e).expect("serialize"))
                        .expect("parse");
                v["event"].as_str().expect("tag").to_owned()
            })
            .collect();
        let unique: std::collections::HashSet<&String> = tags.iter().collect();
        assert_eq!(unique.len(), tags.len(), "tags must not collide: {tags:?}");
    }
}
