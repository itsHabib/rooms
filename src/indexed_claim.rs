//! Shared crash-safe claims for bounded, indexed host resources.
//!
//! A pool is one directory of numeric claim files plus a sibling free-lock.
//! Claims race through `O_CREAT|O_EXCL`; claim publication, release, and
//! reconciliation serialize their short critical sections through the
//! free-lock. Callers own only identity derivation and resource-specific
//! cleanup policy.

#![allow(
    clippy::redundant_pub_crate,
    reason = "sibling allocators share these internals while the module stays crate-private"
)]

use std::io::Write;
use std::path::Path;

use tracing::warn;

use crate::room::Liveness;

#[derive(Debug, Clone, Copy)]
pub struct Claimer {
    pub pid: u32,
    pub starttime: u64,
}

impl Claimer {
    #[must_use]
    pub fn current() -> Option<Self> {
        let pid = std::process::id();
        let starttime = crate::room::starttime_of(pid)?;
        Some(Self { pid, starttime })
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Pool {
    pub dir_name: &'static str,
    pub lock_name: &'static str,
    pub max_index: u8,
    pub label: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClaimOutcome {
    Claimed(u8),
    PoolFull { cap: u8 },
    InvalidIndex { index: u8, max: u8 },
    TargetTaken { index: u8 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClaimDecision {
    Accept,
    #[cfg(any(target_os = "linux", test))]
    Skip,
}

#[derive(Clone, Copy)]
pub(crate) struct ClaimSpec<'a> {
    pub state: &'a Path,
    pub pool: Pool,
    pub owner_id: &'a str,
    pub me: Claimer,
    pub cap: u8,
    pub target: Option<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FreeOutcome {
    Removed,
    AlreadyFree,
    AlreadyReassigned,
}

#[derive(Debug)]
pub(crate) enum ReleaseError {
    Io(std::io::Error),
    Cleanup(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Reclaimed {
    pub index: u8,
    pub owner_id: String,
    pub removed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReconcileAction {
    Keep,
    Remove,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClaimToken {
    pub owner_id: String,
    pub pid: u32,
    pub starttime: u64,
}

struct DeadClaim<'a> {
    path: &'a Path,
    index: u8,
    expected: &'a str,
    owner_id: &'a str,
}

pub(crate) fn claim(
    state: &Path,
    pool: Pool,
    owner_id: &str,
    me: Claimer,
    cap: u8,
    target: Option<u8>,
) -> Result<ClaimOutcome, std::io::Error> {
    let spec = ClaimSpec {
        state,
        pool,
        owner_id,
        me,
        cap,
        target,
    };
    claim_with(spec, |_| Ok(ClaimDecision::Accept))
}

pub(crate) fn claim_with<F, E>(spec: ClaimSpec<'_>, mut select: F) -> Result<ClaimOutcome, E>
where
    F: FnMut(u8) -> Result<ClaimDecision, E>,
    E: From<std::io::Error>,
{
    if !is_id_shaped(spec.owner_id) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "{} owner ID must be 26 lowercase ASCII letters or digits",
                spec.pool.label
            ),
        )
        .into());
    }
    if let Some(index) = spec.target {
        if !valid_index(spec.pool, index) {
            return Ok(ClaimOutcome::InvalidIndex {
                index,
                max: spec.pool.max_index,
            });
        }
    }
    let dir = spec.state.join(spec.pool.dir_name);
    std::fs::create_dir_all(&dir)?;
    // Reconciliation may remove a malformed claim only while holding this same
    // lock. A live creator therefore finishes its token before reconcile can
    // classify an observed partial write as abandoned.
    let _lock = lock_frees(spec.state, spec.pool)?;
    if let Some(index) = spec.target {
        return claim_target(&dir, spec.owner_id, spec.me, index, &mut select);
    }
    let cap = spec.cap.min(spec.pool.max_index);
    for index in 1..=cap {
        if try_claim(&dir, index, spec.owner_id, spec.me)?
            && select_claim(&dir, index, &mut select)?
        {
            return Ok(ClaimOutcome::Claimed(index));
        }
    }
    Ok(ClaimOutcome::PoolFull { cap })
}

fn claim_target<F, E>(
    dir: &Path,
    owner_id: &str,
    me: Claimer,
    index: u8,
    select: &mut F,
) -> Result<ClaimOutcome, E>
where
    F: FnMut(u8) -> Result<ClaimDecision, E>,
    E: From<std::io::Error>,
{
    if try_claim(dir, index, owner_id, me)? && select_claim(dir, index, select)? {
        return Ok(ClaimOutcome::Claimed(index));
    }
    Ok(ClaimOutcome::TargetTaken { index })
}

fn select_claim<F, E>(dir: &Path, index: u8, select: &mut F) -> Result<bool, E>
where
    F: FnMut(u8) -> Result<ClaimDecision, E>,
    E: From<std::io::Error>,
{
    let decision = match select(index) {
        Ok(decision) => decision,
        Err(error) => {
            remove_rejected_claim(dir, index);
            return Err(error);
        }
    };
    if decision == ClaimDecision::Accept {
        return Ok(true);
    }
    std::fs::remove_file(dir.join(index.to_string()))?;
    Ok(false)
}

fn remove_rejected_claim(dir: &Path, index: u8) {
    if let Err(error) = std::fs::remove_file(dir.join(index.to_string())) {
        warn!(index, %error, "could not remove rejected indexed claim");
    }
}

fn try_claim(dir: &Path, index: u8, owner_id: &str, me: Claimer) -> Result<bool, std::io::Error> {
    let path = dir.join(index.to_string());
    let open = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path);
    let mut file = match open {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => return Ok(false),
        #[cfg(windows)]
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return Ok(false),
        Err(error) => return Err(error),
    };
    let token = format!("{owner_id}\n{} {}\n", me.pid, me.starttime);
    if let Err(error) = file.write_all(token.as_bytes()) {
        drop(file);
        if let Err(remove_error) = std::fs::remove_file(&path) {
            warn!(
                index,
                error = %remove_error,
                "could not remove half-written indexed claim"
            );
        }
        return Err(error);
    }
    Ok(true)
}

pub(crate) fn free(
    state: &Path,
    pool: Pool,
    index: u8,
    expected_owner: &str,
) -> Result<FreeOutcome, std::io::Error> {
    match free_with(state, pool, index, expected_owner, || Ok(())) {
        Ok(outcome) => Ok(outcome),
        Err(ReleaseError::Io(error)) => Err(error),
        Err(ReleaseError::Cleanup(detail)) => Err(std::io::Error::other(detail)),
    }
}

pub(crate) fn free_with<F>(
    state: &Path,
    pool: Pool,
    index: u8,
    expected_owner: &str,
    cleanup: F,
) -> Result<FreeOutcome, ReleaseError>
where
    F: FnOnce() -> Result<(), String>,
{
    let path = state.join(pool.dir_name).join(index.to_string());
    let _lock = lock_frees(state, pool).map_err(ReleaseError::Io)?;
    let contents = match std::fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(FreeOutcome::AlreadyFree);
        }
        Err(error) => return Err(ReleaseError::Io(error)),
    };
    if contents.lines().next() != Some(expected_owner) {
        return Ok(FreeOutcome::AlreadyReassigned);
    }
    cleanup().map_err(ReleaseError::Cleanup)?;
    match std::fs::remove_file(path) {
        Ok(()) => Ok(FreeOutcome::Removed),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(FreeOutcome::AlreadyFree),
        Err(error) => Err(ReleaseError::Io(error)),
    }
}

pub(crate) const fn valid_index(pool: Pool, index: u8) -> bool {
    index > 0 && index <= pool.max_index
}

pub(crate) fn lock_frees(state: &Path, pool: Pool) -> Result<std::fs::File, std::io::Error> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(state.join(pool.lock_name))?;
    file.lock()?;
    Ok(file)
}

pub(crate) fn rewrite_atomic(dir: &Path, index: u8, body: &str) -> Result<(), std::io::Error> {
    let tmp = dir.join(format!(".{index}.tmp"));
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&tmp)?;
    file.write_all(body.as_bytes())?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp, dir.join(index.to_string()))?;
    sync_dir(dir)
}

#[cfg(unix)]
pub(crate) fn sync_dir(path: &Path) -> Result<(), std::io::Error> {
    std::fs::File::open(path)?.sync_all()
}

#[cfg(not(unix))]
#[allow(
    clippy::unnecessary_wraps,
    reason = "keeps durable indexed-claim transitions identical across cfg targets"
)]
pub(crate) const fn sync_dir(_path: &Path) -> Result<(), std::io::Error> {
    Ok(())
}

pub(crate) fn reconcile<F>(state: &Path, pool: Pool, mut cleanup: F) -> Vec<Reclaimed>
where
    F: FnMut(u8, &str) -> Result<ReconcileAction, String>,
{
    let dir = state.join(pool.dir_name);
    let read_dir = match std::fs::read_dir(&dir) {
        Ok(read_dir) => read_dir,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => {
            warn!(
                dir = %dir.display(),
                error = %error,
                resource = pool.label,
                "cannot scan indexed-claim directory; skipping reconcile"
            );
            return Vec::new();
        }
    };
    let mut reclaimed = Vec::new();
    for entry in read_dir {
        let Some(item) = reconcile_entry(state, pool, entry, &mut cleanup) else {
            continue;
        };
        reclaimed.push(item);
    }
    reclaimed
}

fn reconcile_entry<F>(
    state: &Path,
    pool: Pool,
    entry: Result<std::fs::DirEntry, std::io::Error>,
    cleanup: &mut F,
) -> Option<Reclaimed>
where
    F: FnMut(u8, &str) -> Result<ReconcileAction, String>,
{
    let entry = match entry {
        Ok(entry) => entry,
        Err(error) => {
            warn!(error = %error, resource = pool.label, "unreadable claim entry; skipping");
            return None;
        }
    };
    let index = index_of(pool, &entry.file_name())?;
    reconcile_path(state, pool, &entry.path(), index, cleanup)
}

fn reconcile_path<F>(
    state: &Path,
    pool: Pool,
    path: &Path,
    index: u8,
    cleanup: &mut F,
) -> Option<Reclaimed>
where
    F: FnMut(u8, &str) -> Result<ReconcileAction, String>,
{
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) => {
            warn!(index, error = %error, resource = pool.label, "cannot read claim; skipping");
            return None;
        }
    };
    let Some(claim) = parse_claim(&contents) else {
        // Slot reservations and leases deliberately use a separate durable
        // grammar. They are opaque to the shared claimer reconciler.
        if contents.starts_with('@') {
            return None;
        }
        discard_abandoned_partial(state, pool, path, index, &contents);
        return None;
    };
    if claimer_liveness(claim.pid, claim.starttime) != Liveness::Dead {
        return None;
    }
    let dead = DeadClaim {
        path,
        index,
        expected: &contents,
        owner_id: &claim.owner_id,
    };
    reconcile_dead(state, pool, &dead, cleanup)
}

fn discard_abandoned_partial(state: &Path, pool: Pool, path: &Path, index: u8, observed: &str) {
    let result = (|| -> Result<bool, std::io::Error> {
        let _lock = lock_frees(state, pool)?;
        let now = match std::fs::read_to_string(path) {
            Ok(now) => now,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        if now != observed || parse_claim(&now).is_some() {
            return Ok(false);
        }
        std::fs::remove_file(path)?;
        Ok(true)
    })();
    match result {
        Ok(true) => warn!(
            index,
            resource = pool.label,
            "removed abandoned partial claim"
        ),
        Ok(false) => {}
        Err(error) => warn!(
            index,
            error = %error,
            resource = pool.label,
            "cannot inspect partial claim; leaving for the next pass"
        ),
    }
}

fn reconcile_dead<F>(
    state: &Path,
    pool: Pool,
    dead: &DeadClaim<'_>,
    cleanup: &mut F,
) -> Option<Reclaimed>
where
    F: FnMut(u8, &str) -> Result<ReconcileAction, String>,
{
    let result = (|| -> Result<Option<Reclaimed>, String> {
        let _lock = lock_frees(state, pool).map_err(|error| error.to_string())?;
        let now = match std::fs::read_to_string(dead.path) {
            Ok(now) => now,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.to_string()),
        };
        if now != dead.expected {
            return Ok(None);
        }
        let action = cleanup(dead.index, dead.owner_id)?;
        if action == ReconcileAction::Keep {
            return Ok(Some(Reclaimed {
                index: dead.index,
                owner_id: dead.owner_id.to_owned(),
                removed: false,
            }));
        }
        std::fs::remove_file(dead.path).map_err(|error| error.to_string())?;
        Ok(Some(Reclaimed {
            index: dead.index,
            owner_id: dead.owner_id.to_owned(),
            removed: true,
        }))
    })();
    match result {
        Ok(reclaimed) => reclaimed,
        Err(error) => {
            warn!(
                index = dead.index,
                error,
                resource = pool.label,
                "cannot reconcile dead claim; leaving for the next pass"
            );
            None
        }
    }
}

fn index_of(pool: Pool, name: &std::ffi::OsStr) -> Option<u8> {
    let name = name.to_str()?;
    let index: u8 = name.parse().ok()?;
    if name != index.to_string() || !valid_index(pool, index) {
        return None;
    }
    Some(index)
}

pub(crate) fn parse_claim(contents: &str) -> Option<ClaimToken> {
    if !contents.ends_with('\n') {
        return None;
    }
    let mut lines = contents.lines();
    let owner_id = lines.next()?;
    if !is_id_shaped(owner_id) {
        return None;
    }
    let mut parts = lines.next()?.split_whitespace();
    let pid = parts.next()?.parse().ok()?;
    let starttime = parts.next()?.parse().ok()?;
    if parts.next().is_some() || lines.next().is_some() {
        return None;
    }
    Some(ClaimToken {
        owner_id: owner_id.to_owned(),
        pid,
        starttime,
    })
}

fn is_id_shaped(value: &str) -> bool {
    value.len() == 26
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte.is_ascii_lowercase())
}

#[must_use]
pub(crate) fn classify_claimer_stat(stat: &str, expected_starttime: u64) -> Liveness {
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

#[cfg(target_os = "linux")]
fn claimer_liveness(pid: u32, starttime: u64) -> Liveness {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => classify_claimer_stat(&stat, starttime),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Liveness::Dead,
        Err(_) => Liveness::Unknown,
    }
}

#[cfg(not(target_os = "linux"))]
#[allow(
    clippy::missing_const_for_fn,
    reason = "kept non-const to match the Linux liveness probe that reads /proc"
)]
fn claimer_liveness(_pid: u32, _starttime: u64) -> Liveness {
    Liveness::Unknown
}

#[cfg(test)]
mod tests {
    use super::{claim, parse_claim, Claimer, Pool};
    #[cfg(target_os = "linux")]
    use super::{reconcile, ReconcileAction};

    const TEST_POOL: Pool = Pool {
        dir_name: "claims",
        lock_name: "claims.lock",
        max_index: 3,
        label: "test resource",
    };

    #[test]
    fn claim_parser_requires_the_exact_two_line_grammar() {
        let owner = "00000000000000000000000001";
        assert!(parse_claim(&format!("{owner}\n42 7\n")).is_some());
        for malformed in [
            format!("{owner}\n42 7 extra\n"),
            format!("{owner}\n42 7\nextra\n"),
            format!("{owner}\n42 7"),
        ] {
            assert!(parse_claim(&malformed).is_none());
        }
    }

    #[test]
    fn invalid_owner_is_rejected_before_claim_state_exists(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let state = tempfile::tempdir()?;
        let result = claim(
            state.path(),
            TEST_POOL,
            "not-a-valid-owner",
            Claimer {
                pid: 42,
                starttime: 7,
            },
            3,
            None,
        );
        let error = match result {
            Err(error) => error,
            Ok(outcome) => return Err(format!("unexpected claim outcome: {outcome:?}").into()),
        };
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert!(!state.path().join(TEST_POOL.dir_name).exists());
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn reconcile_removes_an_abandoned_partial_claim() -> Result<(), Box<dyn std::error::Error>> {
        let state = tempfile::tempdir()?;
        let claims = state.path().join(TEST_POOL.dir_name);
        std::fs::create_dir_all(&claims)?;
        std::fs::write(claims.join("1"), "00000000000000000000000001\n")?;

        let reclaimed = reconcile(state.path(), TEST_POOL, |_, _| Ok(ReconcileAction::Remove));
        assert!(reclaimed.is_empty());
        assert!(!claims.join("1").exists());
        Ok(())
    }
}
