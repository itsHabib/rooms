//! Per-room metadata + liveness — the data the registry reads.
//!
//! Plain data plus its own persistence; no upward import. The filesystem (the
//! per-room dir + the jailer chroot) is the source of truth for "a room
//! exists"; `room.json` only *enriches* it with the pid, label, and age that
//! `rooms ls` shows. A room with no (or unreadable) `room.json` still lists.

use std::net::Ipv4Addr;
use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Schema version for `room.json` (forward-compat, mirrors `result.json`).
///
/// v2 adds `pid_starttime`; v3 adds the optional `slot` object; v4 adds the
/// optional `provenance` (sealed-neutral-base lineage). Older files still read
/// (each added field defaults to `None`).
pub const ROOM_META_SCHEMA_VERSION: u32 = 4;

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

/// A room's claimed network slot, recorded in `room.json`.
///
/// The /30 carved for pool slot `index`, displayed by `rooms ls`. Absent = a
/// legacy shared-tap room. Plain data — the allocation logic lives in
/// [`crate::slot`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Slot {
    /// Pool slot index k (1-based; 0 is reserved for the legacy shared tap).
    pub index: u8,
    /// Host tap device name (`tap-fc<k>`).
    pub tap: String,
    /// Host/gateway side of the slot's /30 (`172.16.0.4k+1`).
    pub gateway: Ipv4Addr,
    /// Guest side of the slot's /30 (`172.16.0.4k+2`).
    pub guest: Ipv4Addr,
    /// Network prefix length (30).
    pub prefix: u8,
}

/// Neutrality lineage of a room — authoritative, persisted, and monotonic.
///
/// A Full snapshot's memory file is plaintext guest RAM, so a base may be frozen
/// only when it is provably secret-free. Rather than *observe* a "no secrets yet"
/// event — unenforceable, since a held room can be tainted after the check —
/// neutrality is a property of how the base was built, recorded here as durable
/// state that only ever moves one way.
///
/// Legal transitions: `Provisioning → Neutral` (the seal, once the base is
/// quiesced) and `{Provisioning, Neutral} → Tainted` (any secret-arm, workload
/// start, or ingress that isn't rooms' own warm-up). `Tainted` is terminal —
/// there is no path back. A plain workload room (or a pre-v4 `room.json`) carries
/// no provenance at all and is never snapshottable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provenance {
    /// Pre-seal: rooms is warming the base over its own controlled session. Only
    /// that warm-up is legitimate here, and it does not taint.
    Provisioning,
    /// Sealed and provably secret-free — the only state a snapshot may freeze.
    Neutral,
    /// Poisoned: unique or secret state may be present. Terminal.
    Tainted,
}

/// The one illegal provenance move: sealing a room that isn't provisioning.
///
/// Tainting is always legal (monotonic, terminal), so [`Provenance::seal`] is the
/// only fallible transition. Carries the offending state — `None` means the room
/// has no provenance at all (not a base).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("illegal seal: only a provisioning base seals to neutral (was {0:?})")]
pub struct IllegalSeal(pub Option<Provenance>);

impl Provenance {
    /// Seal a provisioning base to `Neutral` — the guest is quiesced and the
    /// beacon fired. Legal only from `Provisioning`; sealing a `Neutral` or
    /// `Tainted` room is refused, the load-bearing rule: a tainted base must
    /// never present as neutral to the snapshot path.
    ///
    /// # Errors
    /// [`IllegalSeal`] when `self` is not `Provisioning`.
    pub const fn seal(self) -> Result<Self, IllegalSeal> {
        match self {
            Self::Provisioning => Ok(Self::Neutral),
            other => Err(IllegalSeal(Some(other))),
        }
    }

    /// Whether a snapshot may freeze a room in this state — `true` only for
    /// `Neutral`.
    #[must_use]
    pub const fn is_snapshottable(self) -> bool {
        matches!(self, Self::Neutral)
    }
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
    /// Start time of the pid's process incarnation (`/proc/<pid>/stat` field 22,
    /// jiffies since boot). Pins `pid` to a *specific* process so a recycled pid
    /// reads `Dead`, not a false `Alive`. `None` for a pre-v2 `room.json` (or when
    /// it couldn't be read at boot); liveness then falls back to comm-only.
    #[serde(default)]
    pub pid_starttime: Option<u64>,
    /// Started with `--keep`.
    pub keep: bool,
    /// The room's claimed network slot; `None` for a legacy shared-tap room
    /// (or a pre-v3 `room.json`). Not serialized when absent, so legacy files
    /// keep their exact shape.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot: Option<Slot>,
    /// Sealed-neutral-base lineage (`rooms base-create`); `None` for a plain
    /// workload room or a pre-v4 `room.json`.
    ///
    /// **Private and guarded** — the whole point of the primitive is that no
    /// caller can assign `Neutral` directly or reset an established state past
    /// the monotonic ladder. The only ways in are [`RoomMeta::as_base`] (fresh →
    /// `Provisioning`), [`RoomMeta::seal`] (`Provisioning` → `Neutral`), and
    /// [`RoomMeta::taint`]; read it via [`RoomMeta::provenance`] /
    /// [`RoomMeta::is_snapshottable`]. Not serialized when absent, so legacy
    /// files keep their exact shape.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    provenance: Option<Provenance>,
}

impl RoomMeta {
    /// Build a v-current metadata record. The slot starts `None` — the boot
    /// path that claims one sets the field before the atomic write.
    #[must_use]
    pub const fn new(
        id: String,
        label: Option<String>,
        pid: Option<u32>,
        pid_starttime: Option<u64>,
        keep: bool,
        started_at: DateTime<Utc>,
    ) -> Self {
        Self {
            schema_version: ROOM_META_SCHEMA_VERSION,
            id,
            label,
            started_at,
            pid,
            pid_starttime,
            keep,
            slot: None,
            provenance: None,
        }
    }

    /// Mark a freshly-created room as a sealed-neutral base candidate: fresh
    /// (`None`) provenance starts `Provisioning`, so rooms' own warm-up may run
    /// without tainting it. Chains off [`RoomMeta::new`].
    ///
    /// Only *fresh* metadata is promoted — a room already carrying provenance (a
    /// loaded `Neutral`/`Tainted`, or an already-provisioning base) is returned
    /// unchanged, so this can never perform the forbidden reset to `Provisioning`
    /// and hand `seal` a tainted room to re-neutralize.
    #[must_use]
    pub const fn as_base(mut self) -> Self {
        if self.provenance.is_none() {
            self.provenance = Some(Provenance::Provisioning);
        }
        self
    }

    /// Seal this base to `Neutral` — the quiesced beacon fired.
    ///
    /// # Errors
    /// [`IllegalSeal`] unless the room is a provisioning base: a non-base
    /// (`None`), an already-`Neutral`, or a `Tainted` room cannot be sealed.
    pub fn seal(&mut self) -> Result<(), IllegalSeal> {
        let sealed = self.provenance.ok_or(IllegalSeal(None))?.seal()?;
        self.provenance = Some(sealed);
        Ok(())
    }

    /// Taint this room's provenance irreversibly. A no-op on a non-base (`None`)
    /// room — there is no neutrality to lose.
    pub const fn taint(&mut self) {
        if self.provenance.is_some() {
            self.provenance = Some(Provenance::Tainted);
        }
    }

    /// Read this room's provenance (`None` for a plain workload room).
    ///
    /// The field is private, so this and [`RoomMeta::is_snapshottable`] are the
    /// only read paths onto the authoritative lineage.
    #[must_use]
    pub const fn provenance(&self) -> Option<Provenance> {
        self.provenance
    }

    /// Whether the snapshot path may freeze this room — `true` only for a base
    /// sealed to `Neutral`. The consumption point for `rooms snapshot`.
    #[must_use]
    pub fn is_snapshottable(&self) -> bool {
        self.provenance.is_some_and(Provenance::is_snapshottable)
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

/// Classify liveness from a `/proc/<pid>/stat` line AND verify the process
/// incarnation via its start time. Pure, so the recycled-pid case is
/// unit-testable.
///
/// comm/zombie is checked first (a dead, zombie, or foreign-`comm` pid is `Dead`
/// regardless of start time). Then, when a start time was recorded, a mismatch
/// means the pid was recycled to a *different* process incarnation → `Dead`; a
/// match → `Alive`; an unparseable stat → `Unknown` (fail-safe). With no recorded
/// start time (a pre-v2 `room.json`), this is comm-only — the prior behavior.
#[must_use]
pub fn classify_stat_with_identity(stat: &str, expected_starttime: Option<u64>) -> Liveness {
    let live = classify_stat(stat);
    if live != Liveness::Alive {
        return live;
    }
    let Some(expected) = expected_starttime else {
        return Liveness::Alive;
    };
    match parse_starttime(stat) {
        Some(actual) if actual == expected => Liveness::Alive,
        Some(_) => Liveness::Dead,
        None => Liveness::Unknown,
    }
}

/// Parse the process start time — field 22 of `/proc/<pid>/stat`, the value that
/// pins a pid to a specific incarnation. `None` if the line is malformed.
///
/// Fields after the (parenthesized, space-permitting) `comm` are space-separated;
/// counting from `state` (field 3) right after the last `)`, start time is the
/// 20th token (field 22 overall).
#[must_use]
pub fn parse_starttime(stat: &str) -> Option<u64> {
    let close = stat.rfind(')')?;
    stat.get(close + 1..)?
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()
}

/// Read the start time (field 22 of `/proc/<pid>/stat`) of a live pid, to record
/// at boot. `None` off Linux, or if the pid's stat can't be read/parsed.
#[cfg(target_os = "linux")]
#[must_use]
pub fn starttime_of(pid: u32) -> Option<u64> {
    parse_starttime(&std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
}

#[cfg(not(target_os = "linux"))]
#[allow(
    clippy::missing_const_for_fn,
    reason = "kept non-const to match the Linux starttime_of that reads /proc"
)]
#[must_use]
pub fn starttime_of(_pid: u32) -> Option<u64> {
    None
}

/// Probe a room's liveness via `/proc/<pid>/stat`.
///
/// Uid-independent and pid-reuse-aware, unlike `kill -0` (which returns EPERM
/// cross-uid and reads as a false "dead", breaking `rooms ls` run as the
/// unprivileged operator against a firecracker-uid process). A missing
/// `/proc/<pid>` is `Dead`; a zombie is `Dead` (see `classify_stat`); an
/// unreadable entry is `Unknown` (fail-safe: never claim dead when we can't tell
/// — gc won't reap it). When `expected_starttime` is `Some`, a pid recycled to a
/// *different* incarnation (even another firecracker) reads `Dead` — the identity
/// check `rooms kill` relies on before signaling.
#[must_use]
pub fn probe(pid: Option<u32>, expected_starttime: Option<u64>) -> Liveness {
    let Some(pid) = pid else {
        return Liveness::Unknown;
    };
    probe_pid(pid, expected_starttime)
}

#[cfg(target_os = "linux")]
fn probe_pid(pid: u32, expected_starttime: Option<u64>) -> Liveness {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => classify_stat_with_identity(&stat, expected_starttime),
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
fn probe_pid(_pid: u32, _expected_starttime: Option<u64>) -> Liveness {
    Liveness::Unknown
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, reason = "test module")]

    use super::{
        classify_comm, classify_stat, classify_stat_with_identity, parse_starttime, probe, read,
        write_atomic, IllegalSeal, Liveness, Provenance, RoomMeta, Slot,
    };
    use chrono::Utc;
    use std::net::Ipv4Addr;

    fn sample(pid: Option<u32>) -> RoomMeta {
        RoomMeta::new(
            "01abcdefghijklmnopqrstuvwx".to_owned(),
            Some("id".to_owned()),
            pid,
            pid.map(u64::from),
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
    fn meta_round_trips_with_slot() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut meta = sample(Some(42));
        meta.slot = Some(Slot {
            index: 3,
            tap: "tap-fc3".to_owned(),
            gateway: Ipv4Addr::new(172, 16, 0, 13),
            guest: Ipv4Addr::new(172, 16, 0, 14),
            prefix: 30,
        });
        write_atomic(dir.path(), &meta).expect("write");
        let back = read(dir.path()).expect("read").expect("present");
        assert_eq!(meta, back);
    }

    #[test]
    fn slotless_meta_serializes_without_a_slot_key() {
        // Legacy rooms must keep their exact file shape: no `"slot": null`.
        let json = serde_json::to_string(&sample(None)).expect("serialize");
        assert!(!json.contains("slot"), "absent slot must not serialize");
    }

    #[test]
    fn v2_meta_without_slot_still_reads() {
        // A pre-v3 room.json (no slot field) must still deserialize, with the
        // field defaulting to None (a legacy shared-tap room) — back-compat.
        let dir = tempfile::tempdir().expect("tempdir");
        let v2 = r#"{"schema_version":2,"id":"01abcdefghijklmnopqrstuvwx","label":null,"started_at":"2026-06-28T00:00:00Z","pid":42,"pid_starttime":7,"keep":false}"#;
        std::fs::write(dir.path().join("room.json"), v2).expect("write v2");
        let back = read(dir.path()).expect("read").expect("present");
        assert_eq!(back.slot, None);
    }

    #[test]
    fn v1_meta_without_starttime_still_reads() {
        // A pre-v2 room.json (no pid_starttime field) must still deserialize, with
        // the field defaulting to None (comm-only fallback) — back-compat.
        let dir = tempfile::tempdir().expect("tempdir");
        let v1 = r#"{"schema_version":1,"id":"01abcdefghijklmnopqrstuvwx","label":null,"started_at":"2026-06-28T00:00:00Z","pid":42,"keep":false}"#;
        std::fs::write(dir.path().join("room.json"), v1).expect("write v1");
        let back = read(dir.path()).expect("read").expect("present");
        assert_eq!(back.pid, Some(42));
        assert_eq!(back.pid_starttime, None);
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

    // A synthetic /proc/<pid>/stat with 22 fields; start time (field 22) is the
    // 20th token after the comm's closing `)`.
    const FC_STAT_ST555: &str =
        "1234 (firecracker) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 555";

    #[test]
    fn parse_starttime_reads_field_22() {
        assert_eq!(parse_starttime(FC_STAT_ST555), Some(555));
        // comm with spaces/parens: still split on the LAST ')'.
        assert_eq!(
            parse_starttime("5 (odd ) name) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 777"),
            Some(777)
        );
        assert_eq!(parse_starttime("garbage-no-parens"), None);
    }

    #[test]
    fn identity_check_distinguishes_a_recycled_pid_from_the_same_incarnation() {
        // The precise hole the adversarial pass found: a recycled pid that IS a
        // firecracker (comm matches) must read Dead when its start time differs.
        assert_eq!(
            classify_stat_with_identity(FC_STAT_ST555, Some(555)),
            Liveness::Alive,
            "same incarnation (comm + starttime match) is alive"
        );
        assert_eq!(
            classify_stat_with_identity(FC_STAT_ST555, Some(999)),
            Liveness::Dead,
            "a different starttime means the pid was recycled — not this room's fc"
        );
        assert_eq!(
            classify_stat_with_identity(FC_STAT_ST555, None),
            Liveness::Alive,
            "no recorded starttime (pre-v2 meta) falls back to comm-only"
        );
        // comm/zombie short-circuits before the starttime check.
        assert_eq!(
            classify_stat_with_identity(
                "1234 (firecracker) Z 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 555",
                Some(555)
            ),
            Liveness::Dead,
            "a zombie is dead regardless of starttime"
        );
        assert_eq!(
            classify_stat_with_identity(
                "1234 (bash) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 555",
                Some(555)
            ),
            Liveness::Dead,
            "a foreign comm is dead regardless of starttime"
        );
    }

    #[test]
    fn probe_without_pid_is_unknown() {
        // No pid recorded ⇒ indeterminate ⇒ never reaped.
        assert_eq!(probe(None, None), Liveness::Unknown);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn probe_absent_pid_is_dead() {
        // A pid that cannot exist (>= 2^22 on Linux) has no /proc entry.
        assert_eq!(probe(Some(4_194_305), None), Liveness::Dead);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn probe_self_is_dead_by_name() {
        // The test process is alive but isn't firecracker/jailer — exactly the
        // pid-reuse case. Liveness keys on the process *identity*, not mere
        // existence, so this must read Dead.
        let me = std::process::id();
        assert_eq!(probe(Some(me), None), Liveness::Dead);
    }

    // ---- provenance: the sealed-neutral-base security primitive ----

    #[test]
    fn seal_is_legal_only_from_provisioning() {
        assert_eq!(Provenance::Provisioning.seal(), Ok(Provenance::Neutral));
        assert_eq!(
            Provenance::Neutral.seal(),
            Err(IllegalSeal(Some(Provenance::Neutral))),
            "an already-neutral base must not re-seal"
        );
        assert_eq!(
            Provenance::Tainted.seal(),
            Err(IllegalSeal(Some(Provenance::Tainted))),
            "a tainted base must never present as neutral — the load-bearing rule"
        );
    }

    #[test]
    fn only_neutral_is_snapshottable() {
        assert!(!Provenance::Provisioning.is_snapshottable());
        assert!(Provenance::Neutral.is_snapshottable());
        assert!(!Provenance::Tainted.is_snapshottable());
    }

    #[test]
    fn base_seals_once_then_refuses_reseal_and_taint_is_terminal() {
        let mut base = sample(Some(1)).as_base();
        assert_eq!(base.provenance, Some(Provenance::Provisioning));
        assert!(
            !base.is_snapshottable(),
            "a provisioning base is not yet freezable"
        );

        base.seal().expect("a provisioning base seals to neutral");
        assert_eq!(base.provenance, Some(Provenance::Neutral));
        assert!(base.is_snapshottable(), "a sealed base is freezable");

        // Sealing again is refused — no re-seal.
        assert_eq!(base.seal(), Err(IllegalSeal(Some(Provenance::Neutral))));

        // Taint is irreversible and terminal: it poisons, and seal never recovers.
        base.taint();
        assert_eq!(base.provenance, Some(Provenance::Tainted));
        assert!(
            !base.is_snapshottable(),
            "a tainted base is never freezable"
        );
        assert!(
            base.seal().is_err(),
            "a tainted base never re-seals to neutral"
        );
    }

    #[test]
    fn as_base_never_resets_an_established_provenance() {
        // The security hole: as_base on a loaded Tainted (or Neutral) room must
        // NOT reset it to Provisioning — otherwise seal() could re-neutralize a
        // tainted base. Only fresh (None) metadata is promoted.
        let mut tainted = sample(Some(1)).as_base();
        tainted.taint();
        assert_eq!(tainted.provenance, Some(Provenance::Tainted));
        let mut still_tainted = tainted.as_base();
        assert_eq!(
            still_tainted.provenance,
            Some(Provenance::Tainted),
            "as_base must not resurrect a tainted room to provisioning"
        );
        assert!(
            still_tainted.seal().is_err(),
            "and so it can never be re-sealed to neutral"
        );

        // Same guard for an already-neutral base: no reset, stays snapshottable-as-is.
        let mut sealed = sample(Some(2)).as_base();
        sealed.seal().expect("seal");
        let re = sealed.as_base();
        assert_eq!(re.provenance, Some(Provenance::Neutral));
    }

    #[test]
    fn taint_is_idempotent() {
        let mut base = sample(Some(1)).as_base();
        base.taint();
        base.taint(); // a second taint must not change or resurrect the state
        assert_eq!(base.provenance, Some(Provenance::Tainted));
    }

    #[test]
    fn non_base_room_is_unsealable_and_never_snapshottable() {
        let mut plain = sample(Some(1)); // a plain workload room: provenance None
        assert_eq!(plain.provenance, None);
        assert!(!plain.is_snapshottable());
        assert_eq!(
            plain.seal(),
            Err(IllegalSeal(None)),
            "a room with no provenance is not a base and cannot be sealed"
        );
        plain.taint();
        assert_eq!(
            plain.provenance, None,
            "tainting a non-base room stays None"
        );
    }

    #[test]
    fn provenance_round_trips_and_serializes_lowercase() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut meta = sample(Some(7)).as_base();
        meta.seal().expect("seal");
        write_atomic(dir.path(), &meta).expect("write");
        let back = read(dir.path()).expect("read").expect("present");
        assert_eq!(back.provenance, Some(Provenance::Neutral));

        let json = serde_json::to_string(&meta).expect("serialize");
        assert!(
            json.contains("\"provenance\":\"neutral\""),
            "provenance serializes as a lowercase tag, got {json}"
        );
    }

    #[test]
    fn provenanceless_room_omits_the_key() {
        // A plain workload room must keep its exact file shape: no "provenance".
        let json = serde_json::to_string(&sample(None)).expect("serialize");
        assert!(
            !json.contains("provenance"),
            "absent provenance must not serialize"
        );
    }

    #[test]
    fn v3_meta_without_provenance_still_reads() {
        // A pre-v4 room.json (no provenance field) must still deserialize, with
        // the field defaulting to None (a plain workload room) — back-compat.
        let dir = tempfile::tempdir().expect("tempdir");
        let v3 = r#"{"schema_version":3,"id":"01abcdefghijklmnopqrstuvwx","label":null,"started_at":"2026-06-28T00:00:00Z","pid":42,"pid_starttime":7,"keep":false}"#;
        std::fs::write(dir.path().join("room.json"), v3).expect("write v3");
        let back = read(dir.path()).expect("read").expect("present");
        assert_eq!(back.provenance, None);
    }
}
