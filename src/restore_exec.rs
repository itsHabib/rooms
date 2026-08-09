//! Recoverable execution of the restore policy.
//!
//! [`crate::restore`] decides what a compatible restore means; this module
//! owns the host effects and their durable transaction boundaries, mirroring
//! [`crate::snapshot_exec`]'s shape: a per-room intent under
//! `restore-intents/` is the durable claim, every boundary advance is an
//! atomic rewrite, and the intent survives (as a tombstone) until both
//! resource cleanup and the lease return are durably complete.

use std::io::{Read, Write as _};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::config::RoomsConfig;
use crate::firecracker::{self, RestoreLaunch, RestoreSpawnRequest};
use crate::restore::{self, RestoreOp};
use crate::room::{self, Liveness};
use crate::slot;
use crate::snapshot::{self, SnapshotMeta};
use crate::snapshot_exec::{canonical_candidate, firecracker_version, overlaps, sha256_file};
use crate::{egress, transport, vsock, witness};

const INTENT_SCHEMA_VERSION: u32 = 1;

/// Bytes of fresh host randomness mixed into the resumed guest's CRNG.
const RESUME_ENTROPY_BYTES: usize = 64;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
enum Boundary {
    /// Durable before any child exists: records the planned identity/paths.
    PreSpawn,
    /// The child's pid/starttime are recorded; the barrier may release.
    Spawned,
    /// The snapshot reservation is leased to this room.
    Leased,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RestoreIntent {
    schema_version: u32,
    room_id: String,
    snapshot_id: String,
    slot_index: u8,
    tap: String,
    snapshot_dir: PathBuf,
    image: PathBuf,
    #[serde(default)]
    pid: Option<u32>,
    #[serde(default)]
    pid_starttime: Option<u64>,
    boundary: Boundary,
}

/// What a completed restore hands back to the orchestration layer: a live,
/// acked room whose teardown must end in [`finish_teardown`].
pub struct Restored {
    pub vm: firecracker::BootedVm,
    pub slot: room::Slot,
    pub room_id: String,
    pub snapshot_id: String,
}

/// Inputs to [`restore`].
pub struct RestoreRequest<'a> {
    pub snapshot_dir: &'a Path,
    /// The backing rootfs; its hash must equal the snapshot's pinned hash.
    pub image: &'a Path,
    /// `--slot` override; must equal the frozen slot when given.
    pub target_slot: Option<u8>,
    /// Room label for `rooms ls`.
    pub label: Option<String>,
    pub keep: bool,
    /// Witness capture on the leased tap (custody, pre-resume).
    pub witness: bool,
    /// Egress policy installed before the guest resumes (custody).
    pub egress: &'a egress::Plan,
    /// Admitted secrets, delivered only through the resume nudge.
    pub secrets: Option<vsock::SecretsPayload>,
    /// `--out` directory (already lifecycle-validated by the CLI); checked
    /// here against every managed root before any process or lease exists.
    pub out_dir: Option<&'a Path>,
    /// Bound on the guest's hygiene ack after resume.
    pub ack_timeout: std::time::Duration,
}

/// Drive a full restore: validate, spawn behind the barrier, lease the frozen
/// slot, install custody, load + resume, and gate on the hygiene ack.
///
/// # Errors
/// Fails closed at every step; a failure after the lease attempts full
/// teardown and returns the lease before the intent tombstone is cleared.
pub async fn restore(config: &RoomsConfig, req: RestoreRequest<'_>) -> anyhow::Result<Restored> {
    let meta = read_snapshot_meta(req.snapshot_dir)?;
    let image = canonical_candidate(req.image)?;
    let snapshot_dir = canonical_candidate(req.snapshot_dir)?;
    validate_output_disjoint(config, req.out_dir, &image, &snapshot_dir)?;

    let host_fc = firecracker_version(config)?;
    let rootfs_hash = sha256_file(&image)?;
    let plan = restore::plan_restore(&meta, &host_fc, &rootfs_hash)?;
    let slot_index = meta
        .slot_index
        .ok_or_else(|| anyhow::anyhow!("snapshot metadata carries no frozen slot"))?;
    if let Some(requested) = req.target_slot {
        if requested != slot_index {
            anyhow::bail!(
                "--slot {requested} differs from the snapshot's frozen slot {slot_index}; a restore must reclaim the exact frozen slot"
            );
        }
    }

    let room_id = firecracker::mint_room_id();
    let mut intent = RestoreIntent {
        schema_version: INTENT_SCHEMA_VERSION,
        room_id: room_id.clone(),
        snapshot_id: meta.snapshot_id.clone(),
        slot_index,
        tap: format!("tap-fc{slot_index}"),
        snapshot_dir: snapshot_dir.clone(),
        image: image.clone(),
        pid: None,
        pid_starttime: None,
        boundary: Boundary::PreSpawn,
    };
    create_intent_exclusive(config, &intent)?;

    let descriptor = room::RoomDescriptor {
        command: req.label.clone(),
        keep: req.keep,
    };
    let spawn_req = RestoreSpawnRequest {
        room_id: &room_id,
        rootfs: &image,
        host_vmstate: &snapshot_dir.join(snapshot::SNAPSHOT_VMSTATE_FILE),
        host_mem: &snapshot_dir.join(snapshot::SNAPSHOT_MEM_FILE),
        descriptor: &descriptor,
        snapshot_id: &meta.snapshot_id,
    };
    let mut launch = match firecracker::spawn_restore(&spawn_req, config).await {
        Ok(launch) => launch,
        Err(e) => {
            // spawn_restore may have staged the room/jail dirs before failing
            // (no lease was taken — this is still PreSpawn). Clean the planned
            // paths, and clear the intent ONLY when that cleanup is proven
            // complete; otherwise keep the tombstone so `rooms gc` retries from
            // it, rather than orphaning a room dir GC would never touch.
            clean_pre_spawn(config, &room_id);
            return Err(e.into());
        }
    };

    match drive_to_ready(config, &req, &meta, &mut intent, &mut launch, plan.ops).await {
        Ok((slot, witness_capture)) => Ok(Restored {
            vm: launch.into_booted(witness_capture),
            slot,
            room_id,
            snapshot_id: meta.snapshot_id,
        }),
        Err(e) => {
            abort_launch(config, launch, &intent);
            Err(e)
        }
    }
}

/// The ordered post-spawn steps, factored so the caller owns the one abort
/// path. Any error after the lease leaves the intent for the abort/GC flow.
async fn drive_to_ready(
    config: &RoomsConfig,
    req: &RestoreRequest<'_>,
    meta: &SnapshotMeta,
    intent: &mut RestoreIntent,
    launch: &mut RestoreLaunch,
    ops: Vec<RestoreOp>,
) -> anyhow::Result<(room::Slot, Option<witness::Capture>)> {
    // Durable pid/starttime before the barrier releases: a crash right here
    // leaves an inert launcher that exits on pipe EOF, classifiable from the
    // intent alone.
    intent.pid = launch.pid();
    intent.pid_starttime = intent.pid.and_then(room::starttime_of);
    intent.boundary = Boundary::Spawned;
    write_intent_atomic(config, intent)?;
    launch.release_barrier().await?;
    launch.wait_api(config.api_socket_timeout).await?;

    // Lease the frozen slot only now that the room has a conclusive liveness
    // breadcrumb. Busy or foreign leases fail closed.
    let state = state_base(config)?;
    let slot = slot::lease(
        &state,
        intent.slot_index,
        &intent.snapshot_id,
        &intent.room_id,
    )?;
    intent.boundary = Boundary::Leased;
    write_intent_atomic(config, intent)?;
    record_slot_on_room(config, &intent.room_id, &slot)?;
    launch.attach_leased_slot(&slot)?;

    let jail_root = config
        .jail_root_dir(&intent.room_id)
        .ok_or_else(|| anyhow::anyhow!("cannot resolve jail root"))?;
    let room_dir = config
        .room_dir(&intent.room_id)
        .ok_or_else(|| anyhow::anyhow!("cannot resolve room dir"))?;
    let (fc_uid, fc_gid) = firecracker::lookup_firecracker_ids()?;

    let mut witness_capture = None;
    let mut resume_delivery = None;
    for op in ops {
        match op {
            RestoreOp::InstallCustody => {
                if req.witness {
                    witness_capture = Some(
                        witness::Capture::start(&slot.tap, &room_dir)
                            .await
                            .map_err(|e| anyhow::anyhow!("witness: {e}"))?,
                    );
                }
                if req.egress.enforces() {
                    egress::install(&slot.tap, req.egress).map_err(|e| anyhow::anyhow!(e))?;
                }
            }
            RestoreOp::LoadSnapshot {
                snapshot_path,
                mem_backend_path,
            } => {
                // The nudge listener binds before the guest can resume: the
                // polling agent must never find the port unbound after resume.
                resume_delivery = Some(vsock::serve_resume(
                    &jail_root,
                    resume_payload(&intent.room_id, req.secrets.as_ref())?,
                    Some((fc_uid, fc_gid)),
                )?);
                transport::api_put(
                    launch.socket(),
                    "/snapshot/load",
                    &serde_json::json!({
                        "snapshot_path": snapshot_path,
                        "mem_backend": {
                            "backend_type": "File",
                            "backend_path": mem_backend_path,
                        },
                        "resume_vm": false,
                    }),
                    config,
                )
                .await?;
            }
            RestoreOp::ResumeVm => {
                transport::api_patch(
                    launch.socket(),
                    "/vm",
                    &serde_json::json!({"state": "Resumed"}),
                    config,
                )
                .await?;
                info!(room = %intent.room_id, snapshot = %meta.snapshot_id, "microVM resumed from snapshot");
            }
            RestoreOp::ApplyHygieneNudgeAndAwaitAck => {
                let delivery = resume_delivery
                    .take()
                    .ok_or_else(|| anyhow::anyhow!("resume nudge was never armed"))?;
                delivery
                    .await_acked(req.ack_timeout)
                    .await
                    .map_err(|e| anyhow::anyhow!("resume hygiene ack: {e}"))?;
            }
        }
    }
    Ok((slot, witness_capture))
}

/// Build the per-restore nudge payload: identity, host clock, fresh entropy,
/// and the admitted secrets (empty when none).
fn resume_payload(
    room_id: &str,
    secrets: Option<&vsock::SecretsPayload>,
) -> anyhow::Result<vsock::ResumePayload> {
    let mut entropy = vec![0_u8; RESUME_ENTROPY_BYTES];
    let mut urandom = std::fs::File::open("/dev/urandom")?;
    urandom.read_exact(&mut entropy)?;
    Ok(vsock::ResumePayload {
        room_id: room_id.to_owned(),
        epoch_secs: chrono::Utc::now().timestamp(),
        entropy,
        secrets: secrets.map_or_else(
            || vsock::SecretsPayload::encode(&[]),
            vsock::SecretsPayload::clone_bytes,
        ),
    })
}

/// Record the leased slot on the persisted room metadata (the spec's "set
/// `RoomMeta.slot` before creating the TAP").
fn record_slot_on_room(
    config: &RoomsConfig,
    room_id: &str,
    slot: &room::Slot,
) -> anyhow::Result<()> {
    let room_dir = config
        .room_dir(room_id)
        .ok_or_else(|| anyhow::anyhow!("cannot resolve room dir"))?;
    let mut meta = room::read(&room_dir)?
        .ok_or_else(|| anyhow::anyhow!("restore room.json disappeared before lease"))?;
    meta.slot = Some(slot.clone());
    room::write_atomic(&room_dir, &meta)?;
    Ok(())
}

/// Abort a launched restore: kill + clean through the guard, then try the
/// full teardown finish (which returns the lease only after the reap-clean
/// gate). The intent tombstone survives any incomplete step for GC.
fn abort_launch(config: &RoomsConfig, mut launch: RestoreLaunch, intent: &RestoreIntent) {
    // cleanup() runs the guard teardown once (kill child, unmount jail, remove
    // dirs, delete the leased tap); into_booted then moves that already-cleaned
    // guard into a BootedVm we immediately dismiss so its Drop does NOT re-run
    // cleanup. finish_teardown below is the lease-returning reap-clean gate.
    launch.guard_mut().cleanup();
    let mut booted = launch.into_booted(None);
    booted.guard_mut().dismiss();
    drop(booted);
    if let Err(e) = finish_teardown(
        config,
        &intent.room_id,
        &intent.snapshot_id,
        intent.slot_index,
    ) {
        warn!(room = %intent.room_id, error = %e, "restore abort left residue; `rooms gc` will retry from the intent tombstone");
    }
}

/// Clean a failed pre-spawn restore: no child ran and no lease was taken, so
/// only the planned room/jail dirs can exist. Reap them room-only (never the
/// shared slot's tap), and clear the intent only if that succeeds — a residual
/// dir keeps the tombstone for `rooms gc`.
fn clean_pre_spawn(config: &RoomsConfig, room_id: &str) {
    let reaped = (|| -> anyhow::Result<()> {
        let room_dir = config
            .room_dir(room_id)
            .ok_or_else(|| anyhow::anyhow!("cannot resolve room dir"))?;
        let jail_dir = config
            .jail_instance_dir(room_id)
            .ok_or_else(|| anyhow::anyhow!("cannot resolve jail dir"))?;
        let socket = config
            .jail_socket(room_id)
            .ok_or_else(|| anyhow::anyhow!("cannot resolve socket"))?;
        firecracker::reap_room_only(&room_dir, &jail_dir, &socket, config)?;
        Ok(())
    })();
    match reaped {
        Ok(()) => {
            let _ = finish_index(config, room_id);
        }
        Err(e) => {
            warn!(room = %room_id, error = %e, "pre-spawn restore cleanup incomplete; keeping intent tombstone for `rooms gc`");
        }
    }
}

/// The terminal teardown gate: prove the process/jail/room/egress/tap are
/// gone, then return the lease, then (and only then) clear the tombstone.
///
/// # Errors
/// Any residue keeps the intent tombstone and returns the failure.
pub fn finish_teardown(
    config: &RoomsConfig,
    room_id: &str,
    snapshot_id: &str,
    slot_index: u8,
) -> anyhow::Result<()> {
    let room_dir = config
        .room_dir(room_id)
        .ok_or_else(|| anyhow::anyhow!("cannot resolve room dir"))?;
    let jail_dir = config
        .jail_instance_dir(room_id)
        .ok_or_else(|| anyhow::anyhow!("cannot resolve jail dir"))?;
    let socket = config
        .jail_socket(room_id)
        .ok_or_else(|| anyhow::anyhow!("cannot resolve socket"))?;
    let tap = format!("tap-fc{slot_index}");
    let state = state_base(config)?;
    // Reap this room's OWN resources first (process/jail/room dir) — these are
    // keyed by room id and always safe. If they survive, the reap errors and
    // the tombstone stays for a retry, before we touch the shared slot.
    firecracker::reap_room_only(&room_dir, &jail_dir, &socket, config)?;
    // The tap and egress chain are named by slot index alone, so they may be
    // deleted ONLY while we hold the lease: a GC retry after an earlier pass
    // already returned the lease — or a `rooms kill` racing a re-lease — must
    // never delete a tap a *different* room has since re-leased and recreated.
    //
    // `hold_lease_for_teardown` keeps the free-lock held across the delete AND
    // the return, so the whole check→delete→return is atomic — no other slot
    // operation (a competing lease of this index) can interleave. `None` means
    // the slot is not our lease, so we leave its tap/egress to whoever owns it
    // now and just clear our tombstone. (This is also why the RoomGuard is
    // deliberately not given tap ownership — see `attach_leased_slot`: this is
    // the sole deleter of a leased tap.)
    match slot::hold_lease_for_teardown(&state, slot_index, snapshot_id, room_id)? {
        Some(hold) => {
            firecracker::remove_egress_and_tap(&tap).map_err(|e| anyhow::anyhow!(e.to_string()))?;
            hold.return_to_reservation()?;
        }
        None => {
            warn!(
                slot = slot_index,
                "restore teardown holds no lease for this room; leaving the slot tap/egress untouched"
            );
        }
    }
    finish_index(config, room_id)
}

/// One indexed restore transaction, as GC observability.
#[derive(Debug, Clone, Serialize)]
pub struct PendingRestore {
    pub room_id: String,
    pub snapshot_id: String,
    pub boundary: String,
    pub slot: u8,
}

/// All indexed restore transactions.
pub fn pending_all(config: &RoomsConfig) -> anyhow::Result<Vec<PendingRestore>> {
    Ok(read_intents(config)?
        .iter()
        .map(|intent| PendingRestore {
            room_id: intent.room_id.clone(),
            snapshot_id: intent.snapshot_id.clone(),
            boundary: match intent.boundary {
                Boundary::PreSpawn => "pre_spawn".to_owned(),
                Boundary::Spawned => "spawned".to_owned(),
                Boundary::Leased => "leased".to_owned(),
            },
            slot: intent.slot_index,
        })
        .collect())
}

/// One reconciled (or failed) restore intent from a GC sweep.
#[derive(Debug, Clone)]
pub struct ReconciledRestore {
    pub room_id: String,
    pub reaped: bool,
    pub reason: String,
}

/// GC sweep over indexed restore transactions: finish the teardown (and lease
/// return) for every conclusively dead restore; skip live and unprovable
/// ones. `only` scopes the sweep to a single room id.
pub fn gc_reconcile(config: &RoomsConfig, only: Option<&str>) -> Vec<ReconciledRestore> {
    let intents = match read_intents(config) {
        Ok(intents) => intents,
        Err(e) => {
            warn!(error = %e, "cannot read restore intents; skipping restore reconcile");
            return Vec::new();
        }
    };
    let mut reconciled = Vec::new();
    for intent in intents {
        if only.is_some_and(|only| only != intent.room_id) {
            continue;
        }
        match reconcile_one(config, &intent) {
            Ok(Some(line)) => reconciled.push(ReconciledRestore {
                room_id: intent.room_id.clone(),
                reaped: true,
                reason: line,
            }),
            Ok(None) => {}
            Err(e) => reconciled.push(ReconciledRestore {
                room_id: intent.room_id.clone(),
                reaped: false,
                reason: format!("restore intent reconcile failed (kept for retry): {e}"),
            }),
        }
    }
    reconciled
}

fn reconcile_one(config: &RoomsConfig, intent: &RestoreIntent) -> anyhow::Result<Option<String>> {
    let Some(pid) = intent.pid else {
        // Pre-spawn: no child was ever authorized. The planned dirs may exist
        // (room dir, staged jail) but nothing runs; clean and clear.
        finish_teardown(
            config,
            &intent.room_id,
            &intent.snapshot_id,
            intent.slot_index,
        )?;
        return Ok(Some(format!(
            "restore {}: cleared pre-spawn intent",
            intent.room_id
        )));
    };
    match room::probe(Some(pid), intent.pid_starttime) {
        Liveness::Alive | Liveness::Unknown => Ok(None),
        Liveness::Dead => {
            finish_teardown(
                config,
                &intent.room_id,
                &intent.snapshot_id,
                intent.slot_index,
            )?;
            Ok(Some(format!(
                "restore {}: reaped dead restore, lease returned to snapshot {}",
                intent.room_id, intent.snapshot_id
            )))
        }
    }
}

// --- output-overlap validation ------------------------------------------

/// Refuse an output that overlaps (in either direction) the image, the rooms
/// state base, any jail-instance root, or the snapshot directory itself; a
/// pre-existing non-empty output is refused outright, which also rules out
/// any snapshot artifacts *beneath* it. Ancestors are checked for a published
/// `snapshot.json` so collection can never write inside a snapshot.
fn validate_output_disjoint(
    config: &RoomsConfig,
    out_dir: Option<&Path>,
    image: &Path,
    snapshot_dir: &Path,
) -> anyhow::Result<()> {
    let Some(out) = out_dir else {
        return Ok(());
    };
    let out = canonical_candidate(out)?;
    if out.exists() {
        if !out.is_dir() {
            anyhow::bail!("restore output is not a directory: {}", out.display());
        }
        if std::fs::read_dir(&out)?.next().is_some() {
            anyhow::bail!(
                "refusing non-empty restore output {} (must be empty or absent)",
                out.display()
            );
        }
    }
    let state = state_base(config)?;
    let instances = config
        .chroot_base()
        .ok_or_else(|| anyhow::anyhow!("cannot resolve jailer chroot base"))?
        .join("firecracker");
    for (name, root) in [
        ("backing image", image.to_path_buf()),
        ("rooms state base", canonical_candidate(&state)?),
        ("jail instance root", canonical_candidate(&instances)?),
        ("snapshot directory", snapshot_dir.to_path_buf()),
    ] {
        if overlaps(&out, &root) {
            anyhow::bail!(
                "restore output {} overlaps the {name} {}",
                out.display(),
                root.display()
            );
        }
    }
    for ancestor in out.ancestors().skip(1) {
        if ancestor.join(snapshot::SNAPSHOT_META_FILE).exists() {
            anyhow::bail!(
                "restore output {} lies inside snapshot directory {}",
                out.display(),
                ancestor.display()
            );
        }
    }
    // Reject an output a still-in-flight snapshot transaction is writing to.
    // A completed snapshot dir is caught above (its `snapshot.json`), but a
    // pending one has no metadata yet — its live artifact pair would race
    // restore's collect/clear step. The intent index names those dirs.
    for pending in crate::snapshot_exec::pending_output_dirs(config)? {
        if overlaps(&out, &canonical_candidate(&pending)?) {
            anyhow::bail!(
                "restore output {} overlaps a pending snapshot transaction's output {}",
                out.display(),
                pending.display()
            );
        }
    }
    Ok(())
}

// --- snapshot input validation ------------------------------------------

fn read_snapshot_meta(snapshot_dir: &Path) -> anyhow::Result<SnapshotMeta> {
    for name in [snapshot::SNAPSHOT_VMSTATE_FILE, snapshot::SNAPSHOT_MEM_FILE] {
        let path = snapshot_dir.join(name);
        let meta = std::fs::symlink_metadata(&path)
            .map_err(|e| anyhow::anyhow!("snapshot artifact {}: {e}", path.display()))?;
        if !meta.file_type().is_file() || meta.len() == 0 {
            anyhow::bail!(
                "snapshot artifact missing, empty, or not regular: {}",
                path.display()
            );
        }
    }
    let meta_path = snapshot_dir.join(snapshot::SNAPSHOT_META_FILE);
    let bytes = std::fs::read(&meta_path)
        .map_err(|e| anyhow::anyhow!("snapshot metadata {}: {e}", meta_path.display()))?;
    let meta: SnapshotMeta = serde_json::from_slice(&bytes)?;
    if !crate::registry::is_valid_room_id(&meta.snapshot_id)
        || !crate::registry::is_valid_room_id(&meta.base_room_id)
    {
        anyhow::bail!("snapshot metadata carries an invalid id");
    }
    Ok(meta)
}

// --- intent index mechanics (mirrors snapshot_exec) ----------------------

fn state_base(config: &RoomsConfig) -> anyhow::Result<PathBuf> {
    config
        .resolved_state_base()
        .ok_or_else(|| anyhow::anyhow!("HOME unset; cannot locate rooms state"))
}

fn intent_dir(config: &RoomsConfig) -> anyhow::Result<PathBuf> {
    config
        .restore_intents_dir()
        .ok_or_else(|| anyhow::anyhow!("cannot resolve restore intent directory"))
}

fn intent_path(config: &RoomsConfig, room_id: &str) -> anyhow::Result<PathBuf> {
    Ok(intent_dir(config)?.join(format!("{room_id}.json")))
}

fn create_intent_exclusive(config: &RoomsConfig, intent: &RestoreIntent) -> anyhow::Result<()> {
    let dir = intent_dir(config)?;
    std::fs::create_dir_all(&dir)?;
    let path = intent_path(config, &intent.room_id)?;
    let bytes = serde_json::to_vec_pretty(intent)?;
    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&path)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);
    sync_dir(&dir)?;
    Ok(())
}

fn write_intent_atomic(config: &RoomsConfig, intent: &RestoreIntent) -> anyhow::Result<()> {
    let dir = intent_dir(config)?;
    let tmp = dir.join(format!(".{}.tmp", intent.room_id));
    let bytes = serde_json::to_vec_pretty(intent)?;
    let mut options = std::fs::OpenOptions::new();
    options.create(true).write(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&tmp)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp, intent_path(config, &intent.room_id)?)?;
    sync_dir(&dir)?;
    Ok(())
}

fn finish_index(config: &RoomsConfig, room_id: &str) -> anyhow::Result<()> {
    let path = intent_path(config, room_id)?;
    match std::fs::remove_file(path) {
        Ok(()) => sync_dir(&intent_dir(config)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn read_intents(config: &RoomsConfig) -> anyhow::Result<Vec<RestoreIntent>> {
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
        let bytes = std::fs::read(entry.path())?;
        let intent: RestoreIntent = serde_json::from_slice(&bytes)?;
        if intent.schema_version != INTENT_SCHEMA_VERSION {
            anyhow::bail!(
                "unsupported restore intent schema {}",
                intent.schema_version
            );
        }
        if !crate::registry::is_valid_room_id(&intent.room_id)
            || !crate::registry::is_valid_room_id(&intent.snapshot_id)
        {
            anyhow::bail!("restore intent contains an invalid id");
        }
        intents.push(intent);
    }
    intents.sort_by(|left, right| left.room_id.cmp(&right.room_id));
    Ok(intents)
}

#[cfg(unix)]
fn sync_dir(path: &Path) -> anyhow::Result<()> {
    std::fs::File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
#[allow(
    clippy::unnecessary_wraps,
    reason = "keeps durable transaction call sites identical across cfg targets"
)]
fn sync_dir(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::expect_used,
        clippy::unwrap_used,
        clippy::indexing_slicing,
        reason = "test module"
    )]

    use super::{
        create_intent_exclusive, finish_index, pending_all, read_intents, validate_output_disjoint,
        write_intent_atomic, Boundary, RestoreIntent, INTENT_SCHEMA_VERSION,
    };
    use crate::config::RoomsConfig;
    use std::path::Path;

    const ROOM_ID: &str = "00000000000000000000000001";
    const SNAP_ID: &str = "00000000000000000000000002";

    fn config(base: &Path) -> RoomsConfig {
        RoomsConfig {
            state_base: Some(base.to_path_buf()),
            ..RoomsConfig::default()
        }
    }

    fn intent() -> RestoreIntent {
        RestoreIntent {
            schema_version: INTENT_SCHEMA_VERSION,
            room_id: ROOM_ID.to_owned(),
            snapshot_id: SNAP_ID.to_owned(),
            slot_index: 3,
            tap: "tap-fc3".to_owned(),
            snapshot_dir: "/tmp/snap".into(),
            image: "/tmp/image.ext4".into(),
            pid: None,
            pid_starttime: None,
            boundary: Boundary::PreSpawn,
        }
    }

    #[test]
    fn intent_round_trips_and_is_exclusive() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        create_intent_exclusive(&config, &intent()).expect("first create");
        let dup = create_intent_exclusive(&config, &intent());
        assert!(dup.is_err(), "a second intent for the room must be refused");
        let read = read_intents(&config).expect("read");
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].room_id, ROOM_ID);
        assert_eq!(read[0].boundary, Boundary::PreSpawn);
    }

    #[test]
    fn boundary_advance_and_finish_clear_the_index() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        create_intent_exclusive(&config, &intent()).expect("create");
        let mut advanced = intent();
        advanced.pid = Some(42);
        advanced.boundary = Boundary::Spawned;
        write_intent_atomic(&config, &advanced).expect("advance");
        let read = read_intents(&config).expect("read");
        assert_eq!(read[0].boundary, Boundary::Spawned);
        assert_eq!(read[0].pid, Some(42));
        finish_index(&config, ROOM_ID).expect("finish");
        assert!(read_intents(&config).expect("read").is_empty());
        // Idempotent: a second finish is a no-op.
        finish_index(&config, ROOM_ID).expect("finish again");
    }

    #[test]
    fn pending_all_summarizes_boundaries() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let mut leased = intent();
        leased.boundary = Boundary::Leased;
        create_intent_exclusive(&config, &leased).expect("create");
        let pending = pending_all(&config).expect("pending");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].boundary, "leased");
        assert_eq!(pending[0].slot, 3);
    }

    #[test]
    fn output_inside_state_base_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let out = dir.path().join("rooms-out");
        let image = dir.path().join("image.ext4");
        std::fs::write(&image, b"x").unwrap();
        let snap = dir.path().join("snap");
        std::fs::create_dir_all(&snap).unwrap();
        let err = validate_output_disjoint(&config, Some(&out), &image, &snap)
            .expect_err("state-base overlap must be refused");
        assert!(err.to_string().contains("state base"), "{err}");
    }

    #[test]
    fn output_overlapping_the_snapshot_or_image_is_refused() {
        let state = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let config = config(state.path());
        let image = elsewhere.path().join("image.ext4");
        std::fs::write(&image, b"x").unwrap();
        let snap = elsewhere.path().join("snap");
        std::fs::create_dir_all(&snap).unwrap();
        // The production caller hands canonicalized paths; mirror that (on
        // macOS the tempdir itself is behind a /private symlink).
        let image = image.canonicalize().unwrap();
        let snap = snap.canonicalize().unwrap();
        let inside_snap = snap.join("out");
        let err = validate_output_disjoint(&config, Some(&inside_snap), &image, &snap)
            .expect_err("output inside the snapshot dir must be refused");
        assert!(err.to_string().contains("snapshot"), "{err}");
        let containing = elsewhere.path().canonicalize().unwrap();
        let err = validate_output_disjoint(&config, Some(&containing), &image, &snap)
            .expect_err("output containing the snapshot dir must be refused");
        assert!(err.to_string().contains("non-empty"), "{err}");
    }

    #[test]
    fn nonexistent_disjoint_output_is_accepted() {
        let state = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let config = config(state.path());
        let image = elsewhere.path().join("image.ext4");
        std::fs::write(&image, b"x").unwrap();
        let snap = elsewhere.path().join("snap");
        std::fs::create_dir_all(&snap).unwrap();
        let out = elsewhere.path().join("collect").join("here");
        validate_output_disjoint(&config, Some(&out), &image, &snap)
            .expect("a disjoint absent output is fine");
    }
}
