//! Per-room metadata + liveness — the data the registry reads.
//!
//! Plain data plus its own persistence; no upward import. The filesystem (the
//! per-room dir + the jailer chroot) is the source of truth for "a room
//! exists"; `room.json` only *enriches* it with the pid, label, and age that
//! `rooms ls` shows. A room with no (or unreadable) `room.json` still lists.

use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Schema version for `room.json` (forward-compat, mirrors `result.json`).
pub const ROOM_META_SCHEMA_VERSION: u32 = 1;

/// Metadata file name inside a room's state dir.
pub const ROOM_META_FILE: &str = "room.json";

/// The policy-supplied facts about a room that `boot` records into `room.json`.
///
/// Carries only what the CLI knows and the substrate doesn't — the human label
/// and whether the room was deliberately held (`--keep`).
#[derive(Debug, Clone, Default)]
pub struct RoomDescriptor {
    /// What the room is running, for display: a command, `cursor:<repo>`, etc.
    pub command: Option<String>,
    /// Started with `--keep` (a deliberately-held room, not an in-flight run).
    pub keep: bool,
}

/// Persisted metadata for one room, written atomically at boot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RoomMeta {
    pub schema_version: u32,
    /// Lowercase ULID — matches the room's state-dir name.
    pub id: String,
    /// Human label (command / task); `None` for an idle or unlabelled room.
    pub label: Option<String>,
    pub started_at: DateTime<Utc>,
    /// Jailer→firecracker child pid; `None` leaves liveness `Unknown`.
    pub pid: Option<u32>,
    /// Started with `--keep`.
    pub keep: bool,
}

impl RoomMeta {
    /// Build a v-current metadata record.
    #[must_use]
    pub const fn new(
        id: String,
        label: Option<String>,
        pid: Option<u32>,
        keep: bool,
        started_at: DateTime<Utc>,
    ) -> Self {
        Self {
            schema_version: ROOM_META_SCHEMA_VERSION,
            id,
            label,
            started_at,
            pid,
            keep,
        }
    }
}

/// Write `meta` to `<room_dir>/room.json` atomically.
///
/// Serialize to a temp file in the same dir, then `rename` over the final name.
/// `rename` is atomic on unix (the production host), so a crash mid-write never
/// leaves a half-written `room.json` for the registry to read.
pub fn write_atomic(room_dir: &Path, meta: &RoomMeta) -> std::io::Result<()> {
    let json = serde_json::to_vec_pretty(meta)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let tmp = room_dir.join(".room.json.tmp");
    let final_path = room_dir.join(ROOM_META_FILE);
    std::fs::write(&tmp, &json)?;
    std::fs::rename(&tmp, &final_path)
}

/// Read `<room_dir>/room.json`.
///
/// `Ok(None)` when absent (a room with no metadata — e.g. a crash before the
/// write); `Err` only when a present file can't be read or parsed. Callers
/// decide how soft to be about the error.
pub fn read(room_dir: &Path) -> std::io::Result<Option<RoomMeta>> {
    let path = room_dir.join(ROOM_META_FILE);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let meta = serde_json::from_slice(&bytes)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    Ok(Some(meta))
}

/// Liveness of a room's firecracker process, derived from its recorded pid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    /// The firecracker (or pre-exec jailer) process is running.
    Alive,
    /// The recorded pid is gone, or reused by an unrelated process.
    Dead,
    /// Can't tell — no pid recorded, or `/proc` unreadable. Never reaped.
    Unknown,
}

/// Process names the recorded pid may legitimately carry: the jailer `exec`s
/// into firecracker, so steady state is `firecracker`; `jailer` covers the brief
/// pre-exec window.
const ROOM_PROC_NAMES: [&str; 2] = ["firecracker", "jailer"];

/// Map a process name (`comm`) to liveness. Pure (the I/O lives in `probe`), so
/// it's unit-testable everywhere — a pid running *something else* (reuse) reads
/// as `Dead`, not a false `Alive`.
#[must_use]
pub fn classify_comm(comm: &str) -> Liveness {
    if ROOM_PROC_NAMES.contains(&comm.trim()) {
        return Liveness::Alive;
    }
    Liveness::Dead
}

/// Map a `/proc/<pid>/stat` line to liveness. Pure, so it's unit-testable
/// everywhere.
///
/// The format is `pid (comm) state ...`; `comm` can contain spaces and parens,
/// so the fields are split on the **last** `)`. A **zombie** (`Z`) or dead
/// (`X`/`x`) process is `Dead` even though `/proc/<pid>` still lists it — a
/// killed firecracker whose parent hasn't reaped it lingers as a zombie with
/// `comm` still `firecracker`, which a comm-only check misreads as alive (caught
/// by host-e2e: a `--keep` room's fc killed under its still-alive launcher).
#[must_use]
pub fn classify_stat(stat: &str) -> Liveness {
    let Some(close) = stat.rfind(')') else {
        return Liveness::Unknown;
    };
    let Some(open) = stat[..close].find('(') else {
        return Liveness::Unknown;
    };
    let comm = &stat[open + 1..close];
    let state = stat[close + 1..].trim_start().chars().next();
    if matches!(state, Some('Z' | 'X' | 'x')) {
        return Liveness::Dead;
    }
    classify_comm(comm)
}

/// Probe a room's liveness via `/proc/<pid>/stat`.
///
/// Uid-independent and pid-reuse-aware, unlike `kill -0` (which returns EPERM
/// cross-uid and reads as a false "dead", breaking `rooms ls` run as the
/// unprivileged operator against a firecracker-uid process). A missing
/// `/proc/<pid>` is `Dead`; a zombie is `Dead` (see `classify_stat`); an
/// unreadable entry is `Unknown` (fail-safe: never claim dead when we can't tell
/// — gc won't reap it).
#[must_use]
pub fn probe(pid: Option<u32>) -> Liveness {
    let Some(pid) = pid else {
        return Liveness::Unknown;
    };
    probe_pid(pid)
}

#[cfg(target_os = "linux")]
fn probe_pid(pid: u32) -> Liveness {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => classify_stat(&stat),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Liveness::Dead,
        Err(_) => Liveness::Unknown,
    }
}

#[cfg(not(target_os = "linux"))]
// A const stub would make clippy suggest the public `probe` be const too, but on
// Linux `probe` reads /proc and can't be — keep both plain across platforms.
#[allow(
    clippy::missing_const_for_fn,
    reason = "kept non-const to match the Linux probe_pid that reads /proc"
)]
fn probe_pid(_pid: u32) -> Liveness {
    Liveness::Unknown
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, reason = "test module")]

    use super::{classify_comm, classify_stat, probe, read, write_atomic, Liveness, RoomMeta};
    use chrono::Utc;

    fn sample(pid: Option<u32>) -> RoomMeta {
        RoomMeta::new(
            "01abcdefghijklmnopqrstuvwx".to_owned(),
            Some("id".to_owned()),
            pid,
            false,
            Utc::now(),
        )
    }

    #[test]
    fn meta_round_trips_through_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let meta = sample(Some(42));
        write_atomic(dir.path(), &meta).expect("write");
        let back = read(dir.path()).expect("read").expect("present");
        assert_eq!(meta, back);
    }

    #[test]
    fn atomic_write_leaves_no_temp() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_atomic(dir.path(), &sample(Some(1))).expect("write");
        assert!(
            !dir.path().join(".room.json.tmp").exists(),
            "temp file must not survive"
        );
        assert!(
            dir.path().join("room.json").exists(),
            "final file must exist"
        );
    }

    #[test]
    fn read_absent_is_none_not_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(read(dir.path()).expect("read"), None);
    }

    #[test]
    fn read_corrupt_is_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("room.json"), b"{not json").expect("write garbage");
        assert!(
            read(dir.path()).is_err(),
            "a present-but-corrupt file must error, not read as absent"
        );
    }

    #[test]
    fn classify_comm_distinguishes_firecracker_from_reuse() {
        assert_eq!(classify_comm("firecracker"), Liveness::Alive);
        assert_eq!(classify_comm("firecracker\n"), Liveness::Alive);
        assert_eq!(classify_comm("jailer"), Liveness::Alive);
        // A pid reused by an unrelated process must NOT read as alive.
        assert_eq!(classify_comm("bash"), Liveness::Dead);
        assert_eq!(classify_comm(""), Liveness::Dead);
    }

    #[test]
    fn classify_stat_detects_zombie_reuse_and_identity() {
        // A running firecracker.
        assert_eq!(
            classify_stat("48757 (firecracker) S 1 48757 0"),
            Liveness::Alive
        );
        assert_eq!(
            classify_stat("48757 (firecracker) R 1 48757 0"),
            Liveness::Alive
        );
        assert_eq!(classify_stat("5 (jailer) S 1 5 0"), Liveness::Alive);
        // A zombie firecracker (killed; parent hasn't reaped it) is DEAD even
        // though comm is still "firecracker" — the bug host-e2e caught.
        assert_eq!(
            classify_stat("48757 (firecracker) Z 1 48757 0"),
            Liveness::Dead
        );
        assert_eq!(
            classify_stat("48757 (firecracker) X 1 48757 0"),
            Liveness::Dead
        );
        // pid reused by an unrelated process.
        assert_eq!(classify_stat("48757 (bash) S 1 48757 0"), Liveness::Dead);
        // comm containing spaces/parens — fields split on the LAST ')'.
        assert_eq!(classify_stat("5 (odd ) name) S 1 5 0"), Liveness::Dead);
        // malformed.
        assert_eq!(classify_stat("garbage-no-parens"), Liveness::Unknown);
    }

    #[test]
    fn probe_without_pid_is_unknown() {
        // No pid recorded ⇒ indeterminate ⇒ never reaped.
        assert_eq!(probe(None), Liveness::Unknown);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn probe_absent_pid_is_dead() {
        // A pid that cannot exist (>= 2^22 on Linux) has no /proc entry.
        assert_eq!(probe(Some(4_194_305)), Liveness::Dead);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn probe_self_is_dead_by_name() {
        // The test process is alive but isn't firecracker/jailer — exactly the
        // pid-reuse case. Liveness keys on the process *identity*, not mere
        // existence, so this must read Dead.
        let me = std::process::id();
        assert_eq!(probe(Some(me)), Liveness::Dead);
    }
}
