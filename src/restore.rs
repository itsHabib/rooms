//! Restore policy — the disjoint "load before config" Firecracker flow as data.
//!
//! Restore is **not** boot (Cheney: policy vs mechanism keeps the two flows from
//! bleeding together). `PUT /snapshot/load` runs *before any other config*, then
//! the VM resumes — no boot-source, no drive PUTs. This module owns the two
//! correctness-critical decisions as pure, testable data: the FR5 compatibility
//! guard (refuse a snapshot the host can't load) and the operation ordering
//! (custody installed *before* the guest resumes; load *before* resume). The async
//! API round-trips and slot reclaim (the execution mechanism) run only on a host
//! with a live Firecracker and are exercised at the phase intermediate gate.

use std::path::PathBuf;

use thiserror::Error;

use crate::snapshot::{SnapshotMeta, SNAPSHOT_MEM_FILE, SNAPSHOT_VMSTATE_FILE};

/// Which pinned field made a snapshot incompatible with the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompatField {
    /// `firecracker --version` differs — the snapshot format is version-pinned.
    FcVersion,
    /// The mounted RO base hash differs — the guest's disk view would not match.
    RootfsHash,
}

impl CompatField {
    const fn as_str(self) -> &'static str {
        match self {
            Self::FcVersion => "fc_version",
            Self::RootfsHash => "rootfs_hash",
        }
    }
}

/// A restore refused before any Firecracker call — fail-closed, never best-effort.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum RestoreError {
    /// The snapshot's pinned `field` does not match the host, so loading it would
    /// fault or silently corrupt. Refuse; the remedy is to re-snapshot on this host.
    #[error("snapshot incompatible with host: {} mismatch; re-snapshot on this host", .field.as_str())]
    SnapshotIncompatible { field: CompatField },
}

/// One ordered restore operation. Pure data — the host-side runner executes the
/// sequence; tests assert the ordering invariants the security model depends on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreOp {
    /// Install the per-clone witness pcap + egress chain. **Before** the guest
    /// resumes: a restored guest has network up instantly with no boot delay, so
    /// custody must exist before it can transmit (FR7).
    InstallCustody,
    /// `PUT /snapshot/load {snapshot_path, mem_backend: {backend_type: "File",
    /// backend_path}}`. Runs **before any other config** — this is what makes
    /// restore a distinct flow from boot. Paths are jail-visible (chroot-rooted).
    LoadSnapshot {
        snapshot_path: PathBuf,
        mem_backend_path: PathBuf,
    },
    /// `PATCH /vm {state: "Resumed"}` — the guest comes live here.
    ResumeVm,
    /// The resume-apply agent applies the hygiene nudge (reseed RNG, step clock,
    /// fresh-key sshd, identity) and ACKs; the host gates the workload on that ack.
    ApplyHygieneNudgeAndAwaitAck,
}

/// A fully-decided restore: the ordered operations for a compatible snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestorePlan {
    /// The operations, in the order the runner must issue them.
    pub ops: Vec<RestoreOp>,
}

impl RestorePlan {
    /// Index of the first `LoadSnapshot`, if any — for ordering assertions.
    #[must_use]
    pub fn load_index(&self) -> Option<usize> {
        self.ops
            .iter()
            .position(|op| matches!(op, RestoreOp::LoadSnapshot { .. }))
    }

    /// Index of `ResumeVm`, if any.
    #[must_use]
    pub fn resume_index(&self) -> Option<usize> {
        self.ops.iter().position(|op| *op == RestoreOp::ResumeVm)
    }

    /// Index of `InstallCustody`, if any.
    #[must_use]
    pub fn custody_index(&self) -> Option<usize> {
        self.ops
            .iter()
            .position(|op| *op == RestoreOp::InstallCustody)
    }
}

/// Compare a snapshot's pinned fields to the host — the FR5 guard.
///
/// Fail closed: any mismatch is a hard refusal, never a best-effort load.
///
/// # Errors
/// [`RestoreError::SnapshotIncompatible`] naming the first field that differs
/// (`fc_version` before `rootfs_hash`).
pub fn check_compat(
    snap: &SnapshotMeta,
    host_fc_version: &str,
    host_rootfs_hash: &str,
) -> Result<(), RestoreError> {
    if snap.fc_version != host_fc_version {
        return Err(RestoreError::SnapshotIncompatible {
            field: CompatField::FcVersion,
        });
    }
    if snap.rootfs_hash != host_rootfs_hash {
        return Err(RestoreError::SnapshotIncompatible {
            field: CompatField::RootfsHash,
        });
    }
    Ok(())
}

/// Decide the restore for `snap`: run the compat guard, then emit the ordered
/// operation sequence. Pure policy — no I/O, no Firecracker, no slot reclaim.
///
/// The invariants the sequence encodes (asserted in tests): custody is installed
/// **before** `ResumeVm`, and `LoadSnapshot` precedes `ResumeVm` and is the first
/// configuration operation.
///
/// # Errors
/// [`RestoreError::SnapshotIncompatible`] if the compat guard fails — no ops are
/// emitted, nothing is loaded.
pub fn plan_restore(
    snap: &SnapshotMeta,
    host_fc_version: &str,
    host_rootfs_hash: &str,
) -> Result<RestorePlan, RestoreError> {
    check_compat(snap, host_fc_version, host_rootfs_hash)?;
    let ops = vec![
        RestoreOp::InstallCustody,
        RestoreOp::LoadSnapshot {
            snapshot_path: PathBuf::from(format!("/{SNAPSHOT_VMSTATE_FILE}")),
            mem_backend_path: PathBuf::from(format!("/{SNAPSHOT_MEM_FILE}")),
        },
        RestoreOp::ResumeVm,
        RestoreOp::ApplyHygieneNudgeAndAwaitAck,
    ];
    Ok(RestorePlan { ops })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, reason = "test module")]

    use super::{check_compat, plan_restore, CompatField, RestoreError, RestoreOp};
    use crate::room::Provenance;
    use crate::snapshot::{SnapshotMeta, SNAPSHOT_SCHEMA_VERSION};
    use chrono::Utc;
    use std::net::Ipv4Addr;
    use std::path::PathBuf;

    fn snap_meta() -> SnapshotMeta {
        SnapshotMeta {
            schema_version: SNAPSHOT_SCHEMA_VERSION,
            snapshot_id: "01snapshotidsnapshotidsnap".to_owned(),
            created_at: Utc::now(),
            fc_version: "1.9.0".to_owned(),
            rootfs_hash: "sha256:abc".to_owned(),
            base_room_id: "01abcdefghijklmnopqrstuvwx".to_owned(),
            slot_index: Some(3),
            guest_ip: Some(Ipv4Addr::new(172, 16, 0, 14)),
            base_repo_sha: Some("deadbeef".to_owned()),
            provenance: Provenance::Neutral,
        }
    }

    #[test]
    fn compat_guard_passes_on_an_exact_match() {
        assert_eq!(check_compat(&snap_meta(), "1.9.0", "sha256:abc"), Ok(()));
    }

    #[test]
    fn compat_guard_fails_closed_on_fc_version_mismatch() {
        assert_eq!(
            check_compat(&snap_meta(), "1.10.0", "sha256:abc"),
            Err(RestoreError::SnapshotIncompatible {
                field: CompatField::FcVersion
            }),
        );
    }

    #[test]
    fn compat_guard_fails_closed_on_rootfs_hash_mismatch() {
        assert_eq!(
            check_compat(&snap_meta(), "1.9.0", "sha256:DIFFERENT"),
            Err(RestoreError::SnapshotIncompatible {
                field: CompatField::RootfsHash
            }),
        );
    }

    #[test]
    fn incompatible_snapshot_emits_no_ops() {
        // Fail closed: a mismatch loads nothing.
        assert!(plan_restore(&snap_meta(), "9.9.9", "sha256:abc").is_err());
    }

    #[test]
    fn restore_installs_custody_and_loads_before_resume() {
        let plan = plan_restore(&snap_meta(), "1.9.0", "sha256:abc").expect("compatible");
        let custody = plan.custody_index().expect("custody op present");
        let load = plan.load_index().expect("load op present");
        let resume = plan.resume_index().expect("resume op present");

        assert!(
            custody < resume,
            "witness/egress must be installed before the guest resumes (FR7)"
        );
        assert!(
            load < resume,
            "the snapshot must be loaded before the vm resumes"
        );
        assert_eq!(
            load, 1,
            "load is the first configuration op (only custody install precedes it)"
        );
    }

    #[test]
    fn restore_emits_the_full_ordered_sequence_with_jail_paths() {
        let plan = plan_restore(&snap_meta(), "1.9.0", "sha256:abc").expect("compatible");
        assert_eq!(
            plan.ops,
            vec![
                RestoreOp::InstallCustody,
                RestoreOp::LoadSnapshot {
                    snapshot_path: PathBuf::from("/snapshot.vmstate"),
                    mem_backend_path: PathBuf::from("/snapshot.mem"),
                },
                RestoreOp::ResumeVm,
                RestoreOp::ApplyHygieneNudgeAndAwaitAck,
            ],
        );
    }

    #[test]
    fn incompatible_error_names_the_offending_field() {
        let err = plan_restore(&snap_meta(), "1.10.0", "sha256:abc").unwrap_err();
        assert!(err.to_string().contains("fc_version"));
    }
}
