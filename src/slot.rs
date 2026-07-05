//! Per-room network slot allocation — `O_EXCL` slot files under the state base.
//!
//! Pool slot k (1-based) owns the /30 at `172.16.0.4k/30`: tap `tap-fc<k>`,
//! gateway `.4k+1`, guest `.4k+2`. Slot 0 is reserved — its /30 derives the
//! legacy shared-tap addresses byte-for-byte (`tap-fc0` / `172.16.0.1` / `.2`)
//! and would collide with the legacy path while both coexist, so the allocator
//! walks k = 1..=cap.
//!
//! Allocation truth is the slot *file* `<state>/slots/<k>`: [`claim`] is an
//! `O_CREAT|O_EXCL` create (the filesystem is the race arbiter — two racers on
//! one index get exactly one winner, no daemon, no lock held over a room's
//! lifetime), [`free`] is a compare-and-delete whose verify+unlink runs under
//! a short-lived free-lock, and each file carries its claimer's own liveness
//! token so [`reconcile`] can judge a leaked slot before any `room.json`
//! exists.
//! `O_EXCL` is atomic on a local filesystem only — not reliably over NFS — so
//! the state base must stay local (doctor enforces this once gc wiring lands).

use std::io::Write;
use std::net::Ipv4Addr;
use std::path::Path;

use tracing::warn;

use crate::error::SlotError;
use crate::room::Liveness;
pub use crate::room::Slot;

/// Directory under the state base holding one lock file per claimed slot.
pub const SLOTS_DIR: &str = "slots";

/// The free-lock file beside the slots dir, serializing every verify+unlink
/// critical section ([`free`] and [`reconcile`]'s removal). Claims never take
/// it — `O_EXCL` creation cannot overwrite anyone.
const FREE_LOCK: &str = "slots.lock";

/// Highest claimable pool slot index: the /24 carve yields 64 /30s, minus the
/// reserved slot 0.
pub const MAX_SLOT: u8 = 63;

/// Identity of the process performing a claim.
///
/// A pid pinned to its incarnation via `/proc/<pid>/stat` start time — the
/// same tuple `room::probe` keys on. Written into the slot file so reconcile
/// can probe the claimer directly.
#[derive(Debug, Clone, Copy)]
pub struct Claimer {
    pub pid: u32,
    pub starttime: u64,
}

/// Claim a slot for `room_id`, whose identity the caller pre-minted.
///
/// With `target: None`, walks k = 1..=cap (clamped to [`MAX_SLOT`]) and
/// `O_EXCL`-creates `<state>/slots/<k>` — first success wins; a lost race
/// advances to k+1; every index claimed is [`SlotError::PoolFull`].
///
/// With `target: Some(k)`, attempts exactly index k (the reserve-by-index hook
/// for snapshot restore, which must reclaim the IP frozen into its snapshot):
/// a taken index is [`SlotError::TargetTaken`], never a silent fallback. The
/// target is bounded by [`MAX_SLOT`] but not by `cap` — how a restore target
/// interacts with a since-lowered cap is settled in the snapshots design.
pub fn claim(
    state: &Path,
    room_id: &str,
    me: Claimer,
    cap: u8,
    target: Option<u8>,
) -> Result<Slot, SlotError> {
    let dir = state.join(SLOTS_DIR);
    std::fs::create_dir_all(&dir)?;
    if let Some(index) = target {
        return claim_target(&dir, room_id, me, index);
    }
    // The error reports the effective cap — the pool size actually walked.
    let cap = cap.min(MAX_SLOT);
    for index in 1..=cap {
        if try_claim(&dir, index, room_id, me)? {
            return Ok(derive(index));
        }
    }
    Err(SlotError::PoolFull { cap })
}

/// Attempt exactly one requested index (reserve-by-index).
fn claim_target(dir: &Path, room_id: &str, me: Claimer, index: u8) -> Result<Slot, SlotError> {
    if index == 0 || index > MAX_SLOT {
        return Err(SlotError::InvalidTarget {
            index,
            max: MAX_SLOT,
        });
    }
    if try_claim(dir, index, room_id, me)? {
        return Ok(derive(index));
    }
    Err(SlotError::TargetTaken { index })
}

/// One `O_EXCL` attempt on `slots/<index>`. `Ok(false)` = lost the race (the
/// file already exists). The token is written through the same handle the
/// create returned, so a crash between create and write leaves an empty file —
/// the explicit claim-in-progress state readers skip, never a half-claim.
fn try_claim(dir: &Path, index: u8, room_id: &str, me: Claimer) -> Result<bool, SlotError> {
    let path = dir.join(index.to_string());
    let open = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path);
    let mut file = match open {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(false),
        // Windows reports ACCESS_DENIED for a create racing a delete-pending
        // file (a slot mid-free), where unix reports the file as existing.
        // Production hosts are Linux; on the Windows dev/CI platform treat it
        // as a lost race — never a claim failure — so racing tests hold there.
        #[cfg(windows)]
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => return Ok(false),
        Err(e) => return Err(SlotError::Io(e)),
    };
    let token = format!("{room_id}\n{} {}\n", me.pid, me.starttime);
    if let Err(e) = file.write_all(token.as_bytes()) {
        // We exclusively created this file and no token committed, so removing
        // it robs no one — and leaving it would wedge the index as
        // claim-in-progress forever after a transient failure (ENOSPC).
        drop(file);
        if let Err(rm) = std::fs::remove_file(&path) {
            warn!(index, error = %rm, "could not remove half-claimed slot file");
        }
        return Err(SlotError::Io(e));
    }
    Ok(true)
}

/// Take the exclusive free-lock, released when the returned handle drops.
///
/// Held only across a verify+unlink critical section — milliseconds, never a
/// room's lifetime — so the crash story stays the OS's: an exiting process
/// releases it. Without this lock, two frees of the same room can interleave
/// with a fresh claim of the index and unlink the *new* room's file, robbing
/// a live claim and re-opening double allocation.
fn lock_frees(state: &Path) -> Result<std::fs::File, SlotError> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(state.join(FREE_LOCK))?;
    file.lock()?;
    Ok(file)
}

/// Derive slot k's network identity: the /30 at `172.16.0.4k`. Callers
/// validate `index` (1..=[`MAX_SLOT`]) before deriving.
fn derive(index: u8) -> Slot {
    let base = 4 * index;
    Slot {
        index,
        tap: format!("tap-fc{index}"),
        gateway: Ipv4Addr::new(172, 16, 0, base + 1),
        guest: Ipv4Addr::new(172, 16, 0, base + 2),
        prefix: 30,
    }
}

/// What [`free`] found at the slot index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freed {
    /// The file named `expected_room_id` and was deleted.
    Removed,
    /// No slot file — already freed (idempotent teardown retry).
    AlreadyFree,
    /// The file names a *different* room: the index was reclaimed and
    /// reassigned since this room recorded it. Left untouched.
    AlreadyReassigned,
}

/// Free `slot_index`, but only if its file still names `expected_room_id`.
///
/// Compare-and-delete, never blind remove: a stale `room.json` driving
/// teardown against a reused index must not free a *live* sibling's slot
/// (the same never-act-on-a-reused-identity rule as `terminate_by_identity`).
pub fn free(state: &Path, slot_index: u8, expected_room_id: &str) -> Result<Freed, SlotError> {
    let path = state.join(SLOTS_DIR).join(slot_index.to_string());
    let _lock = lock_frees(state)?;
    let contents = match std::fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Freed::AlreadyFree),
        Err(e) => return Err(SlotError::Io(e)),
    };
    if contents.lines().next() != Some(expected_room_id) {
        return Ok(Freed::AlreadyReassigned);
    }
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(Freed::Removed),
        // Lost a delete race — the slot is free either way.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Freed::AlreadyFree),
        Err(e) => Err(SlotError::Io(e)),
    }
}

/// One leaked slot found by [`reconcile`]: its claimer is confirmed dead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reclaimed {
    pub index: u8,
    pub room_id: String,
    /// True when the slot file was removed here (no room dir existed to
    /// consider). False when a room dir remains — releasing the slot then
    /// belongs to gc's reap flow, which must also tombstone the dir so a
    /// single corpse can't drive two frees against a reused index.
    pub removed: bool,
}

/// Scan `<state>/slots/` for leaked claims, judging each slot file by its own
/// claimer token — no `room.json` required, so every pre-registry crash window
/// is covered.
///
/// Per file: an empty/short/unparseable token is claim-in-progress → skip
/// (never reclaim; a later pass re-checks). A parsed token probes the recorded
/// claimer identity: alive or unknown → skip (never reclaim a live or
/// unprovable claim); confirmed dead → returned, and the slot file is removed
/// when no room dir exists for the recorded id. Scan errors downgrade to a
/// warning — reconcile is a best-effort sweep, not a gate.
pub fn reconcile(state: &Path) -> Vec<Reclaimed> {
    let dir = state.join(SLOTS_DIR);
    let read_dir = match std::fs::read_dir(&dir) {
        Ok(read_dir) => read_dir,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(e) => {
            warn!(dir = %dir.display(), error = %e, "cannot scan slots dir; skipping reconcile");
            return Vec::new();
        }
    };
    let mut reclaimed = Vec::new();
    for dirent in read_dir.flatten() {
        let Some(index) = slot_index_of(&dirent.file_name()) else {
            continue;
        };
        if let Some(entry) = reconcile_slot(state, &dirent.path(), index) {
            reclaimed.push(entry);
        }
    }
    reclaimed
}

/// Parse a `slots/` file name as a claimable index; `None` for strays
/// (`0`, `64`, temp files) — never touched.
fn slot_index_of(name: &std::ffi::OsStr) -> Option<u8> {
    let index: u8 = name.to_str()?.parse().ok()?;
    (1..=MAX_SLOT).contains(&index).then_some(index)
}

/// Judge one slot file; `Some` only for a confirmed-dead claimer.
fn reconcile_slot(state: &Path, path: &Path, index: u8) -> Option<Reclaimed> {
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(e) => {
            warn!(index, error = %e, "cannot read slot file; skipping");
            return None;
        }
    };
    let SlotToken::Claimed {
        room_id,
        pid,
        starttime,
    } = parse_token(&contents)
    else {
        return None;
    };
    if claimer_liveness(pid, starttime) != Liveness::Dead {
        return None;
    }
    // Room-id shape was validated by parse_token, so this join can't traverse.
    if state.join(&room_id).is_dir() {
        return Some(Reclaimed {
            index,
            room_id,
            removed: false,
        });
    }
    remove_if_unchanged(state, path, &contents, index).then_some(Reclaimed {
        index,
        room_id,
        removed: true,
    })
}

/// Unlink a leaked slot file, but only if it still holds `expected` — under
/// the free-lock, and re-verified there, so the index churning (freed and
/// re-claimed by a live room) between this pass's read and its unlink can
/// never unlink the new claim.
fn remove_if_unchanged(state: &Path, path: &Path, expected: &str, index: u8) -> bool {
    let removed = (|| -> Result<bool, SlotError> {
        let _lock = lock_frees(state)?;
        let now = match std::fs::read_to_string(path) {
            Ok(now) => now,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(SlotError::Io(e)),
        };
        if now != expected {
            return Ok(false);
        }
        std::fs::remove_file(path)?;
        Ok(true)
    })();
    match removed {
        Ok(removed) => removed,
        Err(e) => {
            warn!(index, error = %e, "cannot remove leaked slot file; leaving for the next pass");
            false
        }
    }
}

/// A slot file's parsed contents.
#[derive(Debug, PartialEq, Eq)]
enum SlotToken {
    /// Empty, short, or unparseable — a crash between the `O_EXCL` create and
    /// the token write (or garbage). Never reclaimed.
    InProgress,
    Claimed {
        room_id: String,
        pid: u32,
        starttime: u64,
    },
}

/// Parse `<room_id>\n<pid> <starttime>\n`. Anything that doesn't fully parse —
/// including a malformed room id, which also gates the path join in
/// [`reconcile_slot`] against traversal — is [`SlotToken::InProgress`].
fn parse_token(contents: &str) -> SlotToken {
    // The trailing newline is the commit marker — the last byte claim writes.
    // A crash can truncate the token mid-number ("42 777" → "42 7"), which
    // would parse cleanly, probe the WRONG incarnation, read it dead, and
    // reclaim a live claimer's slot. An unterminated token is in-progress.
    if !contents.ends_with('\n') {
        return SlotToken::InProgress;
    }
    let mut lines = contents.lines();
    let Some(room_id) = lines.next() else {
        return SlotToken::InProgress;
    };
    // Mirrors `registry::is_valid_room_id` (26 lowercase-alphanumerics); a
    // local copy keeps this layer free of a sibling import.
    let id_shaped = room_id.len() == 26
        && room_id
            .bytes()
            .all(|b| b.is_ascii_digit() || b.is_ascii_lowercase());
    if !id_shaped {
        return SlotToken::InProgress;
    }
    let Some(token_line) = lines.next() else {
        return SlotToken::InProgress;
    };
    let mut parts = token_line.split_whitespace();
    let pid = parts.next().and_then(|s| s.parse().ok());
    let starttime = parts.next().and_then(|s| s.parse().ok());
    let (Some(pid), Some(starttime)) = (pid, starttime) else {
        return SlotToken::InProgress;
    };
    SlotToken::Claimed {
        room_id: room_id.to_owned(),
        pid,
        starttime,
    }
}

/// Classify a claimer's `/proc/<pid>/stat` line against its recorded start
/// time. Pure, so the recycled-pid case is unit-testable everywhere.
///
/// Unlike `room::classify_stat` this is comm-agnostic — the claimer is
/// whatever process ran `rooms run`, not firecracker — so identity rests on
/// the (pid, starttime) incarnation pin alone. A zombie is dead (it can never
/// finish its claim); a starttime mismatch is a recycled pid → dead; an
/// unparseable stat is unknown (fail-safe: never reclaimed).
#[must_use]
pub fn classify_claimer_stat(stat: &str, expected_starttime: u64) -> Liveness {
    let Some(close) = stat.rfind(')') else {
        return Liveness::Unknown;
    };
    let state = stat
        .get(close + 1..)
        .and_then(|rest| rest.trim_start().chars().next());
    if matches!(state, Some('Z' | 'X' | 'x')) {
        return Liveness::Dead;
    }
    match crate::room::parse_starttime(stat) {
        Some(actual) if actual == expected_starttime => Liveness::Alive,
        Some(_) => Liveness::Dead,
        None => Liveness::Unknown,
    }
}

/// Probe a claimer's liveness via `/proc/<pid>/stat`. A missing entry is dead;
/// an unreadable one is unknown (fail-safe).
#[cfg(target_os = "linux")]
fn claimer_liveness(pid: u32, starttime: u64) -> Liveness {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => classify_claimer_stat(&stat, starttime),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Liveness::Dead,
        Err(_) => Liveness::Unknown,
    }
}

/// Off Linux there is no `/proc` to consult — unknown, so nothing reclaims.
#[cfg(not(target_os = "linux"))]
#[allow(
    clippy::missing_const_for_fn,
    reason = "kept non-const to match the Linux claimer_liveness that reads /proc"
)]
fn claimer_liveness(_pid: u32, _starttime: u64) -> Liveness {
    Liveness::Unknown
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

    use super::{
        claim, classify_claimer_stat, free, parse_token, reconcile, Claimer, Freed, Liveness,
        SlotError, SlotToken, MAX_SLOT, SLOTS_DIR,
    };
    use std::net::Ipv4Addr;
    use std::path::Path;

    const ME: Claimer = Claimer {
        pid: 1,
        starttime: 1,
    };

    /// A distinct, id-shaped (26 lowercase-alphanumeric) room id per `n`.
    fn room_id(n: u32) -> String {
        format!("{n:026}")
    }

    fn slot_path(state: &Path, index: u8) -> std::path::PathBuf {
        state.join(SLOTS_DIR).join(index.to_string())
    }

    #[test]
    fn derived_addresses_match_the_30_carve() {
        let dir = tempfile::tempdir().unwrap();
        let slot = claim(dir.path(), &room_id(1), ME, 8, None).unwrap();
        assert_eq!(slot.index, 1);
        assert_eq!(slot.tap, "tap-fc1");
        assert_eq!(slot.gateway, Ipv4Addr::new(172, 16, 0, 5));
        assert_eq!(slot.guest, Ipv4Addr::new(172, 16, 0, 6));
        assert_eq!(slot.prefix, 30);
        // The top of the carve still fits the octet: 4·63+2 = 254.
        let top = claim(dir.path(), &room_id(2), ME, MAX_SLOT, Some(MAX_SLOT)).unwrap();
        assert_eq!(top.gateway, Ipv4Addr::new(172, 16, 0, 253));
        assert_eq!(top.guest, Ipv4Addr::new(172, 16, 0, 254));
    }

    #[test]
    fn claim_skips_reserved_slot_zero_and_walks_from_one() {
        let dir = tempfile::tempdir().unwrap();
        let slot = claim(dir.path(), &room_id(1), ME, 8, None).unwrap();
        assert_eq!(slot.index, 1, "the walk starts at 1 — slot 0 is reserved");
        assert!(!slot_path(dir.path(), 0).exists(), "slot 0 never claimed");
    }

    #[test]
    fn claim_writes_the_room_id_and_claimer_token() {
        let dir = tempfile::tempdir().unwrap();
        let id = room_id(7);
        let me = Claimer {
            pid: 4242,
            starttime: 999,
        };
        claim(dir.path(), &id, me, 8, None).unwrap();
        let contents = std::fs::read_to_string(slot_path(dir.path(), 1)).unwrap();
        assert_eq!(contents, format!("{id}\n4242 999\n"));
    }

    #[test]
    fn losers_advance_and_a_full_pool_is_a_distinct_error() {
        let dir = tempfile::tempdir().unwrap();
        let first = claim(dir.path(), &room_id(1), ME, 2, None).unwrap();
        let second = claim(dir.path(), &room_id(2), ME, 2, None).unwrap();
        assert_eq!((first.index, second.index), (1, 2));
        let full = claim(dir.path(), &room_id(3), ME, 2, None);
        assert!(
            matches!(full, Err(SlotError::PoolFull { cap: 2 })),
            "an exhausted pool must be PoolFull with the cap, got {full:?}"
        );
    }

    #[test]
    fn zero_cap_pool_is_immediately_full() {
        let dir = tempfile::tempdir().unwrap();
        let full = claim(dir.path(), &room_id(1), ME, 0, None);
        assert!(matches!(full, Err(SlotError::PoolFull { cap: 0 })));
    }

    #[test]
    fn cap_clamps_to_the_addressing_ceiling() {
        let dir = tempfile::tempdir().unwrap();
        // Fill every claimable slot, then one more with a cap far past 63:
        // the clamp must stop the walk at MAX_SLOT, not wander off the /24.
        for n in 1..=u32::from(MAX_SLOT) {
            claim(dir.path(), &room_id(n), ME, u8::MAX, None).unwrap();
        }
        let full = claim(dir.path(), &room_id(999), ME, u8::MAX, None);
        assert!(
            matches!(full, Err(SlotError::PoolFull { cap: MAX_SLOT })),
            "PoolFull must report the effective (clamped) cap, got {full:?}"
        );
        assert!(!slot_path(dir.path(), 64).exists(), "no claim past 63");
    }

    #[test]
    fn freed_index_is_reused_lowest_first() {
        let dir = tempfile::tempdir().unwrap();
        for n in 1..=3 {
            claim(dir.path(), &room_id(n), ME, 8, None).unwrap();
        }
        assert_eq!(free(dir.path(), 2, &room_id(2)).unwrap(), Freed::Removed);
        let next = claim(dir.path(), &room_id(4), ME, 8, None).unwrap();
        assert_eq!(next.index, 2, "the freed hole is refilled first");
    }

    #[test]
    fn target_index_claims_exactly_that_slot() {
        let dir = tempfile::tempdir().unwrap();
        let slot = claim(dir.path(), &room_id(1), ME, 8, Some(5)).unwrap();
        assert_eq!(slot.index, 5);
        assert!(slot_path(dir.path(), 5).exists());
        // A later first-free walk is unaffected by the hole below the target.
        let walk = claim(dir.path(), &room_id(2), ME, 8, None).unwrap();
        assert_eq!(walk.index, 1);
    }

    #[test]
    fn taken_target_errors_instead_of_falling_back() {
        let dir = tempfile::tempdir().unwrap();
        claim(dir.path(), &room_id(1), ME, 8, Some(5)).unwrap();
        let taken = claim(dir.path(), &room_id(2), ME, 8, Some(5));
        assert!(
            matches!(taken, Err(SlotError::TargetTaken { index: 5 })),
            "a reserve-by-index miss must never silently take another slot"
        );
    }

    #[test]
    fn reserved_and_out_of_range_targets_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        for bad in [0, MAX_SLOT + 1, u8::MAX] {
            let res = claim(dir.path(), &room_id(1), ME, 8, Some(bad));
            assert!(
                matches!(res, Err(SlotError::InvalidTarget { index, .. }) if index == bad),
                "target {bad} must be rejected"
            );
        }
    }

    #[test]
    fn free_mismatched_id_returns_already_reassigned() {
        let dir = tempfile::tempdir().unwrap();
        claim(dir.path(), &room_id(1), ME, 8, None).unwrap();
        let freed = free(dir.path(), 1, &room_id(2)).unwrap();
        assert_eq!(freed, Freed::AlreadyReassigned);
        assert!(
            slot_path(dir.path(), 1).exists(),
            "a mismatched free must leave the file — it belongs to another room"
        );
    }

    #[test]
    fn free_absent_slot_is_already_free() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            free(dir.path(), 1, &room_id(1)).unwrap(),
            Freed::AlreadyFree
        );
    }

    #[test]
    fn free_never_touches_a_claim_in_progress_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(SLOTS_DIR)).unwrap();
        std::fs::write(slot_path(dir.path(), 1), b"").unwrap();
        let freed = free(dir.path(), 1, &room_id(1)).unwrap();
        assert_eq!(freed, Freed::AlreadyReassigned);
        assert!(slot_path(dir.path(), 1).exists());
    }

    #[test]
    fn parse_token_treats_partial_writes_as_in_progress() {
        for partial in [
            "",
            "\n",
            &room_id(1),                            // id only, no token line
            &format!("{}\n", room_id(1)),           // id only, trailing newline
            &format!("{}\n42", room_id(1)),         // pid only, truncated
            &format!("{}\n42\n", room_id(1)),       // pid only, committed
            &format!("{}\n42 x\n", room_id(1)),     // unparseable starttime
            &format!("{}\n42 7", room_id(1)),       // complete-looking but uncommitted
            "short-id\n42 7\n",                     // malformed room id
            "../../../../../../etc/passwd\n42 7\n", // traversal-shaped id
        ] {
            assert_eq!(
                parse_token(partial),
                SlotToken::InProgress,
                "{partial:?} must read as claim-in-progress"
            );
        }
        assert_eq!(
            parse_token(&format!("{}\n42 7\n", room_id(1))),
            SlotToken::Claimed {
                room_id: room_id(1),
                pid: 42,
                starttime: 7
            }
        );
    }

    #[test]
    fn reconcile_skips_claim_in_progress() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(SLOTS_DIR)).unwrap();
        std::fs::write(slot_path(dir.path(), 1), b"").unwrap();
        std::fs::write(slot_path(dir.path(), 2), room_id(2)).unwrap();
        std::fs::write(slot_path(dir.path(), 3), b"garbage here").unwrap();
        assert_eq!(reconcile(dir.path()), Vec::new());
        for index in 1..=3 {
            assert!(
                slot_path(dir.path(), index).exists(),
                "slot {index}: an in-progress claim must never be reclaimed"
            );
        }
    }

    #[test]
    fn reconcile_ignores_stray_file_names() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(SLOTS_DIR)).unwrap();
        let token = format!("{}\n4194305 1\n", room_id(1));
        for stray in ["0", "64", "999", "abc", ".tmp"] {
            std::fs::write(dir.path().join(SLOTS_DIR).join(stray), &token).unwrap();
        }
        assert_eq!(reconcile(dir.path()), Vec::new());
        for stray in ["0", "64", "999", "abc", ".tmp"] {
            assert!(dir.path().join(SLOTS_DIR).join(stray).exists());
        }
    }

    #[test]
    fn reconcile_on_missing_slots_dir_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(reconcile(dir.path()), Vec::new());
    }

    #[cfg(target_os = "linux")]
    mod linux {
        use super::super::{claim, reconcile, Claimer, Reclaimed};
        use super::{room_id, slot_path};

        /// The test process's own (pid, starttime) — a genuinely live claimer.
        fn live_claimer() -> Claimer {
            let pid = std::process::id();
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
            let starttime = crate::room::parse_starttime(&stat).unwrap();
            Claimer { pid, starttime }
        }

        /// A pid that cannot exist on Linux (> `PID_MAX_LIMIT`) — confirmed dead.
        const DEAD: Claimer = Claimer {
            pid: 4_194_305,
            starttime: 1,
        };

        #[test]
        fn reconcile_reclaims_a_dead_claimer_with_no_room_dir() {
            let dir = tempfile::tempdir().unwrap();
            claim(dir.path(), &room_id(1), DEAD, 8, None).unwrap();
            let reclaimed = reconcile(dir.path());
            assert_eq!(
                reclaimed,
                vec![Reclaimed {
                    index: 1,
                    room_id: room_id(1),
                    removed: true
                }]
            );
            assert!(!slot_path(dir.path(), 1).exists(), "leaked slot reclaimed");
        }

        #[test]
        fn reconcile_defers_to_gc_when_a_room_dir_exists() {
            let dir = tempfile::tempdir().unwrap();
            claim(dir.path(), &room_id(1), DEAD, 8, None).unwrap();
            std::fs::create_dir(dir.path().join(room_id(1))).unwrap();
            let reclaimed = reconcile(dir.path());
            assert_eq!(
                reclaimed,
                vec![Reclaimed {
                    index: 1,
                    room_id: room_id(1),
                    removed: false
                }]
            );
            assert!(
                slot_path(dir.path(), 1).exists(),
                "with a room dir present, releasing the slot is gc's call"
            );
        }

        #[test]
        fn reconcile_never_reclaims_a_live_claimer() {
            let dir = tempfile::tempdir().unwrap();
            claim(dir.path(), &room_id(1), live_claimer(), 8, None).unwrap();
            assert_eq!(reconcile(dir.path()), Vec::new());
            assert!(slot_path(dir.path(), 1).exists());
        }

        #[test]
        fn reconcile_reclaims_a_recycled_pid_as_dead() {
            // The claimer pid exists (it's this test process) but the recorded
            // starttime names a *different* incarnation — the pid was recycled,
            // so the claim is dead.
            let dir = tempfile::tempdir().unwrap();
            let mut me = live_claimer();
            me.starttime = me.starttime.wrapping_add(1);
            claim(dir.path(), &room_id(1), me, 8, None).unwrap();
            let reclaimed = reconcile(dir.path());
            assert_eq!(reclaimed.len(), 1);
            assert!(!slot_path(dir.path(), 1).exists());
        }

        #[test]
        fn free_absent_after_reconcile_is_idempotent() {
            let dir = tempfile::tempdir().unwrap();
            claim(dir.path(), &room_id(1), DEAD, 8, None).unwrap();
            reconcile(dir.path());
            // The crashed room's late teardown retry must be a clean no-op.
            let freed = super::super::free(dir.path(), 1, &room_id(1)).unwrap();
            assert_eq!(freed, super::super::Freed::AlreadyFree);
        }
    }

    // A synthetic /proc/<pid>/stat with 22 fields; starttime (field 22) = 555.
    const CLAIMER_STAT: &str = "1234 (rooms) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 555";

    #[test]
    fn classify_claimer_is_comm_agnostic_and_identity_pinned() {
        // The claimer is whatever ran `rooms run` — any comm counts, identity
        // rests on the starttime pin alone.
        assert_eq!(classify_claimer_stat(CLAIMER_STAT, 555), Liveness::Alive);
        assert_eq!(
            classify_claimer_stat(CLAIMER_STAT, 999),
            Liveness::Dead,
            "a starttime mismatch is a recycled pid"
        );
        assert_eq!(
            classify_claimer_stat(
                "1234 (rooms) Z 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 555",
                555
            ),
            Liveness::Dead,
            "a zombie claimer can never finish its claim"
        );
        assert_eq!(
            classify_claimer_stat("garbage-no-parens", 555),
            Liveness::Unknown,
            "unparseable stat is unknown — never reclaimed"
        );
    }

    /// Every claimable derivation, exhaustively: addresses stay inside the
    /// `172.16.0.0/24` carve, follow the 4k+1/4k+2 shape, never collide across
    /// slots, and never touch the legacy shared-tap pair (`.1`/`.2`).
    #[test]
    fn all_derivations_are_disjoint_and_inside_the_carve() {
        let dir = tempfile::tempdir().unwrap();
        let mut octets = std::collections::HashSet::new();
        let mut taps = std::collections::HashSet::new();
        for k in 1..=MAX_SLOT {
            let slot = claim(dir.path(), &room_id(k.into()), ME, MAX_SLOT, Some(k)).unwrap();
            let [a, b, c, gw] = slot.gateway.octets();
            assert_eq!((a, b, c), (172, 16, 0), "gateway inside the supernet");
            assert_eq!(slot.guest.octets()[..3], [172, 16, 0]);
            let guest = slot.guest.octets()[3];
            assert_eq!((gw, guest), (4 * k + 1, 4 * k + 2), "the /30 shape");
            assert!(gw >= 5, "legacy .1/.2 never derived — slot 0 is reserved");
            assert!(
                octets.insert(gw) && octets.insert(guest),
                "slot {k} reuses an address"
            );
            assert!(taps.insert(slot.tap.clone()), "slot {k} reuses a tap");
            assert_eq!(slot.prefix, 30);
        }
    }

    mod race {
        use super::super::{claim, free, Claimer, Freed, SlotError};
        use super::room_id;
        use proptest::prelude::*;
        use std::sync::{Arc, Barrier};

        /// Spawn one claimer thread per `n`, all released together on a
        /// barrier, and join their results in spawn order.
        fn race<F>(k: u8, run: F) -> Vec<Result<super::super::Slot, SlotError>>
        where
            F: Fn(u8) -> Result<super::super::Slot, SlotError> + Clone + Send + 'static,
        {
            let barrier = Arc::new(Barrier::new(usize::from(k)));
            let mut threads = Vec::with_capacity(usize::from(k));
            for n in 0..k {
                let barrier = Arc::clone(&barrier);
                let run = run.clone();
                threads.push(std::thread::spawn(move || {
                    barrier.wait();
                    run(n)
                }));
            }
            threads
                .into_iter()
                .map(|t| t.join().expect("claimer thread panicked"))
                .collect()
        }

        proptest! {
            // K claimers race S slots, K > S: exactly S winners on distinct
            // indices within 1..=S, and exactly K−S PoolFull losers. The
            // filesystem O_EXCL is the only serialization — no locks in the
            // test, all threads released together on a barrier.
            #[test]
            fn claim_race_exactly_s_winners(s in 1u8..=6, extra in 1u8..=6) {
                let k = s + extra;
                let dir = tempfile::tempdir().unwrap();
                let state = dir.path().to_path_buf();
                let results = race(k, move |n| {
                    let me = Claimer { pid: 1000 + u32::from(n), starttime: 1 };
                    claim(&state, &room_id(u32::from(n)), me, s, None)
                });

                let mut winners: Vec<u8> = results
                    .iter()
                    .filter_map(|r| r.as_ref().ok().map(|slot| slot.index))
                    .collect();
                winners.sort_unstable();
                let mut distinct = winners.clone();
                distinct.dedup();
                prop_assert_eq!(&winners, &distinct, "no index may be won twice");
                prop_assert_eq!(winners.len(), usize::from(s), "exactly S winners");
                prop_assert!(winners.iter().all(|&i| (1..=s).contains(&i)));

                let full = results
                    .iter()
                    .filter(|r| matches!(r, Err(SlotError::PoolFull { .. })))
                    .count();
                prop_assert_eq!(full, usize::from(extra), "K−S losers, all PoolFull");
            }

            // K claimers all reserve-by-index the SAME slot: exactly one
            // winner, K−1 TargetTaken — never a silent fallback elsewhere.
            #[test]
            fn target_race_has_exactly_one_winner(k in 2u8..=8, index in 1u8..=super::super::MAX_SLOT) {
                let dir = tempfile::tempdir().unwrap();
                let state = dir.path().to_path_buf();
                let results = race(k, move |n| {
                    let me = Claimer { pid: 1000 + u32::from(n), starttime: 1 };
                    claim(&state, &room_id(u32::from(n)), me, 8, Some(index))
                });

                let winners = results.iter().filter(|r| r.is_ok()).count();
                prop_assert_eq!(winners, 1, "exactly one winner for a contested target");
                let taken = results
                    .iter()
                    .filter(|r| matches!(r, Err(SlotError::TargetTaken { index: i }) if *i == index))
                    .count();
                prop_assert_eq!(taken, usize::from(k) - 1, "every loser sees TargetTaken");
                // The file records the winner's room id, no one else's.
                let winner = results.iter().position(Result::is_ok).unwrap();
                let winner = u32::try_from(winner).unwrap();
                let contents =
                    std::fs::read_to_string(super::slot_path(dir.path(), index)).unwrap();
                let winner_id = room_id(winner);
                prop_assert_eq!(contents.lines().next(), Some(winner_id.as_str()));
            }

            // The robbed-claim regression: T stale frees of a dead room race
            // one claimer re-taking the same index. The free-lock's
            // verify+unlink must never unlink the NEW room's file — exactly
            // one free removes the old claim, and the re-claimer's file
            // survives every straggler.
            #[test]
            fn concurrent_frees_never_rob_a_fresh_claim(freers in 2u8..=6) {
                let dir = tempfile::tempdir().unwrap();
                let state = dir.path().to_path_buf();
                let old = room_id(1);
                let me = Claimer { pid: 1, starttime: 1 };
                claim(&state, &old, me, 8, Some(1)).unwrap();

                let barrier = Arc::new(Barrier::new(usize::from(freers) + 1));
                let mut threads = Vec::with_capacity(usize::from(freers));
                for _ in 0..freers {
                    let state = state.clone();
                    let old = old.clone();
                    let barrier = Arc::clone(&barrier);
                    threads.push(std::thread::spawn(move || {
                        barrier.wait();
                        free(&state, 1, &old)
                    }));
                }
                let claimer = {
                    let barrier = Arc::clone(&barrier);
                    std::thread::spawn(move || {
                        let me = Claimer { pid: 2, starttime: 2 };
                        barrier.wait();
                        loop {
                            match claim(&state, &room_id(2), me, 8, Some(1)) {
                                Ok(slot) => return slot,
                                Err(SlotError::TargetTaken { .. }) => std::thread::yield_now(),
                                Err(e) => panic!("unexpected claim error: {e}"),
                            }
                        }
                    })
                };
                let freed: Vec<Freed> = threads
                    .into_iter()
                    .map(|t| t.join().expect("freer panicked").unwrap())
                    .collect();
                let slot = claimer.join().expect("claimer panicked");
                prop_assert_eq!(slot.index, 1);

                let removed = freed.iter().filter(|f| matches!(f, Freed::Removed)).count();
                prop_assert_eq!(removed, 1, "exactly one free removes the old claim");
                let contents =
                    std::fs::read_to_string(super::slot_path(dir.path(), 1)).unwrap();
                let new_id = room_id(2);
                prop_assert_eq!(
                    contents.lines().next(),
                    Some(new_id.as_str()),
                    "the re-claimer's file must survive every stale free"
                );
            }
        }
    }

    /// Model-based sequence test: random interleavings of claim / free-own /
    /// free-stale, checked op-by-op against a map of what the pool must hold.
    mod model {
        use super::super::{claim, free, Freed, SlotError, SLOTS_DIR};
        use super::{room_id, slot_path, ME};
        use proptest::prelude::*;
        use std::collections::BTreeMap;
        use std::path::Path;

        /// A room id no claim in this test ever mints.
        const STRANGER: u32 = 9_999_999;

        #[derive(Debug, Clone)]
        enum Op {
            /// Claim the next free slot for a freshly minted room.
            Claim,
            /// Free a currently-claimed slot with its owner's id (selector
            /// picks among the claimed).
            FreeOwn(u8),
            /// Free a slot index with a stale id that never owned it.
            FreeStale(u8),
        }

        fn op() -> impl Strategy<Value = Op> {
            prop_oneof![
                3 => Just(Op::Claim),
                2 => any::<u8>().prop_map(Op::FreeOwn),
                1 => any::<u8>().prop_map(Op::FreeStale),
            ]
        }

        /// The slots dir must mirror the model exactly: every modeled claim's
        /// file names its owner, every unmodeled index is absent.
        fn assert_disk_matches(
            state: &Path,
            cap: u8,
            model: &BTreeMap<u8, u32>,
        ) -> Result<(), TestCaseError> {
            for k in 1..=cap {
                let on_disk = std::fs::read_to_string(slot_path(state, k)).ok();
                match model.get(&k) {
                    Some(&owner) => {
                        let owner_id = room_id(owner);
                        prop_assert_eq!(
                            on_disk.as_deref().and_then(|c| c.lines().next()),
                            Some(owner_id.as_str()),
                            "slot {} must name its owner",
                            k
                        );
                    }
                    None => prop_assert!(on_disk.is_none(), "slot {} must be free", k),
                }
            }
            Ok(())
        }

        proptest! {
            #[test]
            fn claim_free_sequences_uphold_the_model(
                ops in proptest::collection::vec(op(), 1..32),
                cap in 1u8..=5,
            ) {
                let dir = tempfile::tempdir().unwrap();
                let state = dir.path();
                let mut model: BTreeMap<u8, u32> = BTreeMap::new();
                let mut minted = 0u32;
                for op in ops {
                    match op {
                        Op::Claim => {
                            minted += 1;
                            let got = claim(state, &room_id(minted), ME, cap, None);
                            match (1..=cap).find(|k| !model.contains_key(k)) {
                                Some(lowest_free) => {
                                    prop_assert_eq!(
                                        got.unwrap().index,
                                        lowest_free,
                                        "claim must take the lowest free index"
                                    );
                                    model.insert(lowest_free, minted);
                                }
                                None => prop_assert!(
                                    matches!(got, Err(SlotError::PoolFull { .. })),
                                    "a full pool must refuse with PoolFull"
                                ),
                            }
                        }
                        Op::FreeOwn(selector) => {
                            let picked = model
                                .iter()
                                .nth(usize::from(selector) % model.len().max(1))
                                .map(|(&k, &owner)| (k, owner));
                            let Some((k, owner)) = picked else { continue };
                            prop_assert_eq!(
                                free(state, k, &room_id(owner)).unwrap(),
                                Freed::Removed
                            );
                            model.remove(&k);
                        }
                        Op::FreeStale(selector) => {
                            let k = 1 + selector % cap;
                            let expected = if model.contains_key(&k) {
                                Freed::AlreadyReassigned
                            } else {
                                Freed::AlreadyFree
                            };
                            prop_assert_eq!(
                                free(state, k, &room_id(STRANGER)).unwrap(),
                                expected,
                                "a stale free must never remove a stranger's claim"
                            );
                        }
                    }
                    assert_disk_matches(state, cap, &model)?;
                }
                // Nothing outside the model may exist in the slots dir at all.
                let stray =
                    std::fs::read_dir(state.join(SLOTS_DIR)).map_or(0, Iterator::count);
                prop_assert_eq!(stray, model.len(), "no stray slot files");
            }
        }
    }

    /// The slot-file token grammar, property-checked: full tokens round-trip,
    /// every truncation reads as claim-in-progress, and no input panics.
    mod token {
        use super::super::{parse_token, SlotToken};
        use super::room_id;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn committed_token_round_trips(
                n in any::<u32>(),
                pid in any::<u32>(),
                starttime in any::<u64>(),
            ) {
                let contents = format!("{}\n{pid} {starttime}\n", room_id(n));
                prop_assert_eq!(
                    parse_token(&contents),
                    SlotToken::Claimed { room_id: room_id(n), pid, starttime }
                );
            }

            // The crash-window property the commit marker exists for: ANY
            // strict prefix of a valid token file — however the write was cut —
            // must read as claim-in-progress, never as a (mis)parsed claim.
            #[test]
            fn every_truncation_is_claim_in_progress(
                n in any::<u32>(),
                pid in any::<u32>(),
                starttime in any::<u64>(),
                cut in any::<proptest::sample::Index>(),
            ) {
                let full = format!("{}\n{pid} {starttime}\n", room_id(n));
                let prefix = &full[..cut.index(full.len())];
                prop_assert_eq!(
                    parse_token(prefix),
                    SlotToken::InProgress,
                    "truncated at {}: {:?}",
                    cut.index(full.len()),
                    prefix
                );
            }

            #[test]
            fn parse_token_is_total(contents in ".*") {
                // Never panics, whatever the file holds.
                let _ = parse_token(&contents);
            }
        }
    }
}
