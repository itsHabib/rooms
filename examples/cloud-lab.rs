//! Repeat a real clone workload and retain its inputs, receipts and host samples.
//! Run on an otherwise idle Linux Rooms host; this does not provision cloud VMs.
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{ensure, Context, Result};
use clap::Parser;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

#[derive(Parser)]
struct Args {
    #[arg(long)]
    rooms: PathBuf,
    #[arg(long)]
    snapshot: PathBuf,
    #[arg(long)]
    image: PathBuf,
    #[arg(long)]
    toolstore: PathBuf,
    #[arg(long)]
    command_file: PathBuf,
    #[arg(long)]
    out: PathBuf,
    #[arg(long, value_delimiter = ',', default_value = "1,2,4,8")]
    counts: Vec<u32>,
    #[arg(long, default_value_t = 2)]
    repeats: u32,
    #[arg(long, default_value_t = 180)]
    wall_seconds: u32,
    /// Optional exact expected guest result.patch digest, independently selected.
    #[arg(long)]
    expected_patch_sha256: Option<String>,
}

fn field<'a>(value: &'a Value, name: &str) -> &'a Value {
    value.get(name).unwrap_or(&Value::Null)
}

fn write_json(path: &Path, value: &Value) -> Result<()> {
    let parent = path.parent().context("JSON output has no parent")?;
    let mut pending = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer_pretty(pending.as_file_mut(), value)?;
    pending.write_all(b"\n")?;
    pending.as_file().sync_all()?;
    pending.persist(path).context("publish JSON evidence")?;
    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    Ok(())
}

fn digest(path: &Path) -> Result<String> {
    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut hash = Sha256::new();
    let mut buffer = vec![0_u8; 65536];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            return Ok(format!("{:x}", hash.finalize()));
        }
        hash.update(buffer.get(..count).context("read exceeds buffer")?);
    }
}

fn observe(program: &str, args: &[&str]) -> Result<Value> {
    let output = Command::new(program).args(args).output()?;
    Ok(json!({"exit": output.status.code(),
        "stdout": String::from_utf8_lossy(&output.stdout),
        "stderr": String::from_utf8_lossy(&output.stderr)}))
}

fn vmm_memory() -> Result<Vec<Value>> {
    let mut rows = Vec::new();
    for entry in fs::read_dir("/proc")? {
        let path = entry?.path();
        if fs::read_to_string(path.join("comm"))
            .unwrap_or_default()
            .trim()
            != "firecracker"
        {
            continue;
        }
        let Ok(smaps) = fs::read_to_string(path.join("smaps_rollup")) else {
            // A disappearing process is a sample race, not zero memory.
            rows.push(json!({"pid": path.file_name(), "unavailable": true}));
            continue;
        };
        rows.push(json!({"pid": path.file_name(), "smaps_rollup": smaps}));
    }
    Ok(rows)
}

fn host_state(rooms: &Path) -> Result<Value> {
    let binary = rooms.to_str().context("non-UTF8 Rooms binary path")?;
    Ok(json!({"vmm": vmm_memory()?,
        "rooms": observe(binary, &["ls", "--json"])?,
        "netns": observe("ip", &["netns", "list"])?,
        "links": observe("ip", &["-j", "link", "show"])?,
        "mounts": observe("findmnt", &["--raw", "--noheadings", "--output", "TARGET"])?,
        "meminfo": fs::read_to_string("/proc/meminfo")?}))
}

fn output<'a>(state: &'a Value, key: &str) -> Option<&'a str> {
    let value = field(state, key);
    if field(value, "exit").as_i64() != Some(0) {
        return None;
    }
    field(value, "stdout").as_str()
}

fn network_and_mounts_clear(state: &Value) -> bool {
    let Some(netns) = output(state, "netns") else {
        return false;
    };
    let Some(mounts) = output(state, "mounts") else {
        return false;
    };
    let Some(links) = output(state, "links") else {
        return false;
    };
    let Ok(links) = serde_json::from_str::<Vec<Value>>(links) else {
        return false;
    };
    let residual_link = links.iter().any(|link| {
        field(link, "ifname").as_str().is_none_or(|name| {
            name.starts_with("tap-fc") || name.starts_with("veth-h") || name.starts_with("veth-g")
        })
    });
    netns.trim().is_empty()
        && !residual_link
        && !mounts
            .lines()
            .any(|line| line.contains("/jailer/") || line.contains("/firecracker/"))
}

fn no_live_rooms(state: &Value) -> bool {
    let text = field(field(state, "rooms"), "stdout")
        .as_str()
        .unwrap_or("");
    let parsed: Value = serde_json::from_str(text).unwrap_or(Value::Null);
    field(field(state, "rooms"), "exit").as_i64() == Some(0)
        && field(&parsed, "rooms")
            .as_array()
            .is_some_and(Vec::is_empty)
        && field(state, "vmm").as_array().is_some_and(Vec::is_empty)
        && network_and_mounts_clear(state)
}

fn start_sampler(out: &Path) -> Result<(mpsc::Sender<()>, std::thread::JoinHandle<Result<()>>)> {
    let mut file = File::create(out.join("memory.ndjson"))?;
    let (tx, rx) = mpsc::channel();
    let task = std::thread::spawn(move || loop {
        let row = json!({"at": chrono::Utc::now(), "vmm": vmm_memory()?,
                "meminfo": fs::read_to_string("/proc/meminfo")?});
        serde_json::to_writer(&mut file, &row)?;
        file.write_all(b"\n")?;
        file.flush()?;
        if rx.recv_timeout(Duration::from_secs(1)) != Err(mpsc::RecvTimeoutError::Timeout) {
            return Ok(());
        }
    });
    Ok((tx, task))
}

fn inspect_result(directory: &Path, expected_patch: Option<&str>) -> Result<Value> {
    let result_path = directory.join("result.json");
    if !result_path.is_file() {
        return Ok(json!({"directory": directory, "execution_complete": false,
            "problem": "missing result.json"}));
    }
    let result: Value = serde_json::from_slice(&fs::read(&result_path)?)?;
    let patch = directory.join("result.patch");
    let patch_hash = patch.is_file().then(|| digest(&patch)).transpose()?;
    let complete = field(&result, "schema_version").as_u64() == Some(1)
        && field(&result, "status").as_str() == Some("succeeded")
        && field(&result, "exit_code").as_i64() == Some(0)
        && expected_patch.is_none_or(|expected| patch_hash.as_deref() == Some(expected));
    Ok(
        json!({"directory": directory, "execution_complete": complete,
        "result": result, "result_sha256": digest(&result_path)?, "patch_sha256": patch_hash}),
    )
}

fn receipts(out: &Path, expected_patch: Option<&str>) -> Result<Vec<Value>> {
    if !out.is_dir() {
        return Ok(Vec::new());
    }
    let mut entries = fs::read_dir(out)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(fs::DirEntry::file_name);
    entries
        .into_iter()
        .filter(|entry| entry.path().is_dir())
        .map(|entry| inspect_result(&entry.path(), expected_patch))
        .collect()
}

fn complete_count(rows: &[Value]) -> usize {
    rows.iter()
        .filter(|row| field(row, "execution_complete").as_bool() == Some(true))
        .count()
}

fn accepted(exit: Option<i32>, expected: usize, rows: &[Value], host: &Value) -> bool {
    exit == Some(0)
        && rows.len() == expected
        && complete_count(rows) == expected
        && no_live_rooms(host)
}

fn readiness_report(stdout: &Path, requested: u32) -> Value {
    let parsed = fs::read(stdout)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
    let Some(clones) = parsed
        .as_ref()
        .and_then(|value| value.get("clones"))
        .and_then(Value::as_array)
    else {
        return json!({"requested": requested, "samples": [], "unavailable": requested,
            "problem": "missing or invalid clone CLI JSON"});
    };
    let mut identities = std::collections::BTreeSet::new();
    let mut samples = Vec::new();
    if clones.len() > requested as usize {
        return json!({"requested": requested, "samples": [], "unavailable": requested,
            "problem": "more clone records than requested"});
    }
    for clone in clones {
        let Some(id) = field(clone, "room_id").as_str() else {
            continue;
        };
        if !identities.insert(id) {
            return json!({"requested": requested, "samples": [], "unavailable": requested,
                "problem": "duplicate clone identity"});
        }
        let timing = field(clone, "readiness");
        let ack = field(timing, "dispatch_to_resume_ack_seconds").as_f64();
        let ssh = field(timing, "dispatch_to_ssh_ready_seconds").as_f64();
        if let (Some(ack), Some(ssh)) = (ack, ssh) {
            if ack.is_finite() && ssh.is_finite() && ack >= 0.0 && ssh >= ack {
                samples.push(
                    json!({"room_id": id, "resume_ack_seconds": ack, "ssh_ready_seconds": ssh}),
                );
            }
        }
    }
    let measured = u32::try_from(samples.len()).unwrap_or(u32::MAX);
    json!({"origin": "per-clone dispatch, after shared preparation and network allocation",
        "includes": "restore and batch barrier before authenticated SSH probe; excludes shared preparation and admission",
        "requested": requested, "measured": measured, "unavailable": requested.saturating_sub(measured),
        "samples": samples})
}

fn run_batch(args: &Args, command: &str, count: u32, trial: u32) -> Result<Value> {
    let out = args.out.join(format!("clones-{count}-trial-{trial}"));
    fs::create_dir(&out)?;
    let output = out.join("out");
    let argv = vec![
        "clone".to_owned(),
        args.snapshot.display().to_string(),
        "--image".into(),
        args.image.display().to_string(),
        "--toolstore".into(),
        args.toolstore.display().to_string(),
        "-n".into(),
        count.to_string(),
        "--command".into(),
        command.into(),
        "--max-wall".into(),
        format!("{}s", args.wall_seconds),
        "--out".into(),
        output.display().to_string(),
        "--json".into(),
    ];
    write_json(&out.join("argv.json"), &json!(argv))?;
    write_json(&out.join("host-before.json"), &host_state(&args.rooms)?)?;
    let (stop, sampler) = start_sampler(&out)?;
    let started = Instant::now();
    let execution = Command::new(&args.rooms)
        .args(&argv)
        .stdout(Stdio::from(File::create(out.join("stdout.json"))?))
        .stderr(Stdio::from(File::create(out.join("host.log"))?))
        .status();
    let elapsed = started.elapsed().as_secs_f64();
    let _ = stop.send(());
    let sample_result = sampler
        .join()
        .map_err(|_| anyhow::anyhow!("memory sampler panicked"))?;
    let after = host_state(&args.rooms)?;
    write_json(&out.join("host-after.json"), &after)?;
    let rows = receipts(&output, args.expected_patch_sha256.as_deref())?;
    let exit = execution
        .as_ref()
        .ok()
        .and_then(std::process::ExitStatus::code);
    let valid =
        execution.is_ok() && sample_result.is_ok() && accepted(exit, count as usize, &rows, &after);
    let row = json!({"schema_version": 1, "count": count, "trial": trial,
        "cli_exit": exit, "spawn_error": execution.err().map(|e| e.to_string()),
        "sampling_error": sample_result.err().map(|e| e.to_string()),
        "elapsed_seconds": elapsed, "execution_complete_count": complete_count(&rows),
        "completed_per_minute": 60.0 * f64::from(u32::try_from(complete_count(&rows))?) / elapsed,
        "execution_valid": valid, "receipts": rows,
        "readiness": readiness_report(&out.join("stdout.json"), count),
        "interpretation": "Execution and exact-patch evidence; no semantic or security qualification inferred. Inspect retained network/mount audits separately."});
    write_json(&out.join("summary.json"), &row)?;
    Ok(row)
}

fn input_manifest(args: &Args) -> Result<Value> {
    let started = Instant::now();
    let mut hashes = BTreeMap::new();
    let paths = [
        args.rooms.clone(),
        args.image.clone(),
        args.command_file.clone(),
        args.toolstore.join("toolstore.sqfs"),
        args.toolstore.join("meta.json"),
        args.snapshot.join("snapshot.json"),
        args.snapshot.join("snapshot.vmstate"),
        args.snapshot.join("snapshot.mem"),
    ];
    for path in paths {
        hashes.insert(path.display().to_string(), digest(&path)?);
    }
    Ok(
        json!({"schema_version": 1, "created_at": chrono::Utc::now(),
        "artifacts_sha256": hashes, "input_hashing_seconds": started.elapsed().as_secs_f64(),
        "uname": observe("uname", &["-a"])?, "lscpu": observe("lscpu", &["--json"])?,
        "expected_patch_sha256": args.expected_patch_sha256, "counts": args.counts,
        "repeats": args.repeats, "wall_seconds": args.wall_seconds}),
    )
}

fn main() -> Result<()> {
    let args = Args::parse();
    ensure!(
        cfg!(target_os = "linux"),
        "run workloads on a Linux Rooms host"
    );
    ensure!(
        !args.counts.is_empty() && args.counts.iter().all(|n| *n > 0),
        "counts must be positive"
    );
    ensure!(
        args.repeats > 0 && args.wall_seconds > 0,
        "repeats and wall time must be positive"
    );
    let command = fs::read_to_string(&args.command_file)?;
    ensure!(!command.trim().is_empty(), "command file is empty");
    fs::create_dir(&args.out).context("output must be a new directory")?;
    let state = host_state(&args.rooms)?;
    write_json(&args.out.join("host-initial.json"), &state)?;
    ensure!(
        no_live_rooms(&state),
        "measurement requires an idle Rooms host"
    );
    write_json(&args.out.join("inputs.json"), &input_manifest(&args)?)?;
    let mut rows = Vec::new();
    for count in &args.counts {
        for trial in 0..args.repeats {
            let row = run_batch(&args, &command, *count, trial)?;
            let valid = field(&row, "execution_valid").as_bool() == Some(true);
            rows.push(row);
            write_json(
                &args.out.join("summary.json"),
                &json!({"schema_version": 1, "batches": rows}),
            )?;
            ensure!(
                valid,
                "batch incomplete; retained its evidence and stopped the ramp"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readiness_keeps_missing_attempts_in_denominator_and_rejects_fake_samples() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("stdout.json");
        assert_eq!(readiness_report(&path, 2)["unavailable"], 2);
        let ready = json!({"room_id":"a", "readiness":{
            "dispatch_to_resume_ack_seconds":1.0, "dispatch_to_ssh_ready_seconds":2.0}});
        write_json(&path, &json!({"clones":[ready]}))?;
        let report = readiness_report(&path, 2);
        assert_eq!(report["measured"], 1);
        assert_eq!(report["unavailable"], 1);
        write_json(&path, &json!({"clones":[ready, ready]}))?;
        assert_eq!(readiness_report(&path, 2)["unavailable"], 2);
        write_json(
            &path,
            &json!({"clones":[{"room_id":"a", "readiness":{
            "dispatch_to_resume_ack_seconds":2.0, "dispatch_to_ssh_ready_seconds":1.0}}]}),
        )?;
        assert_eq!(readiness_report(&path, 1)["unavailable"], 1);
        write_json(
            &path,
            &json!({"clones":[{"room_id":"a", "status":"exited", "exit_code":0}]}),
        )?;
        assert_eq!(readiness_report(&path, 1)["unavailable"], 1);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn replacing_a_report_preserves_existing_readers() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("summary.json");
        let old = json!({"batches": ["first"]});
        write_json(&path, &old)?;
        let previous_reader = File::open(&path)?;
        let new = json!({"batches": ["first", "second"]});
        write_json(&path, &new)?;
        assert_eq!(serde_json::from_reader::<_, Value>(previous_reader)?, old);
        assert_eq!(serde_json::from_reader::<_, Value>(File::open(path)?)?, new);
        Ok(())
    }

    #[test]
    fn replacing_a_closed_report_publishes_complete_json() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("summary.json");
        write_json(&path, &json!({"batches": ["first"]}))?;
        let new = json!({"batches": ["first", "second"]});
        write_json(&path, &new)?;
        assert_eq!(serde_json::from_reader::<_, Value>(File::open(path)?)?, new);
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn a_reader_blocking_replacement_preserves_the_previous_report() -> Result<()> {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("summary.json");
        let old = json!({"batches": ["first"]});
        write_json(&path, &old)?;
        // Deny delete sharing explicitly: Windows must refuse replacement.
        let reader = fs::OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(&path)?;
        assert!(write_json(&path, &json!({"batches": ["second"]})).is_err());
        assert_eq!(serde_json::from_reader::<_, Value>(reader)?, old);
        assert_eq!(serde_json::from_reader::<_, Value>(File::open(path)?)?, old);
        Ok(())
    }

    #[test]
    fn missing_result_is_not_success() -> Result<()> {
        let dir = tempfile::tempdir()?;
        assert_eq!(
            inspect_result(dir.path(), None)?["execution_complete"],
            false
        );
        Ok(())
    }

    #[test]
    fn success_requires_selected_patch_bytes() -> Result<()> {
        let dir = tempfile::tempdir()?;
        write_json(
            &dir.path().join("result.json"),
            &json!({"schema_version": 1, "status": "succeeded", "exit_code": 0}),
        )?;
        fs::write(dir.path().join("result.patch"), "expected")?;
        let hash = digest(&dir.path().join("result.patch"))?;
        assert_eq!(
            inspect_result(dir.path(), Some(&hash))?["execution_complete"],
            true
        );
        fs::write(dir.path().join("result.patch"), "substituted")?;
        assert_eq!(
            inspect_result(dir.path(), Some(&hash))?["execution_complete"],
            false
        );
        Ok(())
    }

    #[test]
    fn count_exit_and_liveness_are_independent() {
        let clean = json!({"vmm": [], "rooms": {"exit": 0, "stdout": "{\"rooms\":[]}"}, "netns":{"exit":0,"stdout":""},"links":{"exit":0,"stdout":"[]"},"mounts":{"exit":0,"stdout":"/"}});
        let rows = vec![json!({"execution_complete": true})];
        assert!(accepted(Some(0), 1, &rows, &clean));
        assert!(!accepted(Some(0), 2, &rows, &clean));
        assert!(!accepted(Some(124), 1, &rows, &clean));
        assert!(!accepted(None, 1, &rows, &clean));
        let unknown = json!({"vmm": [], "rooms": {"exit": 0, "stdout": "broken"}});
        assert!(!accepted(Some(0), 1, &rows, &unknown));
    }
}
