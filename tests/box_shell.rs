//! Hermetic regressions for `scripts/box.sh`: fake `limactl`, `gcloud`, and
//! `ssh` programs stand in for the compute backends and record every call, so
//! each command path runs without a VM or a cloud account.

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::panic, reason = "integration test module")]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

// `BOX_TEST_LIMA_INSTANCES` holds "<name> <token>" lines, as `limactl list` prints them.
const FAKE_LIMACTL: &str = r#"#!/bin/sh
echo "limactl $*" >>"$BOX_TEST_LOG"
case "$1 $3" in
    "list {{.Dir}}") printf '%s\n' "$BOX_TEST_LIMA_DIR" ;;
    "list {{.Name}} "*) printf '%s\n' "${BOX_TEST_LIMA_INSTANCES:-}" ;;
esac
exit 0
"#;

const FAKE_GCLOUD: &str = r#"#!/bin/sh
echo "gcloud $*" >>"$BOX_TEST_LOG"
case "$*" in
    *"instances describe"*) echo 203.0.113.7 ;;
    *"instances list"*)
        [ -n "${BOX_TEST_GCP_LIST_FAILS:-}" ] && exit 1
        printf '%s\n' "${BOX_TEST_GCP_LISTED:-}" ;;
esac
exit 0
"#;

// The remote command is always ssh's last argument.
const FAKE_SSH: &str = r#"#!/bin/sh
echo "ssh $*" >>"$BOX_TEST_LOG"
for last in "$@"; do :; done
case "$last" in
    *"rooms doctor"*) cat "$BOX_TEST_DOCTOR" ;;
    *"tar -x"*) cat >"$BOX_TEST_TAR" ;;
esac
exit 0
"#;

const DOCTOR_WARN_ONLY: &str = r#"{"schema_version":1,"checks":[
{"name":"kvm","ok":true,"message":"/dev/kvm is usable"},
{"name":"anthropic_api_key","ok":true,"message":"warn: no Anthropic credential set"}]}"#;

const DOCTOR_FAILING: &str = r#"{"schema_version":1,"checks":[
{"name":"kvm","ok":true,"message":"/dev/kvm is usable"},
{"name":"rooms_fwd","ok":false,"message":"ROOMS_FWD not installed"}]}"#;

struct Harness {
    root: tempfile::TempDir,
}

impl Harness {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("temp dir");
        let bin = root.path().join("bin");
        fs::create_dir(&bin).expect("fake bin dir");
        for (name, body) in [
            ("limactl", FAKE_LIMACTL),
            ("gcloud", FAKE_GCLOUD),
            ("ssh", FAKE_SSH),
        ] {
            let path = bin.join(name);
            fs::write(&path, body).expect("fake written");
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
                .expect("fake is executable");
        }
        let lima = root.path().join("lima-instance");
        fs::create_dir(&lima).expect("lima instance dir");
        fs::write(lima.join("ssh.config"), "Host lima-fake\n").expect("lima ssh config");
        fs::write(root.path().join("doctor.json"), DOCTOR_WARN_ONLY).expect("doctor report");
        Self { root }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.root.path().join(rel)
    }

    fn run(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let path = format!(
            "{}:{}",
            self.path("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        Command::new("bash")
            .arg(repo_root().join("scripts/box.sh"))
            .args(args)
            .env("PATH", path)
            .env("HOME", self.root.path())
            .env("ROOMS_BOX_STATE", self.path("state"))
            .env("ROOMS_BOX_SSH_TIMEOUT", "5")
            .env("BOX_TEST_LOG", self.path("calls.log"))
            .env("BOX_TEST_LIMA_DIR", self.path("lima-instance"))
            .env("BOX_TEST_DOCTOR", self.path("doctor.json"))
            .env("BOX_TEST_TAR", self.path("shipped.tar"))
            .env_remove("ROOMS_BOX_GCP_PROJECT")
            .envs(env.iter().copied())
            .output()
            .expect("box.sh runs")
    }

    fn calls(&self) -> String {
        fs::read_to_string(self.path("calls.log")).unwrap_or_default()
    }

    fn up(&self, name: &str, backend: &str, extra: &[&str]) -> Output {
        let mut args = vec!["up", name, "--backend", backend];
        args.extend_from_slice(extra);
        let out = self.run(&args, &[]);
        assert!(out.status.success(), "up failed: {}", stderr(&out));
        out
    }

    fn token(&self, name: &str) -> String {
        let env = fs::read_to_string(self.path(&format!("state/{name}/box.env"))).expect("box.env");
        env.lines()
            .find_map(|line| line.strip_prefix("BOX_TOKEN="))
            .expect("box.env records a token")
            .to_owned()
    }
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn position(haystack: &str, needle: &str) -> usize {
    haystack
        .find(needle)
        .unwrap_or_else(|| panic!("expected {needle:?} in:\n{haystack}"))
}

#[test]
fn rejects_invalid_names_before_any_backend_call() {
    let h = Harness::new();
    let out = h.run(&["up", "Bad_Name", "--backend", "lima"], &[]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("invalid box name"),
        "{}",
        stderr(&out)
    );
    assert!(
        h.calls().is_empty(),
        "no backend call expected: {}",
        h.calls()
    );
}

#[test]
fn gcp_up_requires_an_explicit_project() {
    let h = Harness::new();
    let out = h.run(&["up", "cloudbox", "--backend", "gcp"], &[]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("ROOMS_BOX_GCP_PROJECT"),
        "{}",
        stderr(&out)
    );
    assert!(
        h.calls().is_empty(),
        "no gcloud call expected: {}",
        h.calls()
    );
    assert!(
        !h.path("state/cloudbox").exists(),
        "no state for a refused box"
    );
}

#[test]
fn gcp_up_creates_an_auto_deleting_nested_spot_vm() {
    let h = Harness::new();
    let out = h.up("cloudbox", "gcp", &["--project", "sandbox-1"]);
    let calls = h.calls();
    for flag in [
        "compute instances create cloudbox",
        "--project=sandbox-1",
        "--enable-nested-virtualization",
        "--provisioning-model=SPOT",
        "--instance-termination-action=DELETE",
        "--max-run-duration=3h",
        "--image-family=ubuntu-2404-lts-amd64",
        "--labels=purpose=rooms-box",
    ] {
        assert!(calls.contains(flag), "missing {flag} in:\n{calls}");
    }
    let label = format!(
        "--labels=purpose=rooms-box,rooms_box_token={}",
        h.token("cloudbox")
    );
    assert!(calls.contains(&label), "missing {label} in:\n{calls}");
    let config = fs::read_to_string(h.path("state/cloudbox/ssh.config")).expect("ssh config");
    assert!(config.contains("HostName 203.0.113.7"), "{config}");
    assert!(config.contains("User rooms"), "{config}");
    let line = stdout(&out);
    assert!(line.contains(r#""backend":"gcp""#), "{line}");
    assert!(line.contains(r#""host":"box-cloudbox""#), "{line}");
}

#[test]
fn lima_up_uses_the_repo_definition_without_mounts() {
    let h = Harness::new();
    let out = h.up("localbox", "lima", &[]);
    let calls = h.calls();
    assert!(
        calls.contains("limactl create --tty=false --name localbox --set .mounts = []"),
        "{calls}"
    );
    assert!(calls.contains("scripts/lima-rooms-host.yaml"), "{calls}");
    let param = format!(r#".param.roomsBoxToken = "{}""#, h.token("localbox"));
    assert!(calls.contains(&param), "missing {param} in:\n{calls}");
    let line = stdout(&out);
    assert!(line.contains(r#""host":"lima-localbox""#), "{line}");
    let expected = h.path("lima-instance/ssh.config");
    assert!(line.contains(&*expected.to_string_lossy()), "{line}");
}

#[test]
fn up_refuses_a_name_lima_already_has() {
    let h = Harness::new();
    let out = h.run(
        &["up", "rooms-host", "--backend", "lima"],
        &[("BOX_TEST_LIMA_INSTANCES", "rooms-host ")],
    );
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("already has an instance"),
        "{}",
        stderr(&out)
    );
    assert!(!h.calls().contains("limactl create"), "{}", h.calls());
    assert!(
        !h.path("state/rooms-host").exists(),
        "no state for a refused box"
    );
}

#[test]
fn up_refuses_a_name_the_gcp_zone_already_has() {
    let h = Harness::new();
    let out = h.run(
        &[
            "up",
            "cloudbox",
            "--backend",
            "gcp",
            "--project",
            "sandbox-1",
        ],
        &[("BOX_TEST_GCP_LISTED", "cloudbox")],
    );
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("already has an instance"),
        "{}",
        stderr(&out)
    );
    assert!(!h.calls().contains("instances create"), "{}", h.calls());
    assert!(
        !h.path("state/cloudbox").exists(),
        "no state for a refused box"
    );
}

#[test]
fn up_refuses_a_name_that_already_exists() {
    let h = Harness::new();
    h.up("localbox", "lima", &[]);
    let out = h.run(&["up", "localbox", "--backend", "lima"], &[]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("already exists"), "{}", stderr(&out));
}

#[test]
fn down_refuses_boxes_it_did_not_create() {
    let h = Harness::new();
    let out = h.run(&["down", "rooms-host"], &[]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("only manages boxes it created"),
        "{}",
        stderr(&out)
    );
    assert!(
        h.calls().is_empty(),
        "no backend call expected: {}",
        h.calls()
    );
}

#[test]
fn down_deletes_the_recorded_gcp_instance() {
    let h = Harness::new();
    h.up("cloudbox", "gcp", &["--project", "sandbox-1"]);
    let token = h.token("cloudbox");
    let out = h.run(
        &["down", "cloudbox"],
        &[("BOX_TEST_GCP_LISTED", "cloudbox")],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let calls = h.calls();
    let filter = format!("--filter=name=cloudbox AND labels.rooms_box_token={token}");
    assert!(calls.contains(&filter), "missing {filter} in:\n{calls}");
    assert!(
        calls
            .contains("instances delete cloudbox --project=sandbox-1 --zone=us-central1-a --quiet"),
        "{calls}"
    );
    assert!(
        !h.path("state/cloudbox").exists(),
        "state removed after down"
    );
}

#[test]
fn down_treats_a_missing_gcp_instance_as_gone() {
    let h = Harness::new();
    h.up("cloudbox", "gcp", &["--project", "sandbox-1"]);
    let out = h.run(&["down", "cloudbox"], &[]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("nothing to delete"),
        "{}",
        stderr(&out)
    );
    assert!(!h.calls().contains("instances delete"), "{}", h.calls());
    assert!(!h.path("state/cloudbox").exists());
}

#[test]
fn down_keeps_state_when_the_gcp_lookup_fails() {
    let h = Harness::new();
    h.up("cloudbox", "gcp", &["--project", "sandbox-1"]);
    let out = h.run(&["down", "cloudbox"], &[("BOX_TEST_GCP_LIST_FAILS", "1")]);
    assert!(!out.status.success());
    assert!(!h.calls().contains("instances delete"), "{}", h.calls());
    assert!(
        h.path("state/cloudbox/box.env").exists(),
        "state kept for a retry"
    );
}

#[test]
fn down_deletes_a_lima_box_and_its_state() {
    let h = Harness::new();
    h.up("localbox", "lima", &[]);
    let instances = format!("rooms-host \nlocalbox {}", h.token("localbox"));
    let out = h.run(
        &["down", "localbox"],
        &[("BOX_TEST_LIMA_INSTANCES", instances.as_str())],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        h.calls().contains("limactl delete --force localbox"),
        "{}",
        h.calls()
    );
    assert!(!h.path("state/localbox").exists());
}

#[test]
fn down_treats_a_missing_lima_instance_as_gone() {
    let h = Harness::new();
    h.up("localbox", "lima", &[]);
    let out = h.run(
        &["down", "localbox"],
        &[("BOX_TEST_LIMA_INSTANCES", "rooms-host ")],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("nothing to delete"),
        "{}",
        stderr(&out)
    );
    assert!(!h.calls().contains("limactl delete"), "{}", h.calls());
    assert!(!h.path("state/localbox").exists());
}

#[test]
fn down_never_deletes_a_same_named_lima_instance_without_the_token() {
    let h = Harness::new();
    h.up("localbox", "lima", &[]);
    let out = h.run(
        &["down", "localbox"],
        &[("BOX_TEST_LIMA_INSTANCES", "localbox 00000000deadbeef")],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("nothing to delete"),
        "{}",
        stderr(&out)
    );
    assert!(!h.calls().contains("limactl delete"), "{}", h.calls());
}

#[test]
fn ssh_forwards_the_recorded_host_and_command() {
    let h = Harness::new();
    h.up("localbox", "lima", &[]);
    let out = h.run(&["ssh", "localbox", "rooms", "ls"], &[]);
    assert!(out.status.success(), "{}", stderr(&out));
    let expected = format!(
        "ssh -F {} lima-localbox rooms ls",
        h.path("lima-instance/ssh.config").display()
    );
    assert!(
        h.calls().contains(&expected),
        "missing {expected} in:\n{}",
        h.calls()
    );
}

#[test]
fn check_passes_when_doctor_reports_only_warnings() {
    let h = Harness::new();
    h.up("localbox", "lima", &[]);
    let out = h.run(&["check", "localbox"], &[]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("warn anthropic_api_key"),
        "{}",
        stderr(&out)
    );
    assert!(
        h.path("state/localbox/doctor.json").exists(),
        "report is kept"
    );
}

#[test]
fn check_fails_on_any_failed_doctor_check() {
    let h = Harness::new();
    h.up("localbox", "lima", &[]);
    fs::write(h.path("doctor.json"), DOCTOR_FAILING).expect("failing report");
    let out = h.run(&["check", "localbox"], &[]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(stderr(&out).contains("FAIL rooms_fwd"), "{}", stderr(&out));
}

#[test]
fn check_fails_closed_on_an_unreadable_report() {
    let h = Harness::new();
    h.up("localbox", "lima", &[]);
    fs::write(h.path("doctor.json"), "ssh: connect to host: timed out").expect("garbage");
    let out = h.run(&["check", "localbox"], &[]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("no readable report"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn check_fails_closed_on_a_schema_invalid_report() {
    let h = Harness::new();
    h.up("localbox", "lima", &[]);
    for report in [
        r#"{"schema_version":1,"checks":[{"name":"kvm","ok":"false","message":"x"}]}"#,
        r#"{"schema_version":2,"checks":[{"name":"kvm","ok":true,"message":"x"}]}"#,
    ] {
        fs::write(h.path("doctor.json"), report).expect("invalid report");
        let out = h.run(&["check", "localbox"], &[]);
        assert!(!out.status.success(), "accepted {report}");
        assert!(
            stderr(&out).contains("no readable report"),
            "{}",
            stderr(&out)
        );
    }
}

#[test]
fn provision_ships_the_exact_committed_revision_then_builds() {
    let h = Harness::new();
    h.up("localbox", "lima", &[]);
    let out = h.run(&["provision", "localbox"], &[]);
    assert!(out.status.success(), "{}", stderr(&out));

    let head = git_head(&repo_root());
    let calls = h.calls();
    assert!(
        calls.contains(&format!("{head} > ~/rooms/.box-revision")),
        "{calls}"
    );
    let setup = position(&calls, "scripts/setup-rooms-host.sh");
    let network = position(&calls, "scripts/setup-tap.sh --host");
    let build = position(&calls, "cargo build --release --locked");
    assert!(
        setup < network && network < build,
        "provision order:\n{calls}"
    );

    let listing = Command::new("tar")
        .arg("-tf")
        .arg(h.path("shipped.tar"))
        .output()
        .expect("tar lists the shipped tree");
    assert!(String::from_utf8_lossy(&listing.stdout).contains("Cargo.toml"));
}

#[test]
fn provision_refuses_an_unknown_revision_before_shipping() {
    let h = Harness::new();
    h.up("localbox", "lima", &[]);
    let out = h.run(&["provision", "localbox", "--rev", "no-such-revision"], &[]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("unknown revision"),
        "{}",
        stderr(&out)
    );
    assert!(!h.calls().contains("tar -x"), "{}", h.calls());
}

fn git_head(repo: &Path) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "HEAD"])
        .output()
        .expect("git rev-parse runs");
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}
