//! Recoverable execution of the snapshot policy.
//!
//! [`crate::snapshot`] decides what a Full snapshot means. This module owns the
//! host effects and their durable transaction boundaries.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::RoomsConfig;
use crate::error::FirecrackerError;
use crate::firecracker::{self, KillSignalOutcome};
use crate::room::{self, Liveness, RoomMeta};
use crate::slot::{self, Freed, Reserved};
use crate::snapshot::{self, FcOp, SnapshotMeta, SnapshotRequest};
use crate::transport;

pub const SNAPSHOT_INTENTS_DIR: &str = "snapshot-intents";
pub const RESTORE_INTENTS_DIR: &str = "restore-intents";
pub const SNAPSHOTS_DIR: &str = "snapshots";
const INTENT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
enum Boundary {
    Pending,
    TargetsStaged,
    Paused,
    Collected,
    Reserved,
    Reaped,
    Published,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SnapshotIntent {
    schema_version: u32,
    snapshot_id: String,
    base_room_id: String,
    meta: SnapshotMeta,
    out_dir: PathBuf,
    host_vmstate: PathBuf,
    host_mem: PathBuf,
    jail_vmstate: PathBuf,
    jail_mem: PathBuf,
    boundary: Boundary,
}

/// Completed snapshot command output.
#[derive(Debug, Clone, Serialize)]
pub struct SnapshotResult {
    pub snapshot_id: String,
    pub directory: PathBuf,
    pub base_room: String,
    pub slot: u8,
    pub guest_ip: std::net::Ipv4Addr,
    pub provenance: room::Provenance,
}

/// One indexed transaction and the next safe operator action.
#[derive(Debug, Clone, Serialize)]
pub struct PendingSnapshot {
    pub snapshot_id: String,
    pub base_room: String,
    pub boundary: String,
    pub next_action: String,
}

/// `snapshot-recover` output: either an indexed roster or one completion.
#[derive(Debug, Clone, Serialize)]
pub struct RecoveryReport {
    pub pending: Vec<PendingSnapshot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed: Option<SnapshotResult>,
}

/// Create and drive a snapshot transaction.
pub async fn create(
    config: &RoomsConfig,
    base_id: &str,
    explicit_out: Option<&Path>,
) -> anyhow::Result<SnapshotResult> {
    if !crate::registry::is_valid_room_id(base_id) {
        anyhow::bail!("invalid room id: {base_id}");
    }
    let state = state_base(config)?;
    let _lock = acquire_lock(config, base_id)?;
    if let Some(intent) = read_intents(config)?
        .into_iter()
        .find(|intent| intent.base_room_id == base_id)
    {
        validate_resume_output(&intent, explicit_out)?;
        return drive(config, intent).await;
    }
    let room_dir = config
        .room_dir(base_id)
        .ok_or_else(|| anyhow::anyhow!("cannot resolve room directory"))?;
    let base =
        room::read(&room_dir)?.ok_or_else(|| anyhow::anyhow!("no live room with id {base_id}"))?;
    if !base.is_snapshottable() {
        anyhow::bail!(
            "room {base_id} is not a sealed neutral base (provenance: {:?})",
            base.provenance()
        );
    }
    let snapshot_id = ulid::Ulid::new().to_string().to_lowercase();
    let out_dir = resolve_output(config, &state, base_id, &snapshot_id, explicit_out)?;
    prepare_output(&out_dir)?;
    let intent = build_intent(config, &base, snapshot_id, out_dir)?;
    ensure_targets_absent(&intent)?;
    create_intent_exclusive(config, &intent)?;
    drive(config, intent).await
}

/// Resume one indexed transaction, or list all indexed transactions.
pub async fn recover(
    config: &RoomsConfig,
    snapshot_id: Option<&str>,
) -> anyhow::Result<RecoveryReport> {
    let intents = read_intents(config)?;
    let Some(snapshot_id) = snapshot_id else {
        return Ok(RecoveryReport {
            pending: intents.iter().map(pending_summary).collect(),
            completed: None,
        });
    };
    let intent = intents
        .into_iter()
        .find(|intent| intent.snapshot_id == snapshot_id)
        .ok_or_else(|| anyhow::anyhow!("no pending snapshot transaction {snapshot_id}"))?;
    let _lock = acquire_lock(config, &intent.base_room_id)?;
    if intent.boundary < Boundary::Collected {
        let ambiguous_create =
            intent.boundary == Boundary::Paused && any_artifact_nonempty(&intent);
        if ambiguous_create {
            terminate_partial_base(config, &intent)?;
        }
        if ambiguous_create || base_is_conclusively_dead(config, &intent)? {
            abort_dead_partial(config, &intent)?;
            anyhow::bail!(
                "aborted unrecoverable partial snapshot {snapshot_id}; no complete artifact pair was durably recorded"
            );
        }
    }
    let completed = drive(config, intent).await?;
    Ok(RecoveryReport {
        pending: Vec::new(),
        completed: Some(completed),
    })
}

/// Indexed snapshot transaction for a base, used by kill/gc as a fence.
pub fn pending_for_base(
    config: &RoomsConfig,
    base_id: &str,
) -> anyhow::Result<Option<PendingSnapshot>> {
    Ok(read_intents(config)?
        .into_iter()
        .find(|intent| intent.base_room_id == base_id)
        .as_ref()
        .map(pending_summary))
}

/// Whether any pending intent exists. GC conservatively skips the pool-wide
/// leaked-claim sweep while true so a dead, already-unlinked base cannot lose
/// its pre-reservation claim behind recovery.
pub fn has_pending(config: &RoomsConfig) -> anyhow::Result<bool> {
    Ok(!read_intents(config)?.is_empty())
}

/// All indexed transactions as safe summaries for GC observability.
pub fn pending_all(config: &RoomsConfig) -> anyhow::Result<Vec<PendingSnapshot>> {
    Ok(read_intents(config)?.iter().map(pending_summary).collect())
}

fn build_intent(
    config: &RoomsConfig,
    base: &RoomMeta,
    snapshot_id: String,
    out_dir: PathBuf,
) -> anyhow::Result<SnapshotIntent> {
    let jail_root = config
        .jail_root_dir(&base.id)
        .ok_or_else(|| anyhow::anyhow!("cannot resolve jail root"))?;
    let active_vsock = active_vsock(&jail_root)?;
    let request = SnapshotRequest {
        out_dir: out_dir.clone(),
        snapshot_id: snapshot_id.clone(),
        fc_version: firecracker_version(config)?,
        rootfs_hash: sha256_file(&jail_root.join("rootfs"))?,
        base_repo_sha: None,
        active_vsock,
    };
    let plan = snapshot::plan(base, request, Utc::now())?;
    Ok(SnapshotIntent {
        schema_version: INTENT_SCHEMA_VERSION,
        snapshot_id,
        base_room_id: base.id.clone(),
        meta: plan.meta,
        out_dir,
        host_vmstate: plan.host_vmstate,
        host_mem: plan.host_mem,
        jail_vmstate: jail_root.join(snapshot::SNAPSHOT_VMSTATE_FILE),
        jail_mem: jail_root.join(snapshot::SNAPSHOT_MEM_FILE),
        boundary: Boundary::Pending,
    })
}

async fn drive(config: &RoomsConfig, mut intent: SnapshotIntent) -> anyhow::Result<SnapshotResult> {
    if intent.boundary == Boundary::Published {
        finish_index(config, &intent)?;
        return result_from(&intent);
    }
    if intent.boundary < Boundary::TargetsStaged {
        stage_targets(&intent)?;
        advance(config, &mut intent, Boundary::TargetsStaged)?;
    }
    if intent.boundary < Boundary::Collected {
        execute_and_collect(config, &mut intent).await?;
    }
    if intent.boundary < Boundary::Reserved {
        reserve_slot(config, &intent)?;
        advance(config, &mut intent, Boundary::Reserved)?;
    }
    if intent.boundary < Boundary::Reaped {
        reap_base(config, &intent)?;
        advance(config, &mut intent, Boundary::Reaped)?;
    }
    validate_collected(&intent)?;
    snapshot::write_meta_atomic(
        &intent.out_dir.join(snapshot::SNAPSHOT_META_FILE),
        &intent.meta,
    )?;
    advance(config, &mut intent, Boundary::Published)?;
    finish_index(config, &intent)?;
    result_from(&intent)
}

async fn execute_and_collect(
    config: &RoomsConfig,
    intent: &mut SnapshotIntent,
) -> anyhow::Result<()> {
    validate_staged_targets(intent)?;
    let socket = config
        .jail_socket(&intent.base_room_id)
        .ok_or_else(|| anyhow::anyhow!("cannot resolve Firecracker socket"))?;
    let [pause, create] = snapshot_ops();
    if intent.boundary < Boundary::Paused {
        let paused = execute_op(&socket, &pause, config).await;
        if let Err(error) = paused {
            let _ = resume_vm(&socket, config).await;
            return Err(error);
        }
        advance(config, intent, Boundary::Paused)?;
    }
    execute_op(&socket, &create, config).await?;
    collect_pair(intent)?;
    advance(config, intent, Boundary::Collected)
}

fn validate_staged_targets(intent: &SnapshotIntent) -> anyhow::Result<()> {
    for target in [&intent.jail_vmstate, &intent.jail_mem] {
        let meta = std::fs::symlink_metadata(target)?;
        if !meta.file_type().is_file() {
            anyhow::bail!("snapshot jail target is no longer a regular file");
        }
        if intent.boundary == Boundary::TargetsStaged && meta.len() != 0 {
            anyhow::bail!("snapshot jail target changed before the create request");
        }
    }
    Ok(())
}

fn snapshot_ops() -> [FcOp; 2] {
    [
        FcOp::PauseVm,
        FcOp::CreateFullSnapshot {
            snapshot_path: PathBuf::from(format!("/{}", snapshot::SNAPSHOT_VMSTATE_FILE)),
            mem_file_path: PathBuf::from(format!("/{}", snapshot::SNAPSHOT_MEM_FILE)),
        },
    ]
}

async fn execute_op(socket: &Path, op: &FcOp, config: &RoomsConfig) -> anyhow::Result<()> {
    let (method, endpoint, body) = op_request(op);
    if method == "PATCH" {
        transport::api_patch(socket, endpoint, &body, config).await?;
        return Ok(());
    }
    transport::api_put(socket, endpoint, &body, config).await?;
    Ok(())
}

fn op_request(op: &FcOp) -> (&'static str, &'static str, serde_json::Value) {
    match op {
        FcOp::PauseVm => ("PATCH", "/vm", serde_json::json!({"state": "Paused"})),
        FcOp::CreateFullSnapshot {
            snapshot_path,
            mem_file_path,
        } => (
            "PUT",
            "/snapshot/create",
            serde_json::json!({
                "snapshot_type": "Full",
                "snapshot_path": snapshot_path,
                "mem_file_path": mem_file_path,
            }),
        ),
    }
}

async fn resume_vm(socket: &Path, config: &RoomsConfig) -> Result<(), FirecrackerError> {
    transport::api_patch(
        socket,
        "/vm",
        &serde_json::json!({"state": "Resumed"}),
        config,
    )
    .await
}

fn reserve_slot(config: &RoomsConfig, intent: &SnapshotIntent) -> anyhow::Result<()> {
    let state = state_base(config)?;
    let index = intent
        .meta
        .slot_index
        .ok_or_else(|| anyhow::anyhow!("snapshot plan lost its slot"))?;
    match slot::reserve(&state, index, &intent.snapshot_id, &intent.base_room_id)? {
        Reserved::Transferred | Reserved::AlreadyReserved => Ok(()),
        Reserved::NotOwned => {
            anyhow::bail!("base no longer owns slot {index}; preserving snapshot intent")
        }
    }
}

fn reap_base(config: &RoomsConfig, intent: &SnapshotIntent) -> anyhow::Result<()> {
    let room_dir = config
        .room_dir(&intent.base_room_id)
        .ok_or_else(|| anyhow::anyhow!("cannot resolve room directory"))?;
    let jail_dir = config
        .jail_instance_dir(&intent.base_room_id)
        .ok_or_else(|| anyhow::anyhow!("cannot resolve jail directory"))?;
    let socket = config
        .jail_socket(&intent.base_room_id)
        .ok_or_else(|| anyhow::anyhow!("cannot resolve socket"))?;
    if let Some(meta) = room::read(&room_dir)? {
        let Some(pid) = meta.pid else {
            anyhow::bail!("base has no recorded process id; preserving reservation");
        };
        match firecracker::terminate_by_identity(pid, meta.pid_starttime, config.cleanup_grace) {
            KillSignalOutcome::Signaled | KillSignalOutcome::AlreadyExited => {}
            KillSignalOutcome::Indeterminate => {
                anyhow::bail!("base process identity became uncertain; preserving reservation")
            }
            KillSignalOutcome::Survived => {
                anyhow::bail!("base process survived termination; preserving reservation")
            }
        }
    }
    let tap = intent
        .meta
        .slot_index
        .map(|index| format!("tap-fc{index}"))
        .ok_or_else(|| anyhow::anyhow!("snapshot plan lost its slot"))?;
    firecracker::reap_reserved_base(&room_dir, &jail_dir, &socket, &tap, config)?;
    Ok(())
}

fn collect_pair(intent: &SnapshotIntent) -> anyhow::Result<()> {
    let (uid, gid) = firecracker::lookup_firecracker_ids()?;
    copy_private(&intent.jail_vmstate, &intent.host_vmstate, uid, gid)?;
    copy_private(&intent.jail_mem, &intent.host_mem, uid, gid)?;
    validate_collected(intent)?;
    sync_dir(&intent.out_dir)?;
    Ok(())
}

fn copy_private(source: &Path, target: &Path, uid: u32, gid: u32) -> anyhow::Result<()> {
    ensure_regular_nonempty(source)?;
    let name = target
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow::anyhow!("snapshot target has no file name"))?;
    let tmp = target.with_file_name(format!(".{name}.tmp"));
    let mut input = File::open(source)?;
    let mut output = open_private(&tmp)?;
    std::io::copy(&mut input, &mut output)?;
    set_owner_private(&tmp, uid, gid)?;
    output.sync_all()?;
    drop(output);
    std::fs::rename(&tmp, target)?;
    sync_dir(
        target
            .parent()
            .ok_or_else(|| anyhow::anyhow!("snapshot target has no parent"))?,
    )?;
    Ok(())
}

fn stage_targets(intent: &SnapshotIntent) -> anyhow::Result<()> {
    let (uid, gid) = firecracker::lookup_firecracker_ids()?;
    for target in [&intent.jail_vmstate, &intent.jail_mem] {
        match std::fs::symlink_metadata(target) {
            Ok(meta) if meta.file_type().is_file() && meta.len() == 0 => {}
            Ok(_) => anyhow::bail!(
                "refusing non-empty or non-regular snapshot jail target {}",
                target.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let file = open_private_create_new(target)?;
                file.sync_all()?;
            }
            Err(error) => return Err(error.into()),
        }
        set_owner_private(target, uid, gid)?;
    }
    let parent = intent
        .jail_vmstate
        .parent()
        .ok_or_else(|| anyhow::anyhow!("jail target has no parent"))?;
    sync_dir(parent)?;
    Ok(())
}

fn ensure_targets_absent(intent: &SnapshotIntent) -> anyhow::Result<()> {
    for target in [&intent.jail_vmstate, &intent.jail_mem] {
        match std::fs::symlink_metadata(target) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
            Ok(_) => anyhow::bail!(
                "refusing pre-existing snapshot jail target {}",
                target.display()
            ),
        }
    }
    Ok(())
}

fn validate_collected(intent: &SnapshotIntent) -> anyhow::Result<()> {
    ensure_regular_nonempty(&intent.host_vmstate)?;
    ensure_regular_nonempty(&intent.host_mem)
}

fn ensure_regular_nonempty(path: &Path) -> anyhow::Result<()> {
    let meta = std::fs::symlink_metadata(path)?;
    if !meta.file_type().is_file() || meta.len() == 0 {
        anyhow::bail!(
            "snapshot artifact is missing, empty, or not regular: {}",
            path.display()
        );
    }
    Ok(())
}

fn resolve_output(
    config: &RoomsConfig,
    state: &Path,
    base_id: &str,
    snapshot_id: &str,
    explicit: Option<&Path>,
) -> anyhow::Result<PathBuf> {
    let candidate = explicit.map_or_else(
        || state.join(SNAPSHOTS_DIR).join(snapshot_id),
        Path::to_path_buf,
    );
    let resolved = canonical_candidate(&candidate)?;
    let snapshot_index = canonical_candidate(&state.join(SNAPSHOT_INTENTS_DIR))?;
    let restore_index = canonical_candidate(&state.join(RESTORE_INTENTS_DIR))?;
    for index in [&snapshot_index, &restore_index] {
        if overlaps(&resolved, index) {
            anyhow::bail!(
                "snapshot output overlaps transaction index {}",
                index.display()
            );
        }
    }
    for managed in managed_cleanup_roots(config, base_id)? {
        if resolved.starts_with(&managed) {
            anyhow::bail!(
                "snapshot output is under managed cleanup tree {}",
                managed.display()
            );
        }
    }
    Ok(resolved)
}

fn validate_resume_output(
    intent: &SnapshotIntent,
    explicit_out: Option<&Path>,
) -> anyhow::Result<()> {
    let Some(explicit) = explicit_out else {
        return Ok(());
    };
    if canonical_candidate(explicit)? == intent.out_dir {
        return Ok(());
    }
    anyhow::bail!(
        "base already has snapshot transaction {} at {}; requested output differs",
        intent.snapshot_id,
        intent.out_dir.display()
    )
}

fn managed_cleanup_roots(config: &RoomsConfig, base_id: &str) -> anyhow::Result<Vec<PathBuf>> {
    let mut ids: Vec<String> = crate::registry::list_rooms(config)?
        .into_iter()
        .map(|entry| entry.id)
        .collect();
    if !ids.iter().any(|id| id == base_id) {
        ids.push(base_id.to_owned());
    }
    let mut roots = Vec::new();
    for id in ids {
        if let Some(room) = config.room_dir(&id) {
            roots.push(canonical_candidate(&room)?);
        }
        if let Some(jail) = config.jail_instance_dir(&id) {
            roots.push(canonical_candidate(&jail)?);
        }
    }
    Ok(roots)
}

fn canonical_candidate(path: &Path) -> anyhow::Result<PathBuf> {
    if path.exists() {
        return Ok(path.canonicalize()?);
    }
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("path has no parent: {}", path.display()))?;
    std::fs::create_dir_all(parent)?;
    let parent = parent.canonicalize()?;
    let name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("path has no leaf: {}", path.display()))?;
    Ok(parent.join(name))
}

fn overlaps(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

fn prepare_output(path: &Path) -> anyhow::Result<()> {
    if path.exists() {
        let meta = std::fs::symlink_metadata(path)?;
        if !meta.file_type().is_dir() {
            anyhow::bail!("snapshot output is not a directory: {}", path.display());
        }
        if std::fs::read_dir(path)?.next().is_some() {
            anyhow::bail!(
                "refusing unrelated non-empty snapshot output {}",
                path.display()
            );
        }
        return set_dir_private(path);
    }
    std::fs::create_dir(path)?;
    set_dir_private(path)
}

fn create_intent_exclusive(config: &RoomsConfig, intent: &SnapshotIntent) -> anyhow::Result<()> {
    let dir = intent_dir(config)?;
    std::fs::create_dir_all(&dir)?;
    set_dir_private(&dir)?;
    let path = intent_path(config, &intent.base_room_id)?;
    let bytes = serde_json::to_vec_pretty(intent)?;
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    set_create_mode(&mut options);
    let mut file = options.open(&path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            return anyhow::anyhow!(
                "snapshot transaction already pending for base {}",
                intent.base_room_id
            );
        }
        anyhow::Error::from(error)
    })?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    sync_dir(&dir)?;
    Ok(())
}

struct IntentLock {
    _file: File,
}

fn acquire_lock(config: &RoomsConfig, base_id: &str) -> anyhow::Result<IntentLock> {
    let dir = intent_dir(config)?;
    std::fs::create_dir_all(&dir)?;
    set_dir_private(&dir)?;
    let path = dir.join(format!("{base_id}.lock"));
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .map_err(anyhow::Error::from)?;
    file.try_lock()
        .map_err(|_| anyhow::anyhow!("snapshot transaction for base {base_id} is busy"))?;
    file.sync_all()?;
    sync_dir(&dir)?;
    Ok(IntentLock { _file: file })
}

fn advance(
    config: &RoomsConfig,
    intent: &mut SnapshotIntent,
    boundary: Boundary,
) -> anyhow::Result<()> {
    intent.boundary = boundary;
    write_intent_atomic(config, intent)
}

fn write_intent_atomic(config: &RoomsConfig, intent: &SnapshotIntent) -> anyhow::Result<()> {
    let path = intent_path(config, &intent.base_room_id)?;
    let dir = intent_dir(config)?;
    let tmp = dir.join(format!(".{}.tmp", intent.base_room_id));
    let bytes = serde_json::to_vec_pretty(intent)?;
    let mut file = open_private(&tmp)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp, path)?;
    sync_dir(&dir)?;
    Ok(())
}

fn finish_index(config: &RoomsConfig, intent: &SnapshotIntent) -> anyhow::Result<()> {
    let path = intent_path(config, &intent.base_room_id)?;
    match std::fs::remove_file(path) {
        Ok(()) => sync_dir(&intent_dir(config)?)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn read_intents(config: &RoomsConfig) -> anyhow::Result<Vec<SnapshotIntent>> {
    let dir = intent_dir(config)?;
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut intents = Vec::new();
    for entry in entries {
        let entry = entry?;
        if entry.path().extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        if !entry.file_type()?.is_file() {
            anyhow::bail!(
                "snapshot intent is not a regular file: {}",
                entry.path().display()
            );
        }
        let bytes = std::fs::read(entry.path())?;
        let intent: SnapshotIntent = serde_json::from_slice(&bytes)?;
        validate_intent(config, &intent, &entry.path())?;
        intents.push(intent);
    }
    intents.sort_by(|left, right| left.snapshot_id.cmp(&right.snapshot_id));
    Ok(intents)
}

fn validate_intent(
    config: &RoomsConfig,
    intent: &SnapshotIntent,
    path: &Path,
) -> anyhow::Result<()> {
    if intent.schema_version != INTENT_SCHEMA_VERSION {
        anyhow::bail!(
            "unsupported snapshot intent schema {}",
            intent.schema_version
        );
    }
    if !crate::registry::is_valid_room_id(&intent.base_room_id)
        || !crate::registry::is_valid_room_id(&intent.snapshot_id)
    {
        anyhow::bail!("snapshot intent contains an invalid id");
    }
    if path.file_stem().and_then(|stem| stem.to_str()) != Some(intent.base_room_id.as_str())
        || intent.meta.base_room_id != intent.base_room_id
        || intent.meta.snapshot_id != intent.snapshot_id
    {
        anyhow::bail!("snapshot intent identity fields disagree");
    }
    let safe_out = resolve_output(
        config,
        &state_base(config)?,
        &intent.base_room_id,
        &intent.snapshot_id,
        Some(&intent.out_dir),
    )?;
    if safe_out != intent.out_dir {
        anyhow::bail!("snapshot intent output is no longer canonical");
    }
    let jail_root = config
        .jail_root_dir(&intent.base_room_id)
        .ok_or_else(|| anyhow::anyhow!("cannot resolve intent jail root"))?;
    if intent.host_vmstate != intent.out_dir.join(snapshot::SNAPSHOT_VMSTATE_FILE)
        || intent.host_mem != intent.out_dir.join(snapshot::SNAPSHOT_MEM_FILE)
        || intent.jail_vmstate != jail_root.join(snapshot::SNAPSHOT_VMSTATE_FILE)
        || intent.jail_mem != jail_root.join(snapshot::SNAPSHOT_MEM_FILE)
    {
        anyhow::bail!("snapshot intent artifact paths disagree with their roots");
    }
    Ok(())
}

fn pending_summary(intent: &SnapshotIntent) -> PendingSnapshot {
    PendingSnapshot {
        snapshot_id: intent.snapshot_id.clone(),
        base_room: intent.base_room_id.clone(),
        boundary: format!("{:?}", intent.boundary).to_lowercase(),
        next_action: format!(
            "run `rooms snapshot-recover {}` to resume safely",
            intent.snapshot_id
        ),
    }
}

fn base_is_conclusively_dead(
    config: &RoomsConfig,
    intent: &SnapshotIntent,
) -> anyhow::Result<bool> {
    let room_dir = config
        .room_dir(&intent.base_room_id)
        .ok_or_else(|| anyhow::anyhow!("cannot resolve room dir"))?;
    let Some(meta) = room::read(&room_dir)? else {
        return Ok(true);
    };
    Ok(room::probe(meta.pid, meta.pid_starttime) == Liveness::Dead)
}

fn terminate_partial_base(config: &RoomsConfig, intent: &SnapshotIntent) -> anyhow::Result<()> {
    let room_dir = config
        .room_dir(&intent.base_room_id)
        .ok_or_else(|| anyhow::anyhow!("cannot resolve room dir"))?;
    let Some(meta) = room::read(&room_dir)? else {
        return Ok(());
    };
    let Some(pid) = meta.pid else {
        anyhow::bail!("partial snapshot base has no recorded process id");
    };
    match firecracker::terminate_by_identity(pid, meta.pid_starttime, config.cleanup_grace) {
        KillSignalOutcome::Signaled | KillSignalOutcome::AlreadyExited => Ok(()),
        KillSignalOutcome::Indeterminate => {
            anyhow::bail!("partial snapshot base identity is indeterminate")
        }
        KillSignalOutcome::Survived => anyhow::bail!("partial snapshot base survived termination"),
    }
}

fn any_artifact_nonempty(intent: &SnapshotIntent) -> bool {
    [
        &intent.host_vmstate,
        &intent.host_mem,
        &intent.jail_vmstate,
        &intent.jail_mem,
    ]
    .into_iter()
    .any(|path| std::fs::metadata(path).is_ok_and(|meta| meta.len() > 0))
}

fn abort_dead_partial(config: &RoomsConfig, intent: &SnapshotIntent) -> anyhow::Result<()> {
    if intent.boundary >= Boundary::Reserved {
        anyhow::bail!("a reserved snapshot transaction is not abortable");
    }
    if intent.out_dir.join(snapshot::SNAPSHOT_META_FILE).exists() {
        anyhow::bail!("refusing abort: snapshot metadata is already published");
    }
    let state = state_base(config)?;
    let index = intent
        .meta
        .slot_index
        .ok_or_else(|| anyhow::anyhow!("snapshot plan lost its slot"))?;
    if !slot::claimed_by(&state, index, &intent.base_room_id)? {
        anyhow::bail!("refusing abort: slot {index} no longer holds the exact base claim");
    }
    remove_partial_outputs(intent)?;
    abort_reap(config, intent, index)?;
    finish_index(config, intent)
}

fn abort_reap(config: &RoomsConfig, intent: &SnapshotIntent, index: u8) -> anyhow::Result<()> {
    let room_dir = config
        .room_dir(&intent.base_room_id)
        .ok_or_else(|| anyhow::anyhow!("cannot resolve room dir"))?;
    let jail_dir = config
        .jail_instance_dir(&intent.base_room_id)
        .ok_or_else(|| anyhow::anyhow!("cannot resolve jail dir"))?;
    let socket = config
        .jail_socket(&intent.base_room_id)
        .ok_or_else(|| anyhow::anyhow!("cannot resolve socket"))?;
    let tap = format!("tap-fc{index}");
    firecracker::reap_reserved_base(&room_dir, &jail_dir, &socket, &tap, config)?;
    match slot::free(&state_base(config)?, index, &intent.base_room_id)? {
        Freed::Removed | Freed::AlreadyFree => Ok(()),
        Freed::AlreadyReassigned => {
            anyhow::bail!("slot {index} changed owner during partial-snapshot abort")
        }
    }
}

fn remove_partial_outputs(intent: &SnapshotIntent) -> anyhow::Result<()> {
    let vm_tmp = intent
        .host_vmstate
        .with_file_name(format!(".{}.tmp", snapshot::SNAPSHOT_VMSTATE_FILE));
    let mem_tmp = intent
        .host_mem
        .with_file_name(format!(".{}.tmp", snapshot::SNAPSHOT_MEM_FILE));
    for path in [
        &intent.host_vmstate,
        &intent.host_mem,
        &intent.jail_vmstate,
        &intent.jail_mem,
        &vm_tmp,
        &mem_tmp,
    ] {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    sync_dir(&intent.out_dir)?;
    Ok(())
}

fn result_from(intent: &SnapshotIntent) -> anyhow::Result<SnapshotResult> {
    Ok(SnapshotResult {
        snapshot_id: intent.snapshot_id.clone(),
        directory: intent.out_dir.clone(),
        base_room: intent.base_room_id.clone(),
        slot: intent
            .meta
            .slot_index
            .ok_or_else(|| anyhow::anyhow!("snapshot metadata lost slot"))?,
        guest_ip: intent
            .meta
            .guest_ip
            .ok_or_else(|| anyhow::anyhow!("snapshot metadata lost guest ip"))?,
        provenance: intent.meta.provenance,
    })
}

fn active_vsock(jail_root: &Path) -> anyhow::Result<bool> {
    for entry in std::fs::read_dir(jail_root)? {
        let name = entry?.file_name();
        if name
            .to_str()
            .is_some_and(|value| value.starts_with(&format!("{}_", crate::vsock::UDS_NAME)))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn firecracker_version(config: &RoomsConfig) -> anyhow::Result<String> {
    let output = std::process::Command::new(&config.firecracker_binary)
        .arg("--version")
        .output()?;
    if !output.status.success() {
        anyhow::bail!(
            "firecracker --version failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn sha256_file(path: &Path) -> anyhow::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0_u8; 64 * 1024].into_boxed_slice();
    loop {
        let read = file.read(&mut buf)?;
        if read == 0 {
            break;
        }
        let chunk = buf
            .get(..read)
            .ok_or_else(|| anyhow::anyhow!("hash buffer read exceeded capacity"))?;
        hasher.update(chunk);
    }
    Ok(format!("sha256:{:x}", hasher.finalize()))
}

fn state_base(config: &RoomsConfig) -> anyhow::Result<PathBuf> {
    config
        .resolved_state_base()
        .ok_or_else(|| anyhow::anyhow!("HOME unset; cannot locate rooms state"))
}

fn intent_dir(config: &RoomsConfig) -> anyhow::Result<PathBuf> {
    config
        .snapshot_intents_dir()
        .ok_or_else(|| anyhow::anyhow!("cannot resolve snapshot intent directory"))
}

fn intent_path(config: &RoomsConfig, base_id: &str) -> anyhow::Result<PathBuf> {
    Ok(intent_dir(config)?.join(format!("{base_id}.json")))
}

fn open_private(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).write(true).truncate(true);
    set_create_mode(&mut options);
    options.open(path)
}

#[cfg(unix)]
fn sync_dir(path: &Path) -> std::io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(not(unix))]
#[allow(
    clippy::unnecessary_wraps,
    reason = "keeps durable transaction call sites identical across cfg targets"
)]
const fn sync_dir(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

fn open_private_create_new(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    set_create_mode(&mut options);
    options.open(path)
}

#[cfg(unix)]
fn set_create_mode(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(0o600);
}

#[cfg(not(unix))]
const fn set_create_mode(_options: &mut OpenOptions) {}

#[cfg(unix)]
fn set_dir_private(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
#[allow(
    clippy::unnecessary_wraps,
    reason = "keeps the private-permissions contract identical across cfg targets"
)]
const fn set_dir_private(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_owner_private(path: &Path, uid: u32, gid: u32) -> anyhow::Result<()> {
    use std::os::unix::fs::{chown, PermissionsExt};
    chown(path, Some(uid), Some(gid))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
#[allow(
    clippy::unnecessary_wraps,
    reason = "keeps the ownership contract identical across cfg targets"
)]
const fn set_owner_private(_path: &Path, _uid: u32, _gid: u32) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::indexing_slicing, reason = "test module")]

    use super::{
        acquire_lock, create_intent_exclusive, op_request, overlaps, pending_for_base,
        resolve_output, sha256_file, snapshot_ops, Boundary, SnapshotIntent, INTENT_SCHEMA_VERSION,
    };
    use crate::config::RoomsConfig;
    use crate::room::{Provenance, RoomMeta, Slot};
    use crate::snapshot::SnapshotMeta;
    use chrono::Utc;
    use std::net::Ipv4Addr;
    use std::path::Path;

    const BASE_ID: &str = "00000000000000000000000001";
    const SNAPSHOT_ID: &str = "00000000000000000000000002";

    fn config(base: &Path) -> RoomsConfig {
        RoomsConfig {
            state_base: Some(base.to_path_buf()),
            ..RoomsConfig::default()
        }
    }

    fn intent(config: &RoomsConfig) -> SnapshotIntent {
        let snapshots = config.snapshots_dir().expect("snapshots");
        std::fs::create_dir_all(&snapshots).expect("create snapshots");
        let out = snapshots
            .canonicalize()
            .expect("canonical")
            .join(SNAPSHOT_ID);
        let jail = config.jail_root_dir(BASE_ID).expect("jail");
        SnapshotIntent {
            schema_version: INTENT_SCHEMA_VERSION,
            snapshot_id: SNAPSHOT_ID.to_owned(),
            base_room_id: BASE_ID.to_owned(),
            meta: SnapshotMeta {
                schema_version: 1,
                snapshot_id: SNAPSHOT_ID.to_owned(),
                created_at: Utc::now(),
                fc_version: "1.9.0".to_owned(),
                rootfs_hash: "abc".to_owned(),
                base_room_id: BASE_ID.to_owned(),
                slot_index: Some(1),
                guest_ip: Some(Ipv4Addr::new(172, 16, 0, 6)),
                base_repo_sha: None,
                provenance: Provenance::Neutral,
            },
            out_dir: out.clone(),
            host_vmstate: out.join(crate::snapshot::SNAPSHOT_VMSTATE_FILE),
            host_mem: out.join(crate::snapshot::SNAPSHOT_MEM_FILE),
            jail_vmstate: jail.join(crate::snapshot::SNAPSHOT_VMSTATE_FILE),
            jail_mem: jail.join(crate::snapshot::SNAPSHOT_MEM_FILE),
            boundary: Boundary::Pending,
        }
    }

    fn write_room(config: &RoomsConfig) {
        let dir = config.room_dir(BASE_ID).expect("room dir");
        std::fs::create_dir_all(&dir).expect("create room");
        let mut meta = RoomMeta::new_base(
            BASE_ID.to_owned(),
            Some("base".to_owned()),
            None,
            None,
            true,
            Utc::now(),
        );
        meta.slot = Some(Slot {
            index: 1,
            tap: "tap-fc1".to_owned(),
            gateway: Ipv4Addr::new(172, 16, 0, 5),
            guest: Ipv4Addr::new(172, 16, 0, 6),
            prefix: 30,
        });
        meta.seal().expect("seal");
        crate::room::write_atomic(&dir, &meta).expect("write room");
    }

    #[test]
    fn overlap_is_symmetric() {
        assert!(overlaps(Path::new("/a"), Path::new("/a/b")));
        assert!(overlaps(Path::new("/a/b"), Path::new("/a")));
        assert!(!overlaps(Path::new("/a/b"), Path::new("/a/c")));
    }

    #[test]
    fn sha256_is_exact() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("rootfs");
        std::fs::write(&path, b"abc").expect("write");
        assert_eq!(
            sha256_file(&path).expect("hash"),
            "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn operations_map_to_exact_firecracker_requests_in_order() {
        let [pause, create] = snapshot_ops();
        let (method, endpoint, body) = op_request(&pause);
        assert_eq!((method, endpoint), ("PATCH", "/vm"));
        assert_eq!(body, serde_json::json!({"state": "Paused"}));
        let (method, endpoint, body) = op_request(&create);
        assert_eq!((method, endpoint), ("PUT", "/snapshot/create"));
        assert_eq!(
            body,
            serde_json::json!({
                "snapshot_type": "Full",
                "snapshot_path": "/snapshot.vmstate",
                "mem_file_path": "/snapshot.mem"
            })
        );
    }

    #[test]
    fn per_base_intent_is_exclusive_and_indexed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = config(dir.path());
        let intent = intent(&config);
        create_intent_exclusive(&config, &intent).expect("first intent");
        let error = create_intent_exclusive(&config, &intent).expect_err("second must lose");
        assert!(error.to_string().contains("already pending"));
        let pending = pending_for_base(&config, BASE_ID)
            .expect("read index")
            .expect("pending");
        assert_eq!(pending.snapshot_id, SNAPSHOT_ID);
    }

    #[test]
    fn transaction_lock_rejects_a_concurrent_driver() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = config(dir.path());
        let _first = acquire_lock(&config, BASE_ID).expect("first lock");
        assert!(acquire_lock(&config, BASE_ID).is_err());
    }

    #[test]
    fn kill_and_gc_refuse_a_pending_snapshot_base() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = config(dir.path());
        write_room(&config);
        create_intent_exclusive(&config, &intent(&config)).expect("intent");

        let killed = crate::registry::kill(&config, BASE_ID).expect("kill report");
        assert_eq!(killed.exit_code(), 2);
        assert!(killed.outcomes[0].reason.contains(SNAPSHOT_ID));

        let gc = crate::registry::gc(&config, &crate::registry::GcOptions::default())
            .expect("gc report");
        let outcome = gc
            .outcomes
            .iter()
            .find(|outcome| outcome.id == BASE_ID)
            .expect("base outcome");
        assert!(!outcome.reaped);
        assert!(outcome.reason.contains("protected pending snapshot"));
        assert!(config.room_dir(BASE_ID).expect("room").exists());
    }

    #[test]
    fn absent_base_transaction_remains_visible_to_kill_and_gc() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = config(dir.path());
        create_intent_exclusive(&config, &intent(&config)).expect("intent");
        let killed = crate::registry::kill(&config, BASE_ID).expect("kill report");
        assert_eq!(killed.exit_code(), 2);
        assert!(killed.outcomes[0].reason.contains(SNAPSHOT_ID));
        let gc = crate::registry::gc(&config, &crate::registry::GcOptions::default())
            .expect("gc report");
        assert!(gc
            .outcomes
            .iter()
            .any(|outcome| outcome.id == BASE_ID && outcome.reason.contains(SNAPSHOT_ID)));
    }

    #[test]
    fn output_rejects_transaction_indexes_in_both_directions() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = config(dir.path());
        let state = dir.path().canonicalize().expect("canonical state");
        assert!(resolve_output(
            &config,
            &state,
            BASE_ID,
            SNAPSHOT_ID,
            Some(&state.join("snapshot-intents/child"))
        )
        .is_err());
        assert!(resolve_output(&config, &state, BASE_ID, SNAPSHOT_ID, Some(&state)).is_err());
    }

    #[test]
    fn output_rejects_a_custom_managed_jail_tree() {
        let dir = tempfile::tempdir().expect("tempdir");
        let jail = dir.path().join("custom-jail");
        let config = RoomsConfig {
            state_base: Some(dir.path().join("state")),
            jailer_chroot_base: Some(jail.clone()),
            ..RoomsConfig::default()
        };
        std::fs::create_dir_all(&jail).expect("jail root");
        let state = config.resolved_state_base().expect("state");
        std::fs::create_dir_all(&state).expect("create state");
        let state = state.canonicalize().expect("canonical");
        let output = config
            .jail_instance_dir(BASE_ID)
            .expect("instance")
            .join("snapshot");
        assert!(resolve_output(&config, &state, BASE_ID, SNAPSHOT_ID, Some(&output)).is_err());
    }
}
