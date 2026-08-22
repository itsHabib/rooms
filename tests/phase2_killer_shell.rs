//! Shell-semantics regressions for the privileged Phase-2 proof harness.

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::panic, reason = "integration test module")]

use std::io::Write;
use std::process::{Command, Output, Stdio};

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
