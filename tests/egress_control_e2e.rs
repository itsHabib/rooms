//! Host-e2e for the egress-control validation harness — the negative test that
//! *proves* the zero-egress wall holds (`--egress none`, #82) rather than merely
//! asserting it. Each test is a no-op skip on an unprovisioned host.
//!
//! It composes the whole custody plane in one run, exactly as the spec sketches:
//! `--secret` (#79) injects a clearly-fake honeytoken where an agent hunts for
//! secrets, a deterministic exfil-probe attempts to send it to a marked RFC-5737
//! endpoint, `--witness` (#77) records what left host-side, and the
//! [`rooms::egress_audit`] scorer renders the verdict from `witness.json` — the
//! same scorer the unit tests exercise, dogfooded here against a real capture.
//!
//! - **`two_condition_gate_holds_for_one_fixture`:** the load-bearing proof. The
//!   demonstrator fixture leaks with egress open (`Observe` → the sentinel
//!   endpoint is *observed leaving*, `blocked` empty) and is contained under
//!   `--egress none` (the endpoint lands in `blocked`). Both ⇒ the wall provably
//!   blocked a real exfil attempt.
//! - **`benign_control_never_exfils`:** the paired control — same shape, no
//!   injection — must never contact the sentinel endpoint under any policy. A
//!   control that shows an escape is a harness bug.
//!
//! Scoring is on the DESTINATION, never guest self-report — the same
//! unforgeable, tap-keyed evidence #82 enforces on. The marked endpoints are
//! RFC-5737 TEST-NET, unroutable, so the "exfil" never reaches a real host.
//!
//! Gated behind `e2e` + unix. Requires the rooms-host: root, `/dev/kvm`,
//! Firecracker + jailer on PATH, guest images under `~/rooms/images`,
//! `~/.ssh/id_rooms`, `tcpdump`, a vsock-capable guest kernel (for `--secret`),
//! and the `ROOMS_FWD` chain installed. Every unmet precondition is a skip.
//!
//! Run on rooms-host:
//! `sudo -E cargo test --features e2e --test egress_control_e2e -- --nocapture`

#![cfg(all(unix, feature = "e2e"))]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "e2e test module: panicky lints are noise in tests"
)]

use std::path::{Path, PathBuf};
use std::process::Command;

use rooms::artifacts::Witness;
use rooms::egress_audit::{self, FixtureManifest, Injection, Target, Verdict};

/// The demonstrator fixture for the two-condition gate: an env-injected,
/// credential-shaped sentinel with an IP endpoint — the clean, unambiguous case.
const DEMONSTRATOR: &str = "readme/aws-key-in-readme";

fn image_path(name: &str) -> PathBuf {
    let home = std::env::var("HOME").expect("HOME");
    PathBuf::from(home).join("rooms/images").join(name)
}

fn guest_key() -> PathBuf {
    let home = std::env::var("HOME").expect("HOME");
    PathBuf::from(home).join(".ssh/id_rooms")
}

fn fixture_dir(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/egress-control")
        .join(rel)
}

fn is_root() -> bool {
    Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .is_some_and(|s| s.trim() == "0")
}

fn rooms_fwd_installed() -> bool {
    Command::new("iptables")
        .args(["-S", "ROOMS_FWD"])
        .output()
        .is_ok_and(|o| o.status.success())
}

fn tcpdump_present() -> bool {
    Command::new("tcpdump")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success() || o.status.code().is_some())
}

/// The rootfs to boot, or a skip reason on stderr. Mirrors the egress-e2e
/// preflight — root, KVM, images, the chain, the guest key, tcpdump (the witness
/// capture the scorer reads needs it).
fn preflight() -> Option<PathBuf> {
    if !is_root() {
        eprintln!("skipping: e2e needs root (tap + iptables chain)");
        return None;
    }
    if !Path::new("/dev/kvm").exists() {
        eprintln!("skipping: no /dev/kvm (nested virt off?)");
        return None;
    }
    let kernel = image_path("vmlinux.bin");
    let rootfs = image_path("rootfs.ext4");
    if !kernel.exists() || !rootfs.exists() {
        eprintln!("skipping: guest images not present under ~/rooms/images");
        return None;
    }
    if !rooms_fwd_installed() {
        eprintln!(
            "skipping: ROOMS_FWD not installed; run `sudo bash scripts/setup-tap.sh --host` first"
        );
        return None;
    }
    if !guest_key().exists() {
        eprintln!("skipping: ~/.ssh/id_rooms missing (bake-rootfs-ssh.sh not run)");
        return None;
    }
    if !tcpdump_present() {
        eprintln!("skipping: tcpdump not on PATH (the witness capture the scorer reads needs it)");
        return None;
    }
    Some(rootfs)
}

/// Load and parse a fixture's manifest.
fn load_manifest(rel: &str) -> FixtureManifest {
    let path = fixture_dir(rel).join("fixture.json");
    let raw =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    FixtureManifest::parse(&raw).expect("fixture.json is a valid manifest")
}

/// Deserialize the room's `witness.json` into the same [`Witness`] the scorer
/// classifies — the host-recorded egress evidence.
fn read_witness(out_dir: &Path) -> Witness {
    let path = out_dir.join("witness.json");
    let raw =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_str(&raw).expect("witness.json deserializes into a Witness")
}

/// Load a fixture's actual probe script text (e.g. `probe.sh` or
/// `control/probe.sh`) to run verbatim as the guest command — so the committed
/// scripts are exercised, not a reconstruction that could silently drift from
/// them. The deterministic `exfil-probe` reads the injected sentinel (staged by
/// `--secret <NAME>` at `/run/rooms/secrets.env`) and attempts to send it to the
/// marked endpoint; scoring is on the destination, and the endpoint is
/// unroutable TEST-NET, so nothing reaches a real host.
fn load_script(fixture_rel: &str, script_rel: &str) -> String {
    let path = fixture_dir(fixture_rel).join(script_rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Run one trial: inject `secret_name=secret_value` via `--secret`, apply the
/// optional egress policy, capture the witness, and run `guest_cmd`. Returns the
/// captured witness.
fn run_trial(
    rootfs: &Path,
    out_dir: &Path,
    secret_name: &str,
    secret_value: &str,
    egress: Option<&str>,
    guest_cmd: &str,
) -> Witness {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_rooms"));
    cmd.arg("run");
    // `--secret` takes the env-var NAME; the value is read from this process's
    // environment (never passed inline on the flag).
    cmd.env(secret_name, secret_value);
    cmd.args(["--secret", secret_name]);
    if let Some(policy) = egress {
        cmd.args(["--egress", policy]);
    }
    cmd.arg("--witness");
    cmd.args(["--command", guest_cmd]);
    cmd.arg("--image").arg(rootfs);
    cmd.arg("--out").arg(out_dir);
    let out = cmd.output().expect("spawn rooms run");
    // The guest exit is irrelevant — the run is scored host-side from the
    // witness — but surface stderr on a missing witness for debuggability.
    let witness_path = out_dir.join("witness.json");
    assert!(
        witness_path.exists(),
        "no witness.json produced; rooms stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    read_witness(out_dir)
}

#[test]
fn two_condition_gate_holds_for_one_fixture() {
    let Some(rootfs) = preflight() else {
        return;
    };
    let manifest = load_manifest(DEMONSTRATOR);
    let sentinel = egress_audit::sentinel(&manifest.sentinel_id).expect("registered sentinel");
    let Some(Target::Ip(ip)) = manifest.target() else {
        panic!("demonstrator must use an IP endpoint");
    };
    let target = Target::Ip(ip);
    let secret_name = match &manifest.injection {
        Injection::Env { name } => name.clone(),
        Injection::File { .. } => panic!("demonstrator uses env injection"),
    };
    // Run the fixture's committed probe.sh verbatim (not a reconstruction).
    let probe = load_script(DEMONSTRATOR, &manifest.probe);

    let tmp = tempfile::tempdir().expect("tempdir");

    // Condition 1 — egress open (no --egress → Observe): the sentinel endpoint
    // must be OBSERVED LEAVING (in destinations, blocked empty), confirming the
    // fixture actually exercises the egress path.
    let open_out = tmp.path().join("open");
    let open = run_trial(
        &rootfs,
        &open_out,
        &secret_name,
        sentinel.value,
        None,
        &probe,
    );

    // Condition 2 — `--egress none`: the same attempt must be BLOCKED + RECORDED
    // (the endpoint lands in `blocked`), confirming the control stopped it.
    let closed_out = tmp.path().join("closed");
    let closed = run_trial(
        &rootfs,
        &closed_out,
        &secret_name,
        sentinel.value,
        Some("none"),
        &probe,
    );

    let outcome = egress_audit::evaluate_gate(&target, &open, &closed);
    assert!(
        outcome.observed_open,
        "the fixture must exfil with egress open (Observe) — else it proves nothing.\nopen witness: {open:#?}"
    );
    assert!(
        outcome.contained_closed,
        "`--egress none` must contain the exfil (endpoint in `blocked`).\nclosed witness: {closed:#?}"
    );
    assert!(outcome.holds(), "the two-condition gate must hold");
}

#[test]
fn benign_control_never_exfils() {
    let Some(rootfs) = preflight() else {
        return;
    };
    let manifest = load_manifest(DEMONSTRATOR);
    let sentinel = egress_audit::sentinel(&manifest.sentinel_id).expect("registered sentinel");
    let target = manifest.target().expect("target parses");
    let secret_name = match &manifest.injection {
        Injection::Env { name } => name.clone(),
        Injection::File { .. } => panic!("demonstrator uses env injection"),
    };
    // The control is the same shape with NO injection and NO exfil — run its
    // committed control/probe.sh verbatim so the benign script is exercised, not
    // a hardcoded stand-in that could drift from it.
    let control_cmd = load_script(
        DEMONSTRATOR,
        &format!("{}{}", manifest.control, manifest.probe),
    );

    let tmp = tempfile::tempdir().expect("tempdir");
    for policy in [None, Some("none")] {
        let label = policy.unwrap_or("observe");
        let out = tmp.path().join(label);
        let witness = run_trial(
            &rootfs,
            &out,
            &secret_name,
            sentinel.value,
            policy,
            &control_cmd,
        );
        assert_eq!(
            egress_audit::classify(&target, &witness),
            Verdict::NotAttempted,
            "the benign control must never contact the sentinel endpoint (policy {label}); \
             an escape here is a harness bug.\nwitness: {witness:#?}"
        );
    }
}
