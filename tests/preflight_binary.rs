//! Integration guard: the real `rooms doctor --json` output round-trips through
//! the preflight gate.
//!
//! Runs anywhere — doctor emits its report regardless of how the host is
//! provisioned — so it needs no rooms-host, and it catches any Serialize /
//! Deserialize drift on the doctor report that would silently break the gate
//! (the report is `Serialize` in the binary, `Deserialize` in the gate; nothing
//! but this test pins the two together against the actual CLI output).

#![allow(clippy::expect_used, reason = "integration test module")]

use std::path::Path;

use rooms::preflight;

#[test]
fn real_doctor_json_round_trips_through_the_gate() {
    let bin = Path::new(env!("CARGO_BIN_EXE_rooms"));
    // On an unprovisioned CI host the gate will report failures; the point is
    // that the binary's real `--json` deserialized cleanly (no schema drift), so
    // the Preflight decision is well-formed either way.
    let preflight = preflight::run(bin, None)
        .expect("rooms doctor --json must round-trip through the preflight gate");
    // Exercise the decision path so the round-tripped report is actually used.
    let _ = preflight.passed();
}
