//! Snapshot policy — pause a neutral base and describe the Full snapshot to take.
//!
//! Policy over `transport` (Cheney: policy vs mechanism). This module owns the
//! *decisions* — refuse a non-neutral or vsock-active guest, order the Firecracker
//! API calls, and shape the `snapshot.json` metadata — as pure, testable data. The
//! actual API round-trips (the `execute` mechanism) live below it and run only on a
//! host with a live Firecracker; here we produce a [`SnapshotPlan`] the runner
//! carries out, so every correctness-critical choice is unit-testable without a VM.

use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::room::{Provenance, RoomMeta};

/// Schema version for `snapshot.json` (forward-compat, mirrors `room.json`).
pub const SNAPSHOT_SCHEMA_VERSION: u32 = 1;

/// Metadata file name written beside the snapshot pair in the room state dir.
pub const SNAPSHOT_META_FILE: &str = "snapshot.json";

/// vmstate file name — device + vCPU state (`PUT /snapshot/create` `snapshot_path`).
pub const SNAPSHOT_VMSTATE_FILE: &str = "snapshot.vmstate";

/// Guest-memory file name (`mem_file_path`) — plaintext guest RAM. Treat as a
/// credential even though a neutral base holds no secret.
pub const SNAPSHOT_MEM_FILE: &str = "snapshot.mem";

/// Why a snapshot was refused before any Firecracker call. Both are fail-closed
/// security preconditions, not transient errors — nothing is written on either.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SnapshotError {
    /// The base is not a sealed neutral guest, so its memory file could bake a
    /// secret into every clone. Carries the offending provenance (`None` = not a
    /// base at all).
    #[error("refusing to snapshot a non-neutral room (provenance: {found:?}); only a sealed neutral base may be frozen")]
    NotNeutral { found: Option<Provenance> },
    /// A vsock connection is still open. Firecracker severs active vsock
    /// connections across a snapshot (transport reset on resume), which would
    /// wedge the resume-apply agent — the quiesced beacon must have closed first.
    #[error(
        "refusing to snapshot with an active vsock connection; it would be severed on restore"
    )]
    ActiveVsock,
}

/// One ordered Firecracker API operation a snapshot performs. Pure data — the
/// host-side runner executes the sequence; tests assert its exact order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FcOp {
    /// `PATCH /vm {state: "Paused"}` — must precede the create.
    PauseVm,
    /// `PUT /snapshot/create {snapshot_type: "Full", snapshot_path, mem_file_path}`.
    CreateFullSnapshot {
        snapshot_path: PathBuf,
        mem_file_path: PathBuf,
    },
}

/// The persisted snapshot descriptor (`snapshot.json`).
///
/// Drives restore compatibility (`fc_version` + `rootfs_hash`, the FR5 guard) and
/// the checkpoint receipt. `provenance` is always `Neutral` — [`plan`] refuses any
/// other, so a written snapshot is provably of a sealed neutral guest.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnapshotMeta {
    pub schema_version: u32,
    /// Lowercase ULID identifying this snapshot.
    pub snapshot_id: String,
    pub created_at: DateTime<Utc>,
    /// `firecracker --version` on the creating host — the restore compat key.
    pub fc_version: String,
    /// Hash of the mounted RO base — the other half of the restore compat key.
    pub rootfs_hash: String,
    /// The room this snapshot was frozen from.
    pub base_room_id: String,
    /// The base's pool slot; the frozen guest IP a same-slot restore reclaims.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot_index: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guest_ip: Option<Ipv4Addr>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_repo_sha: Option<String>,
    /// Always `Neutral` (the refusal guard guarantees it) — recorded so a reader
    /// can confirm the snapshot's lineage without re-deriving it.
    pub provenance: Provenance,
}

/// The host-gathered facts a snapshot needs that aren't on the base's [`RoomMeta`].
///
/// The caller collects these on the host (where Firecracker runs) and hands them
/// to [`plan`], keeping the policy pure: `fc_version` from `firecracker --version`,
/// `rootfs_hash` of the mounted base, `active_vsock` from the live device state.
#[derive(Debug, Clone)]
pub struct SnapshotRequest {
    /// Directory the snapshot pair + metadata are written into (the room state dir).
    pub out_dir: PathBuf,
    /// Lowercase ULID for this snapshot.
    pub snapshot_id: String,
    pub fc_version: String,
    pub rootfs_hash: String,
    pub base_repo_sha: Option<String>,
    /// Whether any vsock connection is currently open on the base (the no-active-
    /// vsock precondition).
    pub active_vsock: bool,
}

/// A fully-decided snapshot: the ordered API operations plus the metadata to write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotPlan {
    /// The Firecracker API calls, in the order the runner must issue them.
    pub ops: Vec<FcOp>,
    pub meta: SnapshotMeta,
    /// Where `meta` is written (`<out_dir>/snapshot.json`).
    pub meta_path: PathBuf,
}

/// Decide the snapshot for `base`: refuse a non-neutral or vsock-active guest,
/// otherwise emit the pause→create sequence and the `snapshot.json` metadata.
///
/// Pure policy — no I/O, no Firecracker. `now` is injected so the metadata's
/// timestamp is testable.
///
/// # Errors
/// - [`SnapshotError::NotNeutral`] if `base` is not a sealed neutral base.
/// - [`SnapshotError::ActiveVsock`] if a vsock connection is still open.
pub fn plan(
    base: &RoomMeta,
    req: SnapshotRequest,
    now: DateTime<Utc>,
) -> Result<SnapshotPlan, SnapshotError> {
    if !base.is_snapshottable() {
        return Err(SnapshotError::NotNeutral {
            found: base.provenance(),
        });
    }
    if req.active_vsock {
        return Err(SnapshotError::ActiveVsock);
    }

    let snapshot_path = req.out_dir.join(SNAPSHOT_VMSTATE_FILE);
    let mem_file_path = req.out_dir.join(SNAPSHOT_MEM_FILE);
    let ops = vec![
        FcOp::PauseVm,
        FcOp::CreateFullSnapshot {
            snapshot_path,
            mem_file_path,
        },
    ];

    let (slot_index, guest_ip) = base
        .slot
        .as_ref()
        .map_or((None, None), |s| (Some(s.index), Some(s.guest)));
    let meta = SnapshotMeta {
        schema_version: SNAPSHOT_SCHEMA_VERSION,
        snapshot_id: req.snapshot_id,
        created_at: now,
        fc_version: req.fc_version,
        rootfs_hash: req.rootfs_hash,
        base_room_id: base.id.clone(),
        slot_index,
        guest_ip,
        base_repo_sha: req.base_repo_sha,
        provenance: Provenance::Neutral,
    };
    let meta_path = req.out_dir.join(SNAPSHOT_META_FILE);
    Ok(SnapshotPlan {
        ops,
        meta,
        meta_path,
    })
}

/// Write `meta` to `path` atomically (temp-then-rename), mirroring
/// [`crate::room::write_atomic`]. The runner calls this after the pair is created.
///
/// # Errors
/// Propagates any serialize or filesystem error.
pub fn write_meta_atomic(path: &Path, meta: &SnapshotMeta) -> std::io::Result<()> {
    let json = serde_json::to_vec_pretty(meta)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let Some(dir) = path.parent() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "snapshot meta path has no parent dir",
        ));
    };
    let tmp = dir.join(".snapshot.json.tmp");
    std::fs::write(&tmp, &json)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, reason = "test module")]

    use super::{
        plan, write_meta_atomic, FcOp, SnapshotError, SnapshotMeta, SnapshotRequest,
        SNAPSHOT_SCHEMA_VERSION,
    };
    use crate::room::{Provenance, RoomMeta, Slot};
    use chrono::Utc;
    use std::net::Ipv4Addr;
    use std::path::PathBuf;

    fn slot(index: u8) -> Slot {
        Slot {
            index,
            tap: format!("tap-fc{index}"),
            gateway: Ipv4Addr::new(172, 16, 0, 4 * index + 1),
            guest: Ipv4Addr::new(172, 16, 0, 4 * index + 2),
            prefix: 30,
        }
    }

    // A sealed neutral base with a claimed slot — the only snapshottable shape.
    fn neutral_base(index: u8) -> RoomMeta {
        let mut m = RoomMeta::new_base(
            "01abcdefghijklmnopqrstuvwx".to_owned(),
            Some("base".to_owned()),
            Some(42),
            Some(42),
            true,
            Utc::now(),
        );
        m.slot = Some(slot(index));
        m.seal().expect("seal the provisioning base");
        m
    }

    fn req(dir: &str) -> SnapshotRequest {
        SnapshotRequest {
            out_dir: PathBuf::from(dir),
            snapshot_id: "01snapshotidsnapshotidsnap".to_owned(),
            fc_version: "1.9.0".to_owned(),
            rootfs_hash: "sha256:abc".to_owned(),
            base_repo_sha: Some("deadbeef".to_owned()),
            active_vsock: false,
        }
    }

    #[test]
    fn plan_neutral_base_emits_pause_then_create_in_order() {
        let base = neutral_base(3);
        let p = plan(&base, req("/state/room"), Utc::now()).expect("neutral base plans");
        assert_eq!(
            p.ops,
            vec![
                FcOp::PauseVm,
                FcOp::CreateFullSnapshot {
                    snapshot_path: PathBuf::from("/state/room/snapshot.vmstate"),
                    mem_file_path: PathBuf::from("/state/room/snapshot.mem"),
                },
            ],
            "pause must precede create, and both must be present in that order"
        );
        assert_eq!(p.meta_path, PathBuf::from("/state/room/snapshot.json"));
    }

    #[test]
    fn plan_metadata_is_fully_populated_and_neutral() {
        let base = neutral_base(5);
        let now = Utc::now();
        let p = plan(&base, req("/state/room"), now).expect("plans");
        let m = &p.meta;
        assert_eq!(m.schema_version, SNAPSHOT_SCHEMA_VERSION);
        assert_eq!(m.snapshot_id, "01snapshotidsnapshotidsnap");
        assert_eq!(m.created_at, now);
        assert_eq!(m.fc_version, "1.9.0");
        assert_eq!(m.rootfs_hash, "sha256:abc");
        assert_eq!(m.base_room_id, "01abcdefghijklmnopqrstuvwx");
        assert_eq!(m.slot_index, Some(5));
        assert_eq!(m.guest_ip, Some(Ipv4Addr::new(172, 16, 0, 22)));
        assert_eq!(m.base_repo_sha.as_deref(), Some("deadbeef"));
        assert_eq!(
            m.provenance,
            Provenance::Neutral,
            "a written snapshot is provably of a neutral base"
        );
    }

    #[test]
    fn plan_refuses_every_non_neutral_provenance() {
        // Plain workload room (no provenance).
        let mut plain = RoomMeta::new(
            "01plainroomplainroomplain0".to_owned(),
            None,
            Some(1),
            Some(1),
            false,
            Utc::now(),
        );
        plain.slot = Some(slot(1));
        assert_eq!(
            plan(&plain, req("/x"), Utc::now()),
            Err(SnapshotError::NotNeutral { found: None }),
        );

        // A provisioning (not-yet-sealed) base.
        let mut provisioning = RoomMeta::new_base(
            "01provisioningprovisionin0".to_owned(),
            None,
            Some(2),
            Some(2),
            true,
            Utc::now(),
        );
        provisioning.slot = Some(slot(2));
        assert_eq!(
            plan(&provisioning, req("/x"), Utc::now()),
            Err(SnapshotError::NotNeutral {
                found: Some(Provenance::Provisioning)
            }),
        );

        // A tainted base.
        let mut tainted = neutral_base(4);
        tainted.taint();
        assert_eq!(
            plan(&tainted, req("/x"), Utc::now()),
            Err(SnapshotError::NotNeutral {
                found: Some(Provenance::Tainted)
            }),
        );
    }

    #[test]
    fn plan_refuses_an_active_vsock_connection() {
        let base = neutral_base(1);
        let mut r = req("/x");
        r.active_vsock = true;
        assert_eq!(plan(&base, r, Utc::now()), Err(SnapshotError::ActiveVsock));
    }

    #[test]
    fn refusal_writes_nothing_and_names_the_state() {
        // The error carries the offending provenance so the operator sees *why*.
        let mut tainted = neutral_base(1);
        tainted.taint();
        let err = plan(&tainted, req("/x"), Utc::now()).unwrap_err();
        assert!(err.to_string().contains("non-neutral"));
    }

    #[test]
    fn snapshot_meta_round_trips_through_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = neutral_base(2);
        let p = plan(&base, req(dir.path().to_str().unwrap()), Utc::now()).expect("plans");
        write_meta_atomic(&p.meta_path, &p.meta).expect("write meta");
        let bytes = std::fs::read(&p.meta_path).expect("read");
        let back: SnapshotMeta = serde_json::from_slice(&bytes).expect("deserialize");
        assert_eq!(back, p.meta);
        assert!(
            !dir.path().join(".snapshot.json.tmp").exists(),
            "atomic write leaves no temp"
        );
    }

    #[test]
    fn meta_serializes_provenance_lowercase() {
        let base = neutral_base(1);
        let p = plan(&base, req("/x"), Utc::now()).expect("plans");
        let json = serde_json::to_string(&p.meta).expect("serialize");
        assert!(
            json.contains("\"provenance\":\"neutral\""),
            "provenance is a lowercase tag, got {json}"
        );
    }
}
