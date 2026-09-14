//! Explicit adoption of a verified snapshot into a fresh disposable lab host.
//! The caller transports and seals the files first; normal restore validation
//! hashes them before this helper claims the frozen network slot.
use std::path::PathBuf;

use anyhow::{anyhow, ensure, Result};
use rooms::{restore_exec, slot, RoomsConfig};

#[allow(clippy::print_stdout)] // This example emits its adoption receipt.
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let [directory, image, toolstore] = args.as_slice() else {
        anyhow::bail!("usage: lab-import-snapshot SNAPSHOT IMAGE TOOLSTORE");
    };
    let directory = PathBuf::from(directory);
    let image = PathBuf::from(image);
    let toolstore = PathBuf::from(toolstore);
    let config = RoomsConfig::default();
    let verified = restore_exec::prepare_restore(&config, &directory, &image, Some(&toolstore))?;
    let meta = verified.snapshot_meta();
    let index = meta
        .slot_index
        .ok_or_else(|| anyhow!("snapshot has no slot"))?;
    let state = config
        .resolved_state_base()
        .ok_or_else(|| anyhow!("state base unavailable"))?;
    let identity =
        slot::Claimer::current().ok_or_else(|| anyhow!("process identity unavailable"))?;
    // claim refuses an occupied slot; this helper never overwrites a live owner.
    slot::claim(
        &state,
        &meta.base_room_id,
        identity,
        config.max_pool,
        Some(index),
    )?;
    let reserved = slot::reserve(&state, index, &meta.snapshot_id, &meta.base_room_id)?;
    ensure!(
        matches!(reserved, slot::Reserved::Transferred),
        "snapshot reservation was not transferred"
    );
    println!(
        "{}",
        serde_json::json!({"snapshot_id": meta.snapshot_id, "slot": index, "status": "adopted"})
    );
    Ok(())
}
