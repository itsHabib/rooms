//! `rooms` — disposable Firecracker microVMs with specified deps.

pub mod artifacts;
pub mod clonenet;
pub mod config;
pub mod doctor;
pub mod egress;
pub mod egress_audit;
pub mod error;
pub mod firecracker;
mod indexed_claim;
pub mod isolation;
pub mod lifecycle;
pub mod preflight;
pub mod registry;
pub mod restore;
pub mod restore_exec;
pub mod room;
pub mod rootfs;
pub mod runner;
pub mod slot;
pub mod snapshot;
pub mod snapshot_exec;
pub mod transport;
pub mod veth_isolation;
pub mod vsock;
pub mod witness;

pub use config::RoomsConfig;
pub use error::RoomsError;
