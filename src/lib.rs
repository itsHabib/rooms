//! `rooms` — disposable Firecracker microVMs with specified deps.

pub mod artifacts;
pub mod config;
pub mod doctor;
pub mod error;
pub mod firecracker;
pub mod registry;
pub mod room;
pub mod rootfs;
pub mod runner;
pub mod transport;

pub use config::RoomsConfig;
pub use error::RoomsError;
