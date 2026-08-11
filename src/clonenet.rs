//! Per-clone host-network identity allocation.
//!
//! Every clone reuses its snapshot's frozen guest `/30` inside a private
//! network namespace. This allocator supplies the independent host-side axis:
//! one namespace and one veth `/30` from `172.17.0.0/24` per live clone. Claim
//! files share the slot allocator's crash-safe indexed-claim engine; allocation
//! of the namespace and veth mechanism is layered on that durable ownership.
//!
//! The `none` egress posture must also install an INPUT drop for the host-side
//! veth. This module owns the veth identity; restore integration installs that
//! posture before resume in the follow-on clone-path task.

use std::net::Ipv4Addr;
use std::path::Path;

use crate::error::CloneNetError;
pub use crate::indexed_claim::Claimer;
use crate::indexed_claim::{self, ClaimOutcome, FreeOutcome, Pool, ReconcileAction};

pub const CLONENETS_DIR: &str = "clonenets";
const FREE_LOCK: &str = "clonenets.lock";
pub const MAX_CLONENET: u8 = 63;
pub const DEFAULT_MAX_POOL: u8 = 8;

const CLONENET_POOL: Pool = Pool {
    dir_name: CLONENETS_DIR,
    lock_name: FREE_LOCK,
    max_index: MAX_CLONENET,
    label: "clone network",
};

/// One clone's distinct host-side namespace and veth identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloneNet {
    pub index: u8,
    pub netns: String,
    pub veth_host: String,
    pub veth_guest: String,
    pub host_ip: Ipv4Addr,
    pub netns_ip: Ipv4Addr,
    pub prefix: u8,
}

impl CloneNet {
    /// Derive the host identity for one claimable `/30` index.
    pub fn derive(index: u8) -> Result<Self, CloneNetError> {
        if !indexed_claim::valid_index(CLONENET_POOL, index) {
            return Err(CloneNetError::InvalidIndex {
                index,
                max: MAX_CLONENET,
            });
        }
        let base = 4 * index;
        Ok(Self {
            index,
            netns: format!("rooms-c{index}"),
            veth_host: format!("veth-h{index}"),
            veth_guest: format!("veth-g{index}"),
            host_ip: Ipv4Addr::new(172, 17, 0, base + 1),
            netns_ip: Ipv4Addr::new(172, 17, 0, base + 2),
            prefix: 30,
        })
    }
}

/// Claim one host-network identity without creating its Linux devices yet.
///
/// Durable ownership commits before lifecycle commands run, so a crash can be
/// reconciled from the claim file even when namespace creation was partial.
pub fn claim(
    state: &Path,
    owner_id: &str,
    me: Claimer,
    cap: u8,
    target: Option<u8>,
) -> Result<CloneNet, CloneNetError> {
    match indexed_claim::claim(state, CLONENET_POOL, owner_id, me, cap, target)? {
        ClaimOutcome::Claimed(index) => CloneNet::derive(index),
        ClaimOutcome::PoolFull { cap } => Err(CloneNetError::PoolFull { cap }),
        ClaimOutcome::InvalidIndex { index, max } => {
            Err(CloneNetError::InvalidIndex { index, max })
        }
        ClaimOutcome::TargetTaken { index } => Err(CloneNetError::TargetTaken { index }),
    }
}

/// Result of releasing a clone-network claim by exact owner identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freed {
    Removed,
    AlreadyFree,
    AlreadyReassigned,
}

/// One confirmed-dead clone-network claim removed by [`reconcile`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reclaimed {
    pub index: u8,
    pub owner_id: String,
}

/// Release a pure claim by compare-and-delete.
pub fn release(state: &Path, index: u8, expected_owner: &str) -> Result<Freed, CloneNetError> {
    if !indexed_claim::valid_index(CLONENET_POOL, index) {
        return Err(CloneNetError::InvalidIndex {
            index,
            max: MAX_CLONENET,
        });
    }
    match indexed_claim::free(state, CLONENET_POOL, index, expected_owner)? {
        FreeOutcome::Removed => Ok(Freed::Removed),
        FreeOutcome::AlreadyFree => Ok(Freed::AlreadyFree),
        FreeOutcome::AlreadyReassigned => Ok(Freed::AlreadyReassigned),
    }
}

/// Remove confirmed-dead claims through the shared indexed reconciler.
///
/// Linux resource cleanup is layered onto the same reconciliation path by the
/// lifecycle mechanism; this pure core still guarantees abandoned claims do
/// not wedge the allocation pool.
#[must_use]
pub fn reconcile(state: &Path) -> Vec<Reclaimed> {
    indexed_claim::reconcile(state, CLONENET_POOL, |_, _| Ok(ReconcileAction::Remove))
        .into_iter()
        .map(|entry| Reclaimed {
            index: entry.index,
            owner_id: entry.owner_id,
        })
        .collect()
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

    use std::collections::HashSet;
    use std::net::Ipv4Addr;

    use super::{
        claim, release, Claimer, CloneNet, CloneNetError, Freed, CLONENETS_DIR, MAX_CLONENET,
    };
    #[cfg(target_os = "linux")]
    use super::{reconcile, Reclaimed};

    const ME: Claimer = Claimer {
        pid: 1,
        starttime: 1,
    };

    fn owner_id(value: u32) -> String {
        format!("{value:026}")
    }

    #[test]
    fn derive_uses_the_disjoint_veth_carve_and_short_names() {
        let first = CloneNet::derive(1).unwrap();
        assert_eq!(first.netns, "rooms-c1");
        assert_eq!(first.veth_host, "veth-h1");
        assert_eq!(first.veth_guest, "veth-g1");
        assert_eq!(first.host_ip, Ipv4Addr::new(172, 17, 0, 5));
        assert_eq!(first.netns_ip, Ipv4Addr::new(172, 17, 0, 6));
        assert_eq!(first.prefix, 30);
        assert!(first.veth_host.len() <= 15);
        assert!(first.veth_guest.len() <= 15);

        let top = CloneNet::derive(MAX_CLONENET).unwrap();
        assert_eq!(top.host_ip, Ipv4Addr::new(172, 17, 0, 253));
        assert_eq!(top.netns_ip, Ipv4Addr::new(172, 17, 0, 254));
        assert!(top.veth_host.len() <= 15);
        assert!(top.veth_guest.len() <= 15);
    }

    #[test]
    fn every_derived_identity_is_disjoint() {
        let mut namespaces = HashSet::new();
        let mut interfaces = HashSet::new();
        let mut addresses = HashSet::new();
        for index in 1..=MAX_CLONENET {
            let net = CloneNet::derive(index).unwrap();
            assert!(namespaces.insert(net.netns));
            assert!(interfaces.insert(net.veth_host));
            assert!(interfaces.insert(net.veth_guest));
            assert!(addresses.insert(net.host_ip));
            assert!(addresses.insert(net.netns_ip));
        }
    }

    #[test]
    fn invalid_indices_reject_before_touching_state() {
        let state = tempfile::tempdir().unwrap();
        for index in [0, MAX_CLONENET + 1, u8::MAX] {
            assert!(matches!(
                claim(state.path(), &owner_id(1), ME, 8, Some(index)),
                Err(CloneNetError::InvalidIndex { index: got, .. }) if got == index
            ));
        }
        assert!(!state.path().join(CLONENETS_DIR).exists());
    }

    #[test]
    fn claims_are_independent_and_refill_the_lowest_hole() {
        let state = tempfile::tempdir().unwrap();
        let first = claim(state.path(), &owner_id(1), ME, 8, None).unwrap();
        let second = claim(state.path(), &owner_id(2), ME, 8, None).unwrap();
        let third = claim(state.path(), &owner_id(3), ME, 8, None).unwrap();
        assert_eq!((first.index, second.index, third.index), (1, 2, 3));
        assert_eq!(
            release(state.path(), second.index, &owner_id(2)).unwrap(),
            Freed::Removed
        );
        let refill = claim(state.path(), &owner_id(4), ME, 8, None).unwrap();
        assert_eq!(refill.index, 2);
    }

    #[test]
    fn an_existing_file_is_reservation_opaque_to_the_walk() {
        let state = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(state.path().join(CLONENETS_DIR)).unwrap();
        std::fs::write(state.path().join(CLONENETS_DIR).join("1"), "@reserved\n").unwrap();
        let allocation = claim(state.path(), &owner_id(1), ME, 8, None).unwrap();
        assert_eq!(allocation.index, 2);
    }

    #[test]
    fn a_full_pool_and_taken_target_are_typed() {
        let state = tempfile::tempdir().unwrap();
        claim(state.path(), &owner_id(1), ME, 1, None).unwrap();
        assert!(matches!(
            claim(state.path(), &owner_id(2), ME, 1, None),
            Err(CloneNetError::PoolFull { cap: 1 })
        ));
        assert!(matches!(
            claim(state.path(), &owner_id(2), ME, 8, Some(1)),
            Err(CloneNetError::TargetTaken { index: 1 })
        ));
    }

    #[test]
    fn stale_release_never_removes_a_reassigned_claim() {
        let state = tempfile::tempdir().unwrap();
        claim(state.path(), &owner_id(1), ME, 8, None).unwrap();
        assert_eq!(
            release(state.path(), 1, &owner_id(2)).unwrap(),
            Freed::AlreadyReassigned
        );
        assert!(state.path().join(CLONENETS_DIR).join("1").exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn reconcile_removes_a_confirmed_dead_claim() {
        let state = tempfile::tempdir().unwrap();
        let owner = owner_id(9);
        let dead = Claimer {
            pid: u32::MAX,
            starttime: 1,
        };
        claim(state.path(), &owner, dead, 8, None).unwrap();

        assert_eq!(
            reconcile(state.path()),
            vec![Reclaimed {
                index: 1,
                owner_id: owner,
            }]
        );
        assert!(!state.path().join(CLONENETS_DIR).join("1").exists());
    }
}
