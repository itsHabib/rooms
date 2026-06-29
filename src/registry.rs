//! The room registry — `rooms ls` + `rooms gc` over the on-disk state base.
//!
//! Policy plane: scan the state base, classify each room's liveness, and reap
//! only the corpses. The mechanism it composes — the metadata/liveness in
//! [`crate::room`], the path layout in [`crate::config`], and the teardown in
//! [`crate::firecracker`] — stays dumb; the *decisions* (what state a room is
//! in, what is safe to reap) live here.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::Serialize;
use tracing::warn;

use crate::config::RoomsConfig;
use crate::error::RegistryError;
use crate::firecracker;
use crate::room::{self, Liveness, RoomMeta};

/// Schema version for `ls --json` stdout (mirrors `doctor`/`diff`).
pub const REGISTRY_SCHEMA_VERSION: u32 = 1;

/// A room's lifecycle state as the registry sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RoomState {
    /// firecracker alive, an in-flight `rooms run`.
    Running,
    /// firecracker alive, a deliberately-held `--keep` room.
    Kept,
    /// firecracker dead; the state dir + bind-mounts leaked. The *only* reapable
    /// state.
    OrphanedDead,
    /// Liveness indeterminate (no pid recorded / unreadable `/proc`). Never
    /// reaped — indeterminate ≠ dead.
    Unknown,
}

impl RoomState {
    /// Stable lowercase label for human output (matches the JSON rename).
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Kept => "kept",
            Self::OrphanedDead => "orphaned-dead",
            Self::Unknown => "unknown",
        }
    }

    /// Whether `rooms gc` may reap a room in this state. The whole safety
    /// invariant funnels through this one predicate: only a confirmed-dead
    /// orphan is reapable.
    #[must_use]
    pub const fn is_reapable(self) -> bool {
        matches!(self, Self::OrphanedDead)
    }
}

/// One room as listed by `rooms ls`.
#[derive(Debug, Clone, Serialize)]
pub struct RoomEntry {
    pub id: String,
    pub state: RoomState,
    pub label: Option<String>,
    pub pid: Option<u32>,
    pub started_at: Option<DateTime<Utc>>,
    pub keep: bool,
}

/// The `ls --json` payload (schema'd, like the doctor/diff reports).
#[derive(Debug, Clone, Serialize)]
pub struct ListReport {
    pub schema_version: u32,
    pub rooms: Vec<RoomEntry>,
}

impl ListReport {
    #[must_use]
    pub const fn new(rooms: Vec<RoomEntry>) -> Self {
        Self {
            schema_version: REGISTRY_SCHEMA_VERSION,
            rooms,
        }
    }
}

/// Pure state classification from a room's keep flag + liveness (policy).
#[must_use]
pub const fn classify(keep: bool, liveness: Liveness) -> RoomState {
    match liveness {
        Liveness::Alive if keep => RoomState::Kept,
        Liveness::Alive => RoomState::Running,
        Liveness::Dead => RoomState::OrphanedDead,
        Liveness::Unknown => RoomState::Unknown,
    }
}

/// Whether `id` is a syntactically valid room id (26 lowercase-alphanumerics).
///
/// The lowercased ULID alphabet is a subset, so every real id passes. This is
/// the gate that keeps `..`, `jailer`, absolute paths, and separators out of
/// every path gc builds — the `<id>` arg and every scanned dir name pass
/// through it.
#[must_use]
pub fn is_valid_room_id(id: &str) -> bool {
    id.len() == 26
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || b.is_ascii_lowercase())
}

/// List every room under the state base, classified.
///
/// Tolerant: a room with no (or unreadable) `room.json` still lists, with
/// `Unknown` liveness. Sorted by start time (oldest first), then id for the
/// meta-less rows.
pub fn list_rooms(config: &RoomsConfig) -> Result<Vec<RoomEntry>, RegistryError> {
    let base = config
        .resolved_state_base()
        .ok_or(RegistryError::HomeUnset)?;
    let read_dir = match std::fs::read_dir(&base) {
        Ok(rd) => rd,
        // No state base yet ⇒ no rooms, not an error.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(RegistryError::Io(e)),
    };
    let mut entries = Vec::new();
    for dirent in read_dir {
        let dirent = dirent.map_err(RegistryError::Io)?;
        let name = dirent.file_name().to_string_lossy().into_owned();
        // Skips `jailer/`, stray files, and anything that isn't a room dir.
        if !is_valid_room_id(&name) || !dirent.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        entries.push(entry_for(config, &name));
    }
    entries.sort_by(|a, b| {
        a.started_at
            .cmp(&b.started_at)
            .then_with(|| a.id.cmp(&b.id))
    });
    Ok(entries)
}

/// Build one room's entry: load its (soft) metadata, probe liveness, classify.
fn entry_for(config: &RoomsConfig, id: &str) -> RoomEntry {
    let meta = config.room_dir(id).and_then(|dir| load_meta_soft(&dir));
    let pid = meta.as_ref().and_then(|m| m.pid);
    let keep = meta.as_ref().is_some_and(|m| m.keep);
    RoomEntry {
        id: id.to_owned(),
        state: classify(keep, room::probe(pid)),
        label: meta.as_ref().and_then(|m| m.label.clone()),
        pid,
        started_at: meta.as_ref().map(|m| m.started_at),
        keep,
    }
}

/// Read a room's metadata, downgrading any read/parse error to `None` (one bad
/// file must not break `ls` for healthy rooms) with a warning.
fn load_meta_soft(room_dir: &Path) -> Option<RoomMeta> {
    match room::read(room_dir) {
        Ok(meta) => meta,
        Err(e) => {
            warn!(dir = %room_dir.display(), error = %e, "unreadable room.json; treating as no metadata");
            None
        }
    }
}

/// Inputs to a gc run.
#[derive(Debug, Clone, Default)]
pub struct GcOptions {
    /// Preview only — touch nothing.
    pub dry_run: bool,
    /// Reap only this room id (still only if it's orphaned-dead).
    pub only: Option<String>,
}

/// What gc did (or would do) to one room.
#[derive(Debug, Clone, Serialize)]
pub struct GcOutcome {
    pub id: String,
    pub state: RoomState,
    /// True only when the room was actually removed (never in a dry-run).
    pub reaped: bool,
    pub reason: String,
}

/// The result of a gc run.
#[derive(Debug, Clone, Serialize)]
pub struct GcReport {
    pub schema_version: u32,
    pub dry_run: bool,
    pub outcomes: Vec<GcOutcome>,
}

/// Reap orphaned-dead rooms. Reaps **only** `OrphanedDead` rooms — Running,
/// Kept, and Unknown are reported as skipped, never touched. `--dry-run`
/// previews without removing anything.
pub fn gc(config: &RoomsConfig, opts: &GcOptions) -> Result<GcReport, RegistryError> {
    if let Some(id) = &opts.only {
        if !is_valid_room_id(id) {
            return Err(RegistryError::InvalidRoomId { id: id.clone() });
        }
    }
    let mut rooms = list_rooms(config)?;
    if let Some(only) = &opts.only {
        rooms.retain(|e| &e.id == only);
    }
    let mut outcomes = Vec::with_capacity(rooms.len());
    for entry in &rooms {
        // Accumulate per-room errors instead of aborting the batch: one orphan
        // with a stuck mount must not stop gc from reaping the healthy rest.
        match reap_entry(config, entry, opts.dry_run) {
            Ok(out) => outcomes.push(out),
            Err(e) => {
                warn!(id = %entry.id, error = %e, "reap failed; continuing with the remaining rooms");
                outcomes.push(error_outcome(entry, &e));
            }
        }
    }
    Ok(GcReport {
        schema_version: REGISTRY_SCHEMA_VERSION,
        dry_run: opts.dry_run,
        outcomes,
    })
}

/// Decide and (unless dry-run) perform the reap for one room. The cardinal
/// predicate lives in the first guard: a non-reapable room returns a skip
/// outcome before any path is resolved or touched.
fn reap_entry(
    config: &RoomsConfig,
    entry: &RoomEntry,
    dry_run: bool,
) -> Result<GcOutcome, RegistryError> {
    if !entry.state.is_reapable() {
        return Ok(skip_outcome(entry));
    }
    // Resolve + safety-check the dirs even for a dry-run, so a preview surfaces
    // a path-escape rather than hiding it until the real run.
    let (room_dir, jail_instance_dir, socket) = reap_paths(config, &entry.id)?;
    if dry_run {
        return Ok(outcome(entry, false, "would reap (dry-run)"));
    }
    firecracker::reap_orphan(&room_dir, &jail_instance_dir, &socket, config)?;
    Ok(outcome(entry, true, "reaped"))
}

/// Resolve the two dirs gc removes for `id`, re-checking each sits under its
/// expected parent — the defense-in-depth backstop behind `is_valid_room_id`.
fn reap_paths(
    config: &RoomsConfig,
    id: &str,
) -> Result<(PathBuf, PathBuf, PathBuf), RegistryError> {
    let base = config
        .resolved_state_base()
        .ok_or(RegistryError::HomeUnset)?;
    let chroot_base = config.chroot_base().ok_or(RegistryError::HomeUnset)?;
    let room_dir = config.room_dir(id).ok_or(RegistryError::HomeUnset)?;
    let jail_instance_dir = config
        .jail_instance_dir(id)
        .ok_or(RegistryError::HomeUnset)?;
    let socket = config.jail_socket(id).ok_or(RegistryError::HomeUnset)?;
    ensure_child(&room_dir, &base)?;
    ensure_child(&jail_instance_dir, &chroot_base.join("firecracker"))?;
    Ok((room_dir, jail_instance_dir, socket))
}

/// Reject any reap target that isn't a direct child of its expected parent.
fn ensure_child(path: &Path, expected_parent: &Path) -> Result<(), RegistryError> {
    if path.parent() == Some(expected_parent) {
        return Ok(());
    }
    Err(RegistryError::PathEscape {
        path: path.to_path_buf(),
    })
}

fn skip_outcome(entry: &RoomEntry) -> GcOutcome {
    outcome(entry, false, &format!("skipped: {}", entry.state.label()))
}

fn error_outcome(entry: &RoomEntry, err: &RegistryError) -> GcOutcome {
    outcome(entry, false, &format!("error: {err}"))
}

fn outcome(entry: &RoomEntry, reaped: bool, reason: &str) -> GcOutcome {
    GcOutcome {
        id: entry.id.clone(),
        state: entry.state,
        reaped,
        reason: reason.to_owned(),
    }
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

    use super::{classify, gc, is_valid_room_id, list_rooms, GcOptions, RoomState};
    use crate::config::RoomsConfig;
    use crate::room::{self, Liveness, RoomMeta};
    use chrono::Utc;
    use std::path::{Path, PathBuf};

    const VALID_ID: &str = "01abcdefghijklmnopqrstuvwx";

    fn config_with_base(base: &Path) -> RoomsConfig {
        RoomsConfig {
            state_base: Some(base.to_path_buf()),
            ..RoomsConfig::default()
        }
    }

    /// Materialize a room on disk: its per-room dir + room.json, and (optionally)
    /// its jail instance tree with the two bind-mount *targets* (plain files in
    /// the test — no real mounts). Returns the room id.
    fn make_room(config: &RoomsConfig, pid: Option<u32>, keep: bool, with_jail: bool) -> String {
        let id = unique_id();
        let room_dir = config.room_dir(&id).unwrap();
        std::fs::create_dir_all(&room_dir).unwrap();
        let meta = RoomMeta::new(
            id.clone(),
            Some("sleep 600".to_owned()),
            pid,
            keep,
            Utc::now(),
        );
        room::write_atomic(&room_dir, &meta).unwrap();
        if with_jail {
            let jail_root = config.jail_root_dir(&id).unwrap();
            std::fs::create_dir_all(&jail_root).unwrap();
            std::fs::write(jail_root.join("kernel"), b"k").unwrap();
            std::fs::write(jail_root.join("rootfs"), b"r").unwrap();
            std::fs::write(jail_root.join("api.sock"), b"").unwrap();
        }
        id
    }

    // A distinct 26-char id per call without Math.random / time — derive from a
    // process-local counter so parallel tests don't collide on a fixed id.
    fn unique_id() -> String {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let suffix = format!("{n:06}");
        format!("01abcdefghijklmnopqr{suffix}")
    }

    #[test]
    fn classify_truth_table() {
        assert_eq!(classify(false, Liveness::Alive), RoomState::Running);
        assert_eq!(classify(true, Liveness::Alive), RoomState::Kept);
        assert_eq!(classify(false, Liveness::Dead), RoomState::OrphanedDead);
        assert_eq!(classify(true, Liveness::Dead), RoomState::OrphanedDead);
        assert_eq!(classify(false, Liveness::Unknown), RoomState::Unknown);
        assert_eq!(classify(true, Liveness::Unknown), RoomState::Unknown);
    }

    #[test]
    fn only_orphaned_dead_is_reapable() {
        assert!(RoomState::OrphanedDead.is_reapable());
        assert!(!RoomState::Running.is_reapable());
        assert!(!RoomState::Kept.is_reapable());
        assert!(!RoomState::Unknown.is_reapable());
    }

    #[test]
    fn id_validator_rejects_traversal_and_junk() {
        assert!(is_valid_room_id(VALID_ID));
        for bad in [
            "..",
            "jailer",
            "../../etc",
            "01abc",                       // too short
            "01ABCDEFGHIJKLMNOPQRSTUVWX",  // uppercase
            "01abcdefghijklmnopqrstuv/x",  // separator
            "01abcdefghijklmnopqrstuv.x",  // dot
            "01abcdefghijklmnopqrstuvwxy", // 27 chars
        ] {
            assert!(!is_valid_room_id(bad), "{bad} must be rejected");
        }
    }

    #[test]
    fn list_skips_jailer_dir_and_non_rooms() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with_base(dir.path());
        // a real room + the jailer dir + a stray file with a room-shaped name.
        let id = make_room(&config, Some(std::process::id()), false, true);
        std::fs::create_dir_all(dir.path().join("jailer/firecracker")).unwrap();
        std::fs::write(dir.path().join(VALID_ID), b"not a dir").unwrap();
        let rooms = list_rooms(&config).unwrap();
        assert_eq!(rooms.len(), 1, "only the real room should list");
        assert_eq!(rooms[0].id, id);
    }

    #[test]
    fn list_on_missing_base_is_empty_not_error() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with_base(&dir.path().join("does-not-exist"));
        assert!(list_rooms(&config).unwrap().is_empty());
    }

    #[test]
    fn gc_never_reaps_live_kept_or_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with_base(dir.path());
        // running (this process's pid is alive but isn't firecracker → wait:
        // probe keys on comm, so a real pid reads Dead). To simulate "alive" we
        // can't fake /proc, so cover the never-reap invariant via the states
        // that DON'T require a live process: Unknown (no pid) + a meta-less room.
        let unknown = make_room(&config, None, false, true); // pid None → Unknown
        let report = gc(&config, &GcOptions::default()).unwrap();
        assert!(
            report.outcomes.iter().all(|o| !o.reaped),
            "no non-orphaned room may be reaped"
        );
        // the dirs must survive.
        assert!(config.room_dir(&unknown).unwrap().exists());
        assert!(config.jail_instance_dir(&unknown).unwrap().exists());
    }

    #[test]
    fn gc_reaps_orphaned_dead_room() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with_base(dir.path());
        // A pid that cannot exist → Dead → OrphanedDead → reapable.
        let dead_pid = Some(4_194_305u32);
        let id = make_room(&config, dead_pid, false, true);
        // sanity: the dirs exist before.
        assert!(config.room_dir(&id).unwrap().exists());
        assert!(config.jail_instance_dir(&id).unwrap().exists());

        let report = gc(&config, &GcOptions::default()).unwrap();
        // On Linux the dead pid classifies OrphanedDead and is reaped; elsewhere
        // probe is Unknown and it's skipped. Assert the platform-correct branch.
        if cfg!(target_os = "linux") {
            assert!(report.outcomes.iter().any(|o| o.id == id && o.reaped));
            assert!(!config.room_dir(&id).unwrap().exists(), "room dir reaped");
            assert!(
                !config.jail_instance_dir(&id).unwrap().exists(),
                "jail dir reaped"
            );
        } else {
            assert!(report.outcomes.iter().all(|o| !o.reaped));
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn dry_run_deletes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with_base(dir.path());
        let id = make_room(&config, Some(4_194_305), false, true);
        let report = gc(
            &config,
            &GcOptions {
                dry_run: true,
                only: None,
            },
        )
        .unwrap();
        assert!(report.dry_run);
        assert!(report.outcomes.iter().any(|o| o.id == id && !o.reaped));
        assert!(
            config.room_dir(&id).unwrap().exists(),
            "dry-run must not delete"
        );
        assert!(config.jail_instance_dir(&id).unwrap().exists());
    }

    #[test]
    fn gc_rejects_invalid_only_id() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with_base(dir.path());
        let err = gc(
            &config,
            &GcOptions {
                dry_run: false,
                only: Some("../etc".to_owned()),
            },
        );
        assert!(
            err.is_err(),
            "an invalid --id must be rejected before any fs work"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn gc_only_targets_one_room() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with_base(dir.path());
        let bystander = make_room(&config, Some(4_194_305), false, true);
        let target = make_room(&config, Some(4_194_305), false, true);
        let report = gc(
            &config,
            &GcOptions {
                dry_run: false,
                only: Some(target.clone()),
            },
        )
        .unwrap();
        assert_eq!(report.outcomes.len(), 1);
        assert_eq!(report.outcomes[0].id, target);
        // the non-targeted orphan is untouched.
        assert!(config.room_dir(&bystander).unwrap().exists());
        assert!(!config.room_dir(&target).unwrap().exists());
    }

    /// gc must not abort the whole batch when one orphan's reap fails (a stuck
    /// mount): the healthy orphans still get reaped and the failure surfaces as a
    /// per-room outcome, not an error that aborts the run.
    #[cfg(target_os = "linux")]
    #[test]
    fn gc_continues_past_a_failed_reap() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let config = config_with_base(dir.path());

        // Orphan whose jail removal fails: lock its jail root read-only so the
        // entries under it can't be unlinked -> remove_dir_all fails -> reap errors.
        let stuck = make_room(&config, Some(4_194_305), false, true);
        let stuck_root = config.jail_root_dir(&stuck).unwrap();
        std::fs::create_dir_all(stuck_root.join("sub")).unwrap();
        std::fs::set_permissions(&stuck_root, std::fs::Permissions::from_mode(0o500)).unwrap();

        // A normal orphan that reaps cleanly.
        let healthy = make_room(&config, Some(4_194_305), false, true);

        let report = gc(&config, &GcOptions::default()).unwrap(); // must NOT be Err

        std::fs::set_permissions(&stuck_root, std::fs::Permissions::from_mode(0o700)).unwrap();

        // The healthy orphan is reaped regardless of the stuck one -> no abort.
        let healthy_out = report
            .outcomes
            .iter()
            .find(|o| o.id == healthy)
            .expect("healthy listed");
        assert!(
            healthy_out.reaped,
            "healthy orphan reaped despite the stuck one"
        );
        assert!(!config.room_dir(&healthy).unwrap().exists());

        // Stuck-specific assertions only when the failure was actually injected
        // (a root test runner bypasses the read-only lock).
        if config.room_dir(&stuck).unwrap().exists() {
            let stuck_out = report
                .outcomes
                .iter()
                .find(|o| o.id == stuck)
                .expect("stuck listed");
            assert!(!stuck_out.reaped);
            assert!(
                stuck_out.reason.contains("error"),
                "stuck reap reported as error: {}",
                stuck_out.reason
            );
        }
    }

    #[test]
    fn meta_less_room_is_unknown_not_reaped() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with_base(dir.path());
        let id = unique_id();
        // a room dir with NO room.json (crash before the write).
        std::fs::create_dir_all(config.room_dir(&id).unwrap()).unwrap();
        let rooms = list_rooms(&config).unwrap();
        let entry = rooms.iter().find(|e| e.id == id).expect("listed");
        assert_eq!(entry.state, RoomState::Unknown);
        let report = gc(&config, &GcOptions::default()).unwrap();
        assert!(report.outcomes.iter().all(|o| !o.reaped));
        assert!(config.room_dir(&id).unwrap().exists());
    }

    // Keep PathBuf import used on all platforms.
    #[test]
    fn valid_id_round_trips_to_room_dir() {
        let config = config_with_base(&PathBuf::from("/s"));
        assert_eq!(
            config.room_dir(VALID_ID),
            Some(PathBuf::from(format!("/s/{VALID_ID}")))
        );
    }
}
