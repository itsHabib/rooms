//! `rooms` — disposable Firecracker microVMs with specified deps.
//!
//! The substrate that spawns a clean microVM, runs a command in it,
//! collects artifacts, and tears it down. The CLI is a thin shell over
//! these modules; consumers (eventually ship's `RoomCursorRunner`) call
//! the same surfaces directly.
//!
//! See `docs/features/rooms-v0/spec.md`.

pub mod artifacts;
pub mod firecracker;
pub mod runner;
