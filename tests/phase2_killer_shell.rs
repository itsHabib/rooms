//! Shell-semantics regressions for the privileged Phase-2 proof harness.

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::panic, reason = "integration test module")]

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use rooms::RoomsConfig;

fn shell_function(source: &str, name: &str) -> String {
    let marker = format!("{name}() {{\n");
    let start = source.find(&marker).expect("shell function exists");
    let body = source.get(start..).expect("function start is a boundary");
    let end = body
        .find("\n}\n")
        .expect("shell function has a closing brace")
        + 3;
    body.get(..end)
        .expect("function end is a boundary")
        .to_owned()
}

fn is_bare_shell_return(line: &str) -> bool {
    let code = line.split('#').next().unwrap_or_default().trim();
    code.strip_suffix(';').unwrap_or(code).trim() == "return"
}

fn run_bash(script: &str, args: &[&str]) -> Output {
    let mut child = Command::new("/bin/bash")
        .args(["--noprofile", "--norc", "-s", "--"])
        .args(args)
        .env_clear()
        .env("LC_ALL", "C")
        .env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("bash starts");
    child
        .stdin
        .take()
        .expect("bash stdin is piped")
        .write_all(script.as_bytes())
        .expect("shell regression is written");
    child.wait_with_output().expect("bash exits")
}

fn readonly_seconds(source: &str, name: &str) -> u64 {
    let prefix = format!("readonly {name}=");
    source
        .lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .expect("named readonly shell budget exists")
        .parse()
        .expect("named readonly shell budget is integer seconds")
}

fn program_on_path(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|directory| directory.join(name))
            .find(|candidate| candidate.is_file())
    })
}

#[test]
fn phase2_witness_wall_cap_exceeds_the_runtime_ssh_budget() {
    let source = include_str!("../scripts/phase2-killer.sh");
    let witness_max_wall = readonly_seconds(source, "WITNESS_MAX_WALL_SECONDS");
    let guest_reach = RoomsConfig::default().guest_reach_timeout.as_secs();
    let required = guest_reach + 60;

    assert!(
        witness_max_wall >= required,
        "witness cap {witness_max_wall}s must leave 60s of workload time after the {guest_reach}s SSH budget"
    );
    assert!(source.contains("--max-wall \"${WITNESS_MAX_WALL_SECONDS}s\""));
}

#[test]
fn phase2_witness_raw_pcap_reader_feeds_tcpdump_stdin() {
    let source = include_str!("../scripts/phase2-killer.sh");
    let read_pcap = shell_function(source, "witness_pcap_has_blocked_tuple");
    let temp = tempfile::tempdir().expect("temporary pcap-reader fixture");
    let bin_dir = temp.path().join("bin");
    std::fs::create_dir(&bin_dir).expect("fixture bin directory is created");
    let tcpdump = bin_dir.join("tcpdump");
    std::fs::write(
        &tcpdump,
        concat!(
            "#!/bin/sh\n",
            "printf '%s\\n' \"$@\" >\"$TCPDUMP_ARGS_FILE\"\n",
            "cat >\"$TCPDUMP_STDIN_FILE\"\n",
            "if [ \"${TCPDUMP_EMIT_PACKET:-}\" = 1 ]; then printf 'matching packet\\n'; fi\n",
        ),
    )
    .expect("fake tcpdump is written");
    let mut permissions = std::fs::metadata(&tcpdump)
        .expect("fake tcpdump metadata exists")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&tcpdump, permissions).expect("fake tcpdump is executable");
    let pcap = temp.path().join("retained witness.pcap");
    std::fs::write(&pcap, b"retained-pcap-bytes\n").expect("pcap fixture is written");
    let script = format!(
        r#"set -Eeuo pipefail
sudo() {{
    [[ "$1" == -n ]] || return 90
    shift
    "$@"
}}
{read_pcap}
PATH="$1/bin:/usr/bin:/bin"
LOG_DIR="$1"
TCPDUMP_ARGS_FILE="$1/tcpdump.args"
TCPDUMP_STDIN_FILE="$1/tcpdump.stdin"
TCPDUMP_EMIT_PACKET=1
export PATH TCPDUMP_ARGS_FILE TCPDUMP_STDIN_FILE TCPDUMP_EMIT_PACKET
witness_pcap_has_blocked_tuple "$2" 45675
TCPDUMP_EMIT_PACKET=0
export TCPDUMP_EMIT_PACKET
if witness_pcap_has_blocked_tuple "$2" 45675; then
    exit 91
fi
"#
    );
    let output = run_bash(
        &script,
        &[
            temp.path().to_str().expect("proof path is utf-8"),
            pcap.to_str().expect("pcap path is utf-8"),
        ],
    );

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read(temp.path().join("tcpdump.stdin")).expect("stdin capture exists"),
        b"retained-pcap-bytes\n"
    );
    assert_eq!(
        std::fs::read_to_string(temp.path().join("tcpdump.args"))
            .expect("tcpdump argument capture exists"),
        concat!(
            "-nn\n",
            "-r\n",
            "-\n",
            "-c\n",
            "1\n",
            "dst host 1.1.1.1 and tcp dst port 45675\n",
        )
    );
}

#[test]
fn phase2_witness_nonzero_is_captured_and_accounted_without_errexit() {
    let source = include_str!("../scripts/phase2-killer.sh");
    let run_witness = shell_function(source, "run_witness_batch");
    let account_status = shell_function(source, "account_witness_command_status");
    let temp = tempfile::tempdir().expect("temporary witness fixture");
    let script = format!(
        r#"set -Eeuo pipefail
trap 'printf "err-trap\n"' ERR
run_rooms() {{ return 124; }}
hard_failure() {{ printf 'hard=%s\n' "$1"; }}
{run_witness}
{account_status}
SNAPSHOT_DIR=snapshot
IMAGE=image
WITNESS_COMMAND=command
WITNESS_ROOT="$1/out"
WITNESS_JSON="$1/witness.json"
LOG_DIR="$1"
WITNESS_MAX_WALL_SECONDS=180
WITNESS_COMMAND_STATUS=
WITNESS_PASS=true
run_witness_batch
account_witness_command_status
printf 'continued status=%s pass=%s\n' "$WITNESS_COMMAND_STATUS" "$WITNESS_PASS"
"#
    );
    let output = run_bash(
        &script,
        &[temp.path().to_str().expect("proof path is utf-8")],
    );

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "hard=witness_batch_command_failed\ncontinued status=124 pass=false\n"
    );
    assert!(source.contains("WITNESS_COMMAND_STATUS=\"\""));
    assert!(source.contains("command_exit_code: number_or_null($witness_command_status)"));
}

#[test]
fn phase2_witness_signal_keeps_the_inflight_command_status_unknown() {
    let source = include_str!("../scripts/phase2-killer.sh");
    let run_witness = shell_function(source, "run_witness_batch");
    let temp = tempfile::tempdir().expect("temporary witness-signal fixture");
    let script = format!(
        r#"set -Eeuo pipefail
exec 3>&1
trap 'printf "signal status=<%s>\n" "$WITNESS_COMMAND_STATUS" >&3; exit 130' TERM
run_rooms() {{ kill -TERM "$$"; }}
{run_witness}
SNAPSHOT_DIR=snapshot
IMAGE=image
WITNESS_COMMAND=command
WITNESS_ROOT="$1/out"
WITNESS_JSON="$1/witness.json"
LOG_DIR="$1"
WITNESS_MAX_WALL_SECONDS=180
WITNESS_COMMAND_STATUS=stale
run_witness_batch
printf 'unreachable status=<%s>\n' "$WITNESS_COMMAND_STATUS" >&3
"#
    );
    let output = run_bash(
        &script,
        &[temp.path().to_str().expect("proof path is utf-8")],
    );

    assert_eq!(output.status.code(), Some(130));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "signal status=<>\n"
    );
    assert!(source.contains("WITNESS_COMMAND_STATUS=\"\""));
}

#[test]
fn phase2_witness_signal_during_validation_cannot_publish_a_pass() {
    let source = include_str!("../scripts/phase2-killer.sh");
    let begin = shell_function(source, "begin_witness_validation");
    let commit = shell_function(source, "commit_witness_validation_result");
    let script = format!(
        r#"set -Eeuo pipefail
exec 3>&1
trap 'printf "signal status=<%s> pass=<%s>\n" "$WITNESS_COMMAND_STATUS" "$WITNESS_PASS" >&3; exit 130' TERM
{begin}
{commit}
HARD_FAILURES=0
WITNESS_COMMAND_STATUS=0
WITNESS_PASS=stale
WITNESS_HARD_FAILURES_BEFORE=
begin_witness_validation
kill -TERM "$$"
commit_witness_validation_result
"#
    );
    let output = run_bash(&script, &[]);

    assert_eq!(output.status.code(), Some(130));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "signal status=<0> pass=<false>\n"
    );

    let witness_phase = source
        .split_once("PHASE=\"eight-clone-witness\"")
        .expect("witness phase exists")
        .1
        .split_once("PHASE=\"final-leak-audit\"")
        .expect("final audit follows witness")
        .0;
    let begin_position = witness_phase
        .find("begin_witness_validation")
        .expect("witness validation begins false-first");
    let final_check = witness_phase
        .find("witness_content_alias")
        .expect("capture uniqueness is the final witness check");
    let commit_position = witness_phase
        .find("commit_witness_validation_result")
        .expect("witness validation has a commit point");
    assert!(begin_position < final_check && final_check < commit_position);
    assert!(!witness_phase[..commit_position].contains("WITNESS_PASS=\"true\""));
}

#[test]
fn phase2_long_running_summary_checks_are_false_until_their_final_check() {
    let source = include_str!("../scripts/phase2-killer.sh");
    for (pass, phase, next_phase, final_check) in [
        (
            "READINESS_PASS",
            "eight-clone-live",
            "fleet-memory",
            "clone_not_workload_ready",
        ),
        (
            "PSS_PASS",
            "fleet-memory",
            "fleet-two-hop-return-path",
            "pss_density_missed",
        ),
        (
            "TWO_HOP_PASS",
            "fleet-two-hop-return-path",
            "fleet-topology",
            "proof_endpoint_leaked",
        ),
        (
            "TOPOLOGY_PASS",
            "fleet-topology",
            "fleet-identity",
            "namespace_inode_not_unique",
        ),
        (
            "IDENTITY_PASS",
            "fleet-identity",
            "fleet-isolation",
            "clone_repo_head_drift",
        ),
        (
            "ISOLATION_PASS",
            "fleet-isolation",
            "fleet-teardown",
            "guest_reachable_from_root_namespace",
        ),
    ] {
        let phase_start = format!("PHASE=\"{phase}\"");
        let phase_end = format!("PHASE=\"{next_phase}\"");
        let body = source
            .split_once(&phase_start)
            .unwrap_or_else(|| panic!("{phase} exists"))
            .1
            .split_once(&phase_end)
            .unwrap_or_else(|| panic!("{next_phase} follows {phase}"))
            .0;
        let false_position = body
            .find(&format!("{pass}=\"false\""))
            .unwrap_or_else(|| panic!("{pass} starts false"));
        let check_position = body
            .rfind(final_check)
            .unwrap_or_else(|| panic!("{pass} final check exists"));
        let true_position = body
            .rfind(&format!("{pass}=\"true\""))
            .unwrap_or_else(|| panic!("{pass} has a final commit"));
        assert!(false_position < check_position && check_position < true_position);
    }
}

#[test]
fn phase2_failed_final_roster_cannot_commit_a_false_clean_audit() {
    let source = include_str!("../scripts/phase2-killer.sh");
    let capture_roster = shell_function(source, "capture_final_roster");
    let commit_audit = shell_function(source, "commit_final_leak_audit_result");
    let temp = tempfile::tempdir().expect("temporary final-audit fixture");
    let script = format!(
        r#"set -Eeuo pipefail
trap 'printf "err-trap\n"' ERR
run_rooms() {{ return 73; }}
jq() {{ printf 'jq-should-not-run\n'; return 99; }}
hard_failure() {{ HARD_FAILURES=$((HARD_FAILURES + 1)); printf 'hard=%s\n' "$1"; }}
{capture_roster}
{commit_audit}
PROOF_ROOT="$1"
LOG_DIR="$1"
HARD_FAILURES=4
FINAL_AUDIT_FAILURES_BEFORE=4
FINAL_LEAK_AUDIT_PASS=false
capture_final_roster
commit_final_leak_audit_result
printf 'continued hard_failures=%s audit=%s\n' "$HARD_FAILURES" "$FINAL_LEAK_AUDIT_PASS"
"#
    );
    let output = run_bash(
        &script,
        &[temp.path().to_str().expect("proof path is utf-8")],
    );

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "hard=final_roster_unreadable\ncontinued hard_failures=5 audit=false\n"
    );
}

#[test]
fn phase2_final_roster_requires_the_exact_empty_schema() {
    let source = include_str!("../scripts/phase2-killer.sh");
    let capture_roster = shell_function(source, "capture_final_roster");
    let commit_audit = shell_function(source, "commit_final_leak_audit_result");
    let temp = tempfile::tempdir().expect("temporary final-roster fixture");
    let script = format!(
        r#"set -Eeuo pipefail
run_rooms() {{ printf '%s\n' "$ROSTER"; }}
jq() {{
    if [[ "$1" != -se ]] \
        || [[ "$2" != *'length == 1'* ]] \
        || [[ "$2" != *'.[0].schema_version == 1'* ]] \
        || [[ "$2" != *'.[0].rooms | type == "array" and length == 0'* ]]; then
        printf 'predicate-mismatch\n'
        return 98
    fi
    [[ "$(<"$3")" == '{{"schema_version":1,"rooms":[]}}' ]]
}}
hard_failure() {{ HARD_FAILURES=$((HARD_FAILURES + 1)); printf 'hard=%s\n' "$1"; }}
{capture_roster}
{commit_audit}
PROOF_ROOT="$1"
LOG_DIR="$1"
ROSTER="$2"
HARD_FAILURES=0
FINAL_AUDIT_FAILURES_BEFORE=0
FINAL_LEAK_AUDIT_PASS=false
capture_final_roster
commit_final_leak_audit_result
printf 'hard_failures=%s audit=%s\n' "$HARD_FAILURES" "$FINAL_LEAK_AUDIT_PASS"
"#
    );
    let run_case = |roster: &str| {
        run_bash(
            &script,
            &[temp.path().to_str().expect("proof path is utf-8"), roster],
        )
    };

    let valid = run_case(r#"{"schema_version":1,"rooms":[]}"#);
    assert!(valid.status.success());
    assert_eq!(
        String::from_utf8_lossy(&valid.stdout),
        "hard_failures=0 audit=true\n"
    );
    for mutant in [
        r"{}",
        r#"{"schema_version":1,"rooms":null}"#,
        r#"{"schema_version":1,"rooms":""}"#,
        r#"{"schema_version":2,"rooms":[]}"#,
    ] {
        let output = run_case(mutant);
        assert!(
            output.status.success(),
            "{mutant}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            "hard=rooms_not_empty\nhard_failures=1 audit=false\n",
            "mutant {mutant}"
        );
    }
}

#[test]
fn phase2_clone_id_tracking_includes_failure_only_records() {
    let source = include_str!("../scripts/phase2-killer.sh");
    let add_id = shell_function(source, "add_created_id");
    let track_ids = shell_function(source, "track_json_ids");
    let temp = tempfile::tempdir().expect("temporary clone-id fixture");
    let script = format!(
        r#"set -Eeuo pipefail
valid_room_id() {{ [[ "$1" =~ ^[0-9a-z]{{26}}$ ]]; }}
jq() {{
    [[ "$1" == -ser ]] \
        && [[ "$*" == *'expected exactly one clone envelope'* ]] \
        && [[ "$*" == *'$failures[]?.room_id'* ]] \
        || return 99
    printf '%s\n' 00000000000000000000000001 00000000000000000000000002
}}
fatal() {{ printf 'fatal=%s\n' "$1"; return 1; }}
{add_id}
{track_ids}
CREATED_IDS_FILE="$1/created-room-ids.txt"
: >"$CREATED_IDS_FILE"
track_json_ids "$1/result.json"
sort "$CREATED_IDS_FILE"
"#
    );
    let output = run_bash(
        &script,
        &[temp.path().to_str().expect("proof path is utf-8")],
    );

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "00000000000000000000000001\n00000000000000000000000002\n"
    );

    let final_audit = source
        .split_once("PHASE=\"final-leak-audit\"")
        .expect("final audit phase exists")
        .1;
    let scan = final_audit
        .find("if ! proof_room_paths_absent; then")
        .expect("authoritative room path scan is in the final audit");
    let commit = final_audit
        .find("commit_final_leak_audit_result")
        .expect("final audit has a commit point");
    assert!(scan < commit, "room-path scan must precede audit success");
}

fn clean_witness_batch() -> serde_json::Value {
    let clones = (1..=8)
        .map(|index| {
            let room_id = format!("01{index:024}");
            serde_json::json!({
                "room_id": room_id,
                "snapshot_id": "snapshot",
                "slot": 1,
                "guest_ip": "172.16.0.6",
                "clone_net_index": index,
                "namespace": format!("rooms-c{index}"),
                "host_veth": format!("veth-h{index}"),
                "status": "exited",
                "exit_code": 0,
                "out_dir": format!("/out/{room_id}")
            })
        })
        .collect::<Vec<_>>();
    serde_json::json!({"clones": clones})
}

fn clean_witness_artifact() -> serde_json::Value {
    serde_json::json!({
        "schema_version": 2,
        "tap": "tap-fc1",
        "capture_complete": true,
        "egress_policy": "none",
        "permitted": [],
        "destinations": [{"ip":"1.1.1.1","port":1234,"proto":"tcp","packets":1}],
        "blocked": [{"ip":"1.1.1.1","port":1234,"proto":"tcp","packets":1}],
        "dns_queries": []
    })
}

#[test]
fn phase2_real_jq_rejects_malformed_witness_artifact_collections() {
    let Some(jq_path) = program_on_path("jq") else {
        return;
    };
    let jq_path = jq_path.to_str().expect("jq path is utf-8");
    let source = include_str!("../scripts/phase2-killer.sh");
    let witness_check = shell_function(source, "witness_artifact_is_clean");
    let temp = tempfile::tempdir().expect("temporary witness-artifact fixture");
    let artifact_path = temp.path().join("witness.json");
    let script = format!(
        r#"set -euo pipefail
sudo() {{
    [[ "$1" == -n ]]
    shift
    [[ "$1" == jq ]]
    shift
    "$JQ_BIN" "$@"
}}
{witness_check}
JQ_BIN="$2"
witness_artifact_is_clean "$1" 1234
"#
    );
    let run_bytes = |bytes: &[u8]| {
        std::fs::write(&artifact_path, bytes).expect("witness artifact is written");
        run_bash(
            &script,
            &[
                artifact_path.to_str().expect("artifact path is utf-8"),
                jq_path,
            ],
        )
    };
    let valid = clean_witness_artifact();
    let valid_bytes = serde_json::to_vec(&valid).expect("valid witness serializes");
    assert!(run_bytes(&valid_bytes).status.success());

    let target = serde_json::json!({
        "ip": "1.1.1.1",
        "port": 1234,
        "proto": "tcp",
        "packets": 1
    });
    let mut mutants = Vec::new();
    for (field, value) in [
        ("permitted", serde_json::Value::Null),
        ("permitted", serde_json::json!({})),
        ("destinations", serde_json::json!({"target": target})),
        ("blocked", serde_json::json!({"target": target})),
    ] {
        let mut mutant = valid.clone();
        mutant
            .as_object_mut()
            .expect("witness is an object")
            .insert(field.to_owned(), value);
        mutants.push(mutant);
    }
    let mut missing = valid.clone();
    missing
        .as_object_mut()
        .expect("witness is an object")
        .remove("permitted");
    mutants.push(missing);

    for mutant in mutants {
        let bytes = serde_json::to_vec(&mutant).expect("mutant witness serializes");
        assert!(!run_bytes(&bytes).status.success(), "accepted {mutant}");
    }
    let streamed = format!("{valid}\n{valid}\n");
    assert!(!run_bytes(streamed.as_bytes()).status.success());
}

#[test]
fn phase2_real_jq_rejects_streamed_and_contradictory_witness_envelopes() {
    let Some(jq_path) = program_on_path("jq") else {
        return;
    };
    let jq_path = jq_path.to_str().expect("jq path is utf-8");
    let source = include_str!("../scripts/phase2-killer.sh");
    let witness_check = shell_function(source, "witness_batch_is_clean");
    let temp = tempfile::tempdir().expect("temporary witness-jq fixture");
    let batch_path = temp.path().join("batch.json");
    let valid = clean_witness_batch();
    let witness_script = format!(
        r#"set -euo pipefail
jq() {{ "$JQ_BIN" "$@"; }}
{witness_check}
JQ_BIN="$2"
witness_batch_is_clean "$1" snapshot 172.16.0.6
"#
    );
    let run_witness = || {
        run_bash(
            &witness_script,
            &[batch_path.to_str().expect("batch path is utf-8"), jq_path],
        )
    };

    std::fs::write(
        &batch_path,
        serde_json::to_vec(&valid).expect("valid batch serializes"),
    )
    .expect("valid batch is written");
    assert!(run_witness().status.success());

    let mut contradictory = valid.clone();
    contradictory
        .as_object_mut()
        .expect("batch is an object")
        .insert(
            "failures".to_owned(),
            serde_json::json!([{"room_id":"01000000000000000000000001","status":"failed"}]),
        );
    std::fs::write(
        &batch_path,
        serde_json::to_vec(&contradictory).expect("contradictory batch serializes"),
    )
    .expect("contradictory batch is written");
    assert!(!run_witness().status.success());

    std::fs::write(&batch_path, format!("{valid}\n{valid}\n")).expect("streamed batch is written");
    assert!(!run_witness().status.success());
}

#[test]
fn phase2_real_jq_rejects_a_streamed_final_roster() {
    let Some(jq_path) = program_on_path("jq") else {
        return;
    };
    let jq_path = jq_path.to_str().expect("jq path is utf-8");
    let source = include_str!("../scripts/phase2-killer.sh");
    let temp = tempfile::tempdir().expect("temporary roster-jq fixture");
    let capture_roster = shell_function(source, "capture_final_roster");
    let commit_audit = shell_function(source, "commit_final_leak_audit_result");
    let roster_script = format!(
        r#"set -euo pipefail
run_rooms() {{ printf '%s\n' "$ROSTER"; }}
jq() {{ "$JQ_BIN" "$@"; }}
hard_failure() {{ HARD_FAILURES=$((HARD_FAILURES + 1)); printf 'hard=%s\n' "$1"; }}
{capture_roster}
{commit_audit}
PROOF_ROOT="$1"
LOG_DIR="$1"
JQ_BIN="$2"
ROSTER="$3"
HARD_FAILURES=0
FINAL_AUDIT_FAILURES_BEFORE=0
FINAL_LEAK_AUDIT_PASS=false
capture_final_roster
commit_final_leak_audit_result
printf 'hard_failures=%s audit=%s\n' "$HARD_FAILURES" "$FINAL_LEAK_AUDIT_PASS"
"#
    );
    let streamed_roster =
        "{\"schema_version\":1,\"rooms\":[{\"id\":\"live\"}]}\n{\"schema_version\":1,\"rooms\":[]}";
    let roster_output = run_bash(
        &roster_script,
        &[
            temp.path().to_str().expect("proof path is utf-8"),
            jq_path,
            streamed_roster,
        ],
    );
    assert!(roster_output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&roster_output.stdout),
        "hard=rooms_not_empty\nhard_failures=1 audit=false\n"
    );
}

#[test]
fn phase2_real_jq_validates_clone_id_envelopes() {
    let Some(jq_path) = program_on_path("jq") else {
        return;
    };
    let jq_path = jq_path.to_str().expect("jq path is utf-8");
    let source = include_str!("../scripts/phase2-killer.sh");
    let temp = tempfile::tempdir().expect("temporary clone-id-jq fixture");
    let add_id = shell_function(source, "add_created_id");
    let track_ids = shell_function(source, "track_json_ids");
    let clone_path = temp.path().join("clones.json");
    let track_script = format!(
        r#"set -euo pipefail
jq() {{ "$JQ_BIN" "$@"; }}
valid_room_id() {{ [[ "$1" =~ ^[0-9a-z]{{26}}$ ]]; }}
fatal() {{ return 91; }}
{add_id}
{track_ids}
JQ_BIN="$3"
CREATED_IDS_FILE="$2"
: >"$CREATED_IDS_FILE"
track_json_ids "$1"
"#
    );
    let tracked_path = temp.path().join("tracked.txt");
    let run_track = || {
        run_bash(
            &track_script,
            &[
                clone_path.to_str().expect("clone path is utf-8"),
                tracked_path.to_str().expect("tracked path is utf-8"),
                jq_path,
            ],
        )
    };

    std::fs::write(
        &clone_path,
        "{\"clones\":[{\"room_id\":\"00000000000000000000000001\"}]}\n",
    )
    .expect("valid clone envelope is written");
    let valid_output = run_track();
    assert!(
        valid_output.status.success(),
        "{}",
        String::from_utf8_lossy(&valid_output.stderr)
    );
    assert_eq!(
        std::fs::read(&tracked_path).expect("tracked file exists"),
        b"00000000000000000000000001\n"
    );

    std::fs::write(
        &clone_path,
        "{\"clones\":[{\"room_id\":\"00000000000000000000000001\\u0000\"}]}\n",
    )
    .expect("NUL-bearing clone envelope is written");
    let nul_output = run_track();
    assert!(!nul_output.status.success());
    assert_eq!(
        std::fs::read(&tracked_path).expect("tracked file exists"),
        b""
    );

    std::fs::write(
        &clone_path,
        concat!(
            "{\"clones\":[",
            "{\"room_id\":\"00000000000000000000000001\"},",
            "{\"room_id\":\"00000000000000000000000002\\n\"}",
            "]}\n"
        ),
    )
    .expect("mixed valid and newline-bearing clone envelope is written");
    let newline_output = run_track();
    assert!(!newline_output.status.success());
    assert_eq!(
        std::fs::read(&tracked_path).expect("tracked file exists"),
        b""
    );

    std::fs::write(
        &clone_path,
        concat!(
            "{\"clones\":[{\"room_id\":\"00000000000000000000000001\"}]}\n",
            "{\"clones\":[{\"room_id\":\"00000000000000000000000002\"}]}\n"
        ),
    )
    .expect("streamed clone envelopes are written");
    let streamed_output = run_track();
    assert!(!streamed_output.status.success());
    assert_eq!(
        std::fs::read(&tracked_path).expect("tracked file exists"),
        b""
    );
}

#[test]
fn phase2_exit_helpers_do_not_inherit_the_traps_failure_status() {
    let source = include_str!("../scripts/phase2-killer.sh");
    let temp = tempfile::tempdir().expect("temporary proof fixture");
    let input = temp.path().join("canonical-slot");
    let output_path = temp.path().join("protected.tsv");
    std::fs::write(&input, b"owner\n").expect("canonical fixture is written");

    let capture = shell_function(source, "capture_protected_entry");
    let capture_script = format!(
        r#"set -u
stat() {{ printf '%s\n' metadata; }}
sha256sum() {{ printf '%064d  %s\n' 0 "$1"; }}
{capture}
INPUT=$1
OUTPUT=$2
trap 'capture_protected_entry "$OUTPUT" "$INPUT"; status=$?; printf "%s\n" "$status"; trap - EXIT; exit 0' EXIT
exit 7
"#
    );
    let capture_output = run_bash(
        &capture_script,
        &[
            input.to_str().expect("input path is utf-8"),
            output_path.to_str().expect("output path is utf-8"),
        ],
    );
    assert!(
        capture_output.status.success(),
        "{}",
        String::from_utf8_lossy(&capture_output.stderr)
    );
    assert_eq!(capture_output.stdout, b"0\n");

    let close_gate = shell_function(source, "close_flat_restore_gate");
    let quiesce = shell_function(source, "quiesce_flat_restore_driver");
    let quiesce_script = format!(
        r#"set -u
{close_gate}
{quiesce}
PROOF_ROOT=$1
FLAT_RESTORE_GATE_FD=
FLAT_RESTORE_GATE=
FLAT_RESTORE_GATE_READY=
FLAT_RESTORE_PID=
FLAT_RESTORE_PID_STARTTIME=
FLAT_RESTORE_PGID=
FLAT_RESTORE_SID=
FLAT_RESTORE_PARENT_SID=
FLAT_RESTORE_LAUNCH_STAGE=idle
true &
wait "$!"
trap 'quiesce_flat_restore_driver; status=$?; printf "%s\n" "$status"; trap - EXIT; exit 0' EXIT
exit 7
"#
    );
    let quiesce_output = run_bash(
        &quiesce_script,
        &[temp.path().to_str().expect("proof path is utf-8")],
    );
    assert!(
        quiesce_output.status.success(),
        "{}",
        String::from_utf8_lossy(&quiesce_output.stderr)
    );
    assert_eq!(quiesce_output.stdout, b"0\n");
}

#[test]
fn phase2_exit_path_functions_have_no_bare_shell_return() {
    for mutant in [
        "return",
        " return;",
        "return ;",
        "return # inherited status",
        "return; # inherited status",
    ] {
        assert!(is_bare_shell_return(mutant), "missed {mutant:?}");
    }
    assert!(!is_bare_shell_return("return 0"));
    assert!(!is_bare_shell_return("return \"$status\""));

    let source = include_str!("../scripts/phase2-killer.sh");
    for name in [
        "run_rooms",
        "capture_protected_entry",
        "capture_protected_state",
        "close_flat_restore_gate",
        "terminate_flat_restore_group",
        "clear_flat_restore_identity",
        "quiesce_flat_restore_driver",
    ] {
        let function = shell_function(source, name);
        assert!(
            function.lines().all(|line| !is_bare_shell_return(line)),
            "{name} contains a bare return that can inherit an EXIT trap status"
        );
    }
}

#[test]
fn phase2_protected_capture_propagates_entry_and_inventory_failures() {
    let source = include_str!("../scripts/phase2-killer.sh");
    let temp = tempfile::tempdir().expect("temporary proof fixture");
    let slot = temp.path().join("canonical-slot");
    let images = temp.path().join("images");
    let output_path = temp.path().join("protected.tsv");
    let symlink_path = temp.path().join("canonical-link");
    std::fs::write(&slot, b"owner\n").expect("canonical fixture is written");
    std::fs::create_dir(&images).expect("canonical images fixture is created");
    std::os::unix::fs::symlink(&slot, &symlink_path).expect("canonical symlink is created");

    let capture_entry = shell_function(source, "capture_protected_entry");
    let entry_script = format!(
        r#"set -u
stat() {{ printf '%s\n' metadata; }}
sha256sum() {{ return 1; }}
{capture_entry}
capture_protected_entry "$2" "$1"
"#
    );
    let entry_output = run_bash(
        &entry_script,
        &[
            slot.to_str().expect("slot path is utf-8"),
            output_path.to_str().expect("output path is utf-8"),
        ],
    );
    assert!(!entry_output.status.success());
    let symlink_output = run_bash(
        &entry_script,
        &[
            symlink_path.to_str().expect("symlink path is utf-8"),
            output_path.to_str().expect("output path is utf-8"),
        ],
    );
    assert!(!symlink_output.status.success());

    let capture_state = shell_function(source, "capture_protected_state");
    let inventory_script = format!(
        r#"set -u
stat() {{ printf '%s\n' metadata; }}
sha256sum() {{ printf '%064d  %s\n' 0 "$1"; }}
find() {{ return 1; }}
sort() {{ cat; }}
{capture_entry}
{capture_state}
CANONICAL_SLOT=$1
CANONICAL_IMAGES=$2
capture_protected_state "$3"
"#
    );
    let inventory_output = run_bash(
        &inventory_script,
        &[
            slot.to_str().expect("slot path is utf-8"),
            images.to_str().expect("images path is utf-8"),
            output_path.to_str().expect("output path is utf-8"),
        ],
    );
    assert!(!inventory_output.status.success());
}

#[test]
fn phase2_guest_clock_window_is_bounded_and_wrap_safe() {
    let source = include_str!("../scripts/phase2-killer.sh");
    let validator = shell_function(source, "guest_clock_within_host_window");
    let script = format!("{validator}\nguest_clock_within_host_window \"$@\"\n");
    let run_case = |args: &[&str]| run_bash(&script, args).status.success();

    assert!(run_case(&[
        "100",
        "100000000000",
        "101000000000",
        "1000000000",
        "2000000000",
    ]));
    assert!(!run_case(&[
        "95",
        "100000000000",
        "101000000000",
        "1000000000",
        "2000000000",
    ]));
    assert!(!run_case(&[
        "106",
        "100000000000",
        "101000000000",
        "1000000000",
        "2000000000",
    ]));
    assert!(!run_case(&[
        "18446744073709551716",
        "100000000000",
        "101000000000",
        "1000000000",
        "2000000000",
    ]));
    assert!(!run_case(&[
        "100",
        "100000000000",
        "110000000000",
        "1000000000",
        "2000000000",
    ]));
    assert!(!run_case(&[
        "100",
        "100000000000",
        "111000000000",
        "1000000000",
        "12000000000",
    ]));
    assert!(!run_case(&[
        "96",
        "101800000000",
        "101920000000",
        "0",
        "210000000",
    ]));
    assert!(!run_case(&[
        "96",
        "100800000000",
        "100920000000",
        "0",
        "210000000",
    ]));
}
