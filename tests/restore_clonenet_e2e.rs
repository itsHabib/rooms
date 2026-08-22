//! Privileged one-clone restore gate for the namespace-aware clone path.
//!
//! This is deliberately strict: when selected on a Linux `rooms-host`, missing
//! KVM/images/firewall substrate is a failure rather than a skip. It creates one
//! real neutral base and Full snapshot, drives two sequential count-one modes
//! through the real `rooms clone` CLI (integrated witnessed command, then kept
//! owner handoff), and proves the live and terminal host invariants.
//!
//! Run on `rooms-host`:
//! `sudo -E env HOME=$HOME cargo test --features e2e --test restore_clonenet_e2e -- --test-threads=1 --nocapture`
//! Set `ROOMS_E2E_ROOTFS=/absolute/image.ext4` to select a freshly baked proof
//! image without overwriting either conventional image under `~/rooms/images`.

#![cfg(all(target_os = "linux", feature = "e2e"))]
#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used,
    reason = "privileged integration test: assertions and direct fixture indexing keep failures legible"
)]

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::{symlink, MetadataExt as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use anyhow::{ensure, Context as _, Result};
use rooms::artifacts;
use rooms::clonenet::{self, CloneNet};
use rooms::config::RoomsConfig;
use rooms::room;
use rooms::runner::{self, GuestTarget, Runner};
use serde::Deserialize;

const CLONE_INDEX: u8 = 1;
const INTERNET_TARGET: &str = "1.1.1.1";
const WORKLOAD_MARKER: &str = "ROOMS_CLONENET_E2E";

#[derive(Debug)]
struct HostPaths {
    rootfs: PathBuf,
    key: PathBuf,
}

struct ProcessBaseline {
    firecracker: BTreeSet<u32>,
    tcpdump: BTreeSet<u32>,
}

#[derive(Debug, Deserialize)]
struct BaseRecord {
    room_id: String,
    slot: u8,
}

#[derive(Debug, Deserialize)]
struct SnapshotRecord {
    snapshot_id: String,
    slot: u8,
    guest_ip: String,
}

#[derive(Debug, Deserialize)]
struct CloneBatch {
    clones: Vec<CloneRecord>,
}

#[derive(Debug, Deserialize)]
struct CloneRecord {
    room_id: String,
    snapshot_id: String,
    slot: u8,
    guest_ip: String,
    clone_net_index: u8,
    namespace: String,
    host_veth: String,
    status: String,
    exit_code: Option<u8>,
    out_dir: Option<PathBuf>,
}

/// Assertion-failure backstop. It operates only inside this test's temporary
/// HOME, but tears down host-global resources through their exact room owners.
struct StateCleanup {
    home: PathBuf,
    state: PathBuf,
    armed: bool,
}

/// The strict gate uses a temporary snapshot, while production snapshots are
/// deliberately published with `FS_IMMUTABLE_FL`. Clear only this gate-owned
/// artifact set before `TempDir` removes its proof tree.
struct SnapshotArtifactCleanup {
    directory: PathBuf,
}

impl SnapshotArtifactCleanup {
    const fn new(directory: PathBuf) -> Self {
        Self { directory }
    }
}

impl Drop for SnapshotArtifactCleanup {
    fn drop(&mut self) {
        for name in ["snapshot.vmstate", "snapshot.mem", "snapshot.json"] {
            let _ = Command::new("chattr")
                .args(["-i", "--"])
                .arg(self.directory.join(name))
                .output();
        }
        let _ = Command::new("chattr")
            .args(["-i", "--"])
            .arg(&self.directory)
            .output();
    }
}

impl StateCleanup {
    fn new(home: PathBuf) -> Self {
        Self {
            state: home.join(".local/state/rooms"),
            home,
            armed: true,
        }
    }

    const fn disarm(&mut self) {
        self.armed = false;
    }

    fn recover_snapshots(&self) {
        let output = rooms_command(&self.home)
            .args(["snapshot-recover", "--json"])
            .output();
        let Ok(output) = output else {
            return;
        };
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(&output.stdout) else {
            return;
        };
        let Some(pending) = value.get("pending").and_then(serde_json::Value::as_array) else {
            return;
        };
        for item in pending {
            let Some(id) = item.get("snapshot_id").and_then(serde_json::Value::as_str) else {
                continue;
            };
            let _ = rooms_command(&self.home)
                .args(["snapshot-recover", id, "--json"])
                .output();
        }
    }

    fn room_ids(&self) -> Vec<String> {
        let Ok(entries) = fs::read_dir(&self.state) else {
            return Vec::new();
        };
        entries
            .filter_map(std::result::Result::ok)
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| rooms::registry::is_valid_room_id(name))
            .collect()
    }

    fn clean(&self) {
        self.recover_snapshots();
        for id in self.room_ids() {
            let _ = rooms_command(&self.home)
                .args(["kill", &id, "--json"])
                .output();
            let _ = rooms_command(&self.home).args(["gc", &id]).output();
        }
        let _ = rooms_command(&self.home).arg("gc").output();
        let _ = clonenet::reconcile(&self.state);
    }
}

impl Drop for StateCleanup {
    fn drop(&mut self) {
        if self.armed {
            self.clean();
        }
    }
}

fn rooms_command(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rooms"));
    command.env("HOME", home);
    command
}

fn command(program: &str, args: &[&str]) -> Output {
    Command::new(program)
        .args(args)
        .output()
        .unwrap_or_else(|error| panic!("spawn {program} {args:?}: {error}"))
}

fn require_success(label: &str, output: &Output) -> Result<()> {
    ensure!(
        output.status.success(),
        "{label} failed ({}):\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

fn snapshot_rootfs(images: &Path) -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("ROOMS_E2E_ROOTFS") {
        let path = PathBuf::from(path);
        rooms::rootfs::validate_snapshot_base_image(&path).map_err(anyhow::Error::msg)?;
        return Ok(path);
    }
    [images.join("rootfs.ext4"), images.join("agent-alpine.ext4")]
        .into_iter()
        .find(|candidate| {
            candidate.is_file()
                && rooms::rootfs::validate_snapshot_base_image(candidate).is_ok()
        })
        .context(
            "no snapshot-capable rootfs found (set ROOMS_E2E_ROOTFS to an image with /sbin/overlay-init and no baked host key)",
        )
}

fn preflight() -> Result<HostPaths> {
    ensure!(
        String::from_utf8_lossy(&command("id", &["-u"]).stdout).trim() == "0",
        "this strict gate requires root; run it through `sudo -E`"
    );
    ensure!(Path::new("/dev/kvm").exists(), "/dev/kvm is missing");
    for binary in [
        "debugfs",
        "chattr",
        "firecracker",
        "jailer",
        "ip",
        "iptables",
        "ssh",
        "tcpdump",
    ] {
        require_success(
            &format!("locate {binary}"),
            &command("sh", &["-c", &format!("command -v {binary}")]),
        )?;
    }
    require_success(
        "ROOMS_FWD preflight",
        &command("iptables", &["-S", "ROOMS_FWD"]),
    )?;
    require_success(
        "ROOMS_VETH_FWD preflight",
        &command("iptables", &["-S", "ROOMS_VETH_FWD"]),
    )?;

    let home = PathBuf::from(std::env::var("HOME").context("HOME is unset")?);
    let images = home.join("rooms/images");
    let rootfs = snapshot_rootfs(&images)?;
    let kernel = images.join("vmlinux.bin");
    let key = home.join(".ssh/id_rooms");
    ensure!(kernel.is_file(), "missing kernel: {}", kernel.display());
    ensure!(key.is_file(), "missing guest key: {}", key.display());

    let net = CloneNet::derive(CLONE_INDEX)?;
    ensure!(
        !command("ip", &["netns", "list"])
            .stdout
            .windows(net.netns.len())
            .any(|part| part == net.netns.as_bytes()),
        "{} already exists; the serial gate requires clone index {CLONE_INDEX} free",
        net.netns
    );
    ensure!(
        !command("ip", &["link", "show", "dev", &net.veth_host])
            .status
            .success(),
        "{} already exists; the serial gate requires clone index {CLONE_INDEX} free",
        net.veth_host
    );
    ensure!(
        !Path::new(&format!("/run/rooms/clonenet-owners/{CLONE_INDEX}")).exists(),
        "clone owner marker {CLONE_INDEX} already exists"
    );
    ensure!(
        !command("ip", &["link", "show", "dev", "tap-fc1"])
            .status
            .success(),
        "tap-fc1 already exists; slot 1 is not free"
    );
    Ok(HostPaths { rootfs, key })
}

fn prepare_home(root: &Path, key: &Path) -> Result<PathBuf> {
    let home = root.join("home");
    let ssh = home.join(".ssh");
    fs::create_dir_all(&ssh)?;
    symlink(key, ssh.join("id_rooms"))?;
    Ok(home)
}

fn create_snapshot(home: &Path, rootfs: &Path, out: &Path) -> Result<SnapshotRecord> {
    let base = rooms_command(home)
        .arg("base-create")
        .arg("--image")
        .arg(rootfs)
        .args(["--max-pool", "1", "--json"])
        .output()?;
    require_success("rooms base-create", &base)?;
    let base: BaseRecord = serde_json::from_slice(&base.stdout).context("parse base JSON")?;
    ensure!(base.slot == 1, "base used slot {}, expected 1", base.slot);

    let snapshot = rooms_command(home)
        .arg("snapshot")
        .arg(&base.room_id)
        .arg("--out")
        .arg(out)
        .arg("--json")
        .output()?;
    require_success("rooms snapshot", &snapshot)?;
    let snapshot: SnapshotRecord =
        serde_json::from_slice(&snapshot.stdout).context("parse snapshot JSON")?;
    ensure!(
        snapshot.slot == 1,
        "snapshot froze slot {}, expected 1",
        snapshot.slot
    );
    ensure!(out.join("snapshot.json").is_file(), "snapshot.json missing");
    ensure!(out.join("snapshot.mem").is_file(), "snapshot.mem missing");
    ensure!(
        out.join("snapshot.vmstate").is_file(),
        "snapshot.vmstate missing"
    );
    Ok(snapshot)
}

fn restore_one_clone(home: &Path, rootfs: &Path, snapshot_dir: &Path) -> Result<CloneRecord> {
    let output = rooms_command(home)
        .arg("clone")
        .arg(snapshot_dir)
        .arg("--image")
        .arg(rootfs)
        .args([
            "--count",
            "1",
            "--max-pool",
            "1",
            "--egress",
            "none",
            "--json",
        ])
        .output()?;
    require_success("rooms clone -n 1", &output)?;
    let mut batch: CloneBatch =
        serde_json::from_slice(&output.stdout).context("parse clone batch JSON")?;
    ensure!(
        batch.clones.len() == 1,
        "clone batch was not exactly one room"
    );
    let clone = batch.clones.remove(0);
    ensure!(
        clone.clone_net_index == CLONE_INDEX,
        "unexpected clone-net index"
    );
    ensure!(clone.namespace == "rooms-c1", "unexpected namespace");
    ensure!(clone.status == "kept", "clone was not handed off live");
    Ok(clone)
}

fn run_integrated_witness_clone(
    home: &Path,
    rootfs: &Path,
    snapshot_dir: &Path,
    out_root: &Path,
) -> Result<CloneRecord> {
    let host_ip = CloneNet::derive(CLONE_INDEX)?.host_ip;
    let workload = format!(
        "internet=blocked; \
         if timeout 5 bash -c 'exec 3<>/dev/tcp/{INTERNET_TARGET}/80'; then internet=reached; fi; \
         host=blocked; \
         if timeout 5 bash -c 'exec 3<>/dev/tcp/{host_ip}/22'; then host=reached; fi; \
         printf '{WORKLOAD_MARKER}_INTEGRATED internet=%s host=%s\\n' \"$internet\" \"$host\"; \
         test \"$internet\" = blocked && test \"$host\" = blocked"
    );
    let output = rooms_command(home)
        .arg("clone")
        .arg(snapshot_dir)
        .arg("--image")
        .arg(rootfs)
        .args(["--count", "1", "--max-pool", "1", "--command"])
        .arg(workload)
        .arg("--out")
        .arg(out_root)
        .args(["--witness", "--egress", "none", "--json"])
        .output()?;
    require_success("integrated witnessed rooms clone -n 1", &output)?;
    let mut batch: CloneBatch =
        serde_json::from_slice(&output.stdout).context("parse witnessed clone JSON")?;
    ensure!(batch.clones.len() == 1, "witness batch was not one clone");
    let clone = batch.clones.remove(0);
    ensure!(clone.clone_net_index == CLONE_INDEX);
    ensure!(clone.status == "exited");
    ensure!(clone.exit_code == Some(0));
    let out = clone.out_dir.as_ref().context("clone out dir missing")?;
    ensure!(out == &out_root.join(&clone.room_id));
    let stdout = fs::read_to_string(out.join(artifacts::STDOUT_LOG))?;
    ensure!(stdout.contains(&format!(
        "{WORKLOAD_MARKER}_INTEGRATED internet=blocked host=blocked"
    )));
    let witness_raw = fs::read(out.join(artifacts::WITNESS_PCAP))?;
    ensure!(witness_raw.len() > 24, "integrated witness pcap is empty");
    let witness: artifacts::Witness =
        serde_json::from_slice(&fs::read(out.join(artifacts::WITNESS_JSON))?)?;
    ensure!(witness.capture_complete, "integrated witness is incomplete");
    ensure!(witness.egress_policy == artifacts::EgressPolicy::None);
    ensure!(witness.permitted.is_empty());
    ensure!(
        witness
            .destinations
            .iter()
            .any(|destination| destination.ip == INTERNET_TARGET),
        "integrated witness missed attempted destination: {witness:?}"
    );
    ensure!(
        witness
            .blocked
            .iter()
            .any(|destination| destination.ip == INTERNET_TARGET),
        "integrated witness did not classify attempted destination as blocked"
    );
    Ok(clone)
}

fn assert_topology(
    config: &RoomsConfig,
    clone: &CloneRecord,
) -> Result<(room::RoomMeta, CloneNet)> {
    let net = CloneNet::derive(clone.clone_net_index)?;
    ensure!(
        clone.host_veth == net.veth_host,
        "CLI host-veth identity drift"
    );
    let room_dir = config.room_dir(&clone.room_id).context("room dir")?;
    let meta = room::read(&room_dir)?.context("clone room metadata missing")?;
    let pid = meta.pid.context("clone pid missing")?;
    ensure!(meta.keep, "clone metadata is not kept");
    ensure!(meta.snapshot_lineage.as_deref() == Some(clone.snapshot_id.as_str()));
    ensure!(meta.clone_net_index == Some(clone.clone_net_index));

    let process_inode = fs::metadata(format!("/proc/{pid}/ns/net"))?.ino();
    let namespace_inode = fs::metadata(format!("/run/netns/{}", clone.namespace))?.ino();
    ensure!(
        process_inode == namespace_inode,
        "Firecracker pid {pid} is not in {} ({process_inode} != {namespace_inode})",
        clone.namespace
    );
    require_success(
        "tap inside clone namespace",
        &command(
            "ip",
            &["-n", &clone.namespace, "link", "show", "dev", "tap-fc1"],
        ),
    )?;
    ensure!(
        !command("ip", &["link", "show", "dev", "tap-fc1"])
            .status
            .success(),
        "tap-fc1 escaped into the host namespace"
    );
    require_success(
        "host veth",
        &command("ip", &["link", "show", "dev", &net.veth_host]),
    )?;
    require_success(
        "namespace veth",
        &command(
            "ip",
            &[
                "-n",
                &clone.namespace,
                "link",
                "show",
                "dev",
                &net.veth_guest,
            ],
        ),
    )?;
    Ok((meta, net))
}

fn ssh_guest(namespace: Option<&str>, guest: &str, key: &Path, remote: &str) -> Output {
    let mut command = namespace.map_or_else(
        || Command::new("ssh"),
        |namespace| {
            let mut command = Command::new("ip");
            command.args(["netns", "exec", namespace, "ssh"]);
            command
        },
    );
    command
        .arg("-i")
        .arg(key)
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "IdentitiesOnly=yes",
            "-o",
            "ConnectTimeout=3",
            "-o",
            "StrictHostKeyChecking=no",
            "-o",
            "UserKnownHostsFile=/dev/null",
            "-o",
            "LogLevel=ERROR",
        ])
        .arg(format!("rooms@{guest}"))
        .arg(remote)
        .output()
        .expect("spawn guest ssh")
}

fn drop_packets(chain: &str) -> Result<u64> {
    let output = command("iptables", &["-L", chain, "-v", "-n", "-x"]);
    require_success(&format!("read {chain} counters"), &output)?;
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| {
            let columns: Vec<&str> = line.split_whitespace().collect();
            (columns.get(2) == Some(&"DROP"))
                .then(|| columns.first()?.parse::<u64>().ok())
                .flatten()
        })
        .with_context(|| format!("no DROP counter in {chain}"))
}

fn input_drop_packets(veth: &str) -> Result<u64> {
    let output = command("iptables", &["-L", "INPUT", "-v", "-n", "-x"]);
    require_success("read INPUT counters", &output)?;
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| {
            let columns: Vec<&str> = line.split_whitespace().collect();
            (columns.get(2) == Some(&"DROP") && columns.get(5) == Some(&veth))
                .then(|| columns.first()?.parse::<u64>().ok())
                .flatten()
        })
        .with_context(|| format!("no INPUT DROP counter for {veth}"))
}

async fn exercise_scoped_workload(
    clone: &CloneRecord,
    net: &CloneNet,
    key: &Path,
    config: &RoomsConfig,
    artifact_dir: &Path,
) -> Result<()> {
    let target = GuestTarget::new(&clone.guest_ip, Some(&clone.namespace));
    runner::wait_for_ssh(target, key, config).await?;

    let chain = format!("ROOMS_CEG_{}", clone.clone_net_index);
    let egress_before = drop_packets(&chain)?;
    let input_before = input_drop_packets(&net.veth_host)?;
    let workload = format!(
        "internet=blocked; \
         if timeout 5 bash -c 'exec 3<>/dev/tcp/{INTERNET_TARGET}/80'; then internet=reached; fi; \
         host=blocked; \
         if timeout 5 bash -c 'exec 3<>/dev/tcp/{}/22'; then host=reached; fi; \
         printf '{WORKLOAD_MARKER} internet=%s host=%s\\n' \"$internet\" \"$host\"; \
         test \"$internet\" = blocked && test \"$host\" = blocked",
        net.host_ip
    );
    let outcome = runner::exec(target, key, &Runner::Command(workload), config).await?;
    ensure!(
        outcome.exit_code == 0,
        "blocked workload exited {}",
        outcome.exit_code
    );
    runner::collect_out_to_host(target, key, artifact_dir).await?;
    let stdout = fs::read_to_string(artifact_dir.join(artifacts::STDOUT_LOG))?;
    ensure!(
        stdout.contains(&format!("{WORKLOAD_MARKER} internet=blocked host=blocked")),
        "workload did not prove both blocks: {stdout:?}"
    );
    ensure!(
        drop_packets(&chain)? > egress_before,
        "egress DROP did not count"
    );
    ensure!(
        input_drop_packets(&net.veth_host)? > input_before,
        "host INPUT DROP did not count"
    );

    let scoped = ssh_guest(
        Some(&clone.namespace),
        &clone.guest_ip,
        key,
        "printf CLONENET_SCOPED_SSH",
    );
    require_success("namespace-scoped SSH", &scoped)?;
    ensure!(scoped.stdout == b"CLONENET_SCOPED_SSH");
    let direct = ssh_guest(None, &clone.guest_ip, key, "true");
    ensure!(
        !direct.status.success(),
        "direct host SSH unexpectedly reached clone {}",
        clone.room_id
    );
    Ok(())
}

fn process_set(name: &str) -> BTreeSet<u32> {
    let output = command("pgrep", &["-x", name]);
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.parse().ok())
        .collect()
}

fn assert_owner_exit_reconcile(
    home: &Path,
    state: &Path,
    clone: &CloneRecord,
    pid: u32,
) -> Result<()> {
    let reconciled = clonenet::reconcile(state);
    let kept = reconciled
        .iter()
        .find(|entry| entry.index == clone.clone_net_index && entry.owner_id == clone.room_id);
    ensure!(
        kept.is_some_and(|entry| !entry.removed),
        "dead CLI claimer was not fenced by the live room: {reconciled:?}"
    );
    let gc = rooms_command(home).arg("gc").output()?;
    require_success("rooms gc while clone is live", &gc)?;
    ensure!(
        state.join(&clone.room_id).is_dir(),
        "gc removed a live clone"
    );
    ensure!(
        Path::new(&format!("/proc/{pid}")).exists(),
        "gc killed live clone"
    );
    ensure!(
        state
            .join(clonenet::CLONENETS_DIR)
            .join(clone.clone_net_index.to_string())
            .is_file(),
        "gc/reconcile released a live clone claim"
    );
    Ok(())
}

fn wait_pid_gone(pid: u32) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Path::new(&format!("/proc/{pid}")).exists() {
        ensure!(
            Instant::now() < deadline,
            "Firecracker pid {pid} survived kill"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

fn assert_clone_resources_absent(
    state: &Path,
    clone: &CloneRecord,
    slot_before: &[u8],
) -> Result<()> {
    let net = CloneNet::derive(clone.clone_net_index)?;
    ensure!(!state.join(&clone.room_id).exists(), "room dir leaked");
    ensure!(
        !state
            .join("jailer/firecracker")
            .join(&clone.room_id)
            .exists(),
        "jailer dir leaked"
    );
    ensure!(
        !state
            .join("restore-intents")
            .join(format!("{}.json", clone.room_id))
            .exists(),
        "restore intent leaked"
    );
    ensure!(
        !state
            .join(clonenet::CLONENETS_DIR)
            .join(clone.clone_net_index.to_string())
            .exists(),
        "clone claim leaked"
    );
    ensure!(
        !Path::new(&format!("/run/netns/{}", clone.namespace)).exists(),
        "network namespace leaked"
    );
    ensure!(
        !Path::new(&format!(
            "/run/rooms/clonenet-owners/{}",
            clone.clone_net_index
        ))
        .exists(),
        "clone owner marker leaked"
    );
    ensure!(
        !command("ip", &["link", "show", "dev", &net.veth_host])
            .status
            .success(),
        "host veth leaked"
    );
    ensure!(
        !command(
            "iptables",
            &["-S", &format!("ROOMS_CEG_{}", clone.clone_net_index),]
        )
        .status
        .success(),
        "clone egress chain leaked"
    );
    let input = String::from_utf8_lossy(&command("iptables", &["-S", "INPUT"]).stdout).into_owned();
    ensure!(
        !input
            .lines()
            .any(|line| { line == format!("-A INPUT -i {} -j DROP", net.veth_host) }),
        "clone INPUT rule leaked"
    );
    let forward = String::from_utf8_lossy(&command("iptables", &["-S", "ROOMS_VETH_FWD"]).stdout)
        .into_owned();
    ensure!(
        !forward.lines().any(|line| line.contains(&net.veth_host)),
        "clone forward jump leaked"
    );
    ensure!(
        fs::read(state.join("slots").join(clone.slot.to_string()))? == slot_before,
        "snapshot reservation was not restored byte-identically"
    );
    Ok(())
}

fn kill_and_assert_clean(
    home: &Path,
    config: &RoomsConfig,
    clone: &CloneRecord,
    pid: u32,
    slot_before: &[u8],
    processes: &ProcessBaseline,
) -> Result<()> {
    let output = rooms_command(home)
        .args(["kill", &clone.room_id, "--json"])
        .output()?;
    require_success("rooms kill clone", &output)?;
    let report: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    ensure!(
        report["outcomes"][0]["disposition"] == "killed",
        "kill did not report killed: {report}"
    );
    wait_pid_gone(pid)?;

    let state = config.resolved_state_base().context("state base")?;
    assert_clone_resources_absent(&state, clone, slot_before)?;

    let gc = rooms_command(home).arg("gc").output()?;
    require_success("idempotent final rooms gc", &gc)?;
    let ls = rooms_command(home).args(["ls", "--json"]).output()?;
    require_success("final rooms ls", &ls)?;
    let listed = rooms::registry::parse_ls_report(&String::from_utf8(ls.stdout)?)
        .map_err(anyhow::Error::msg)?;
    ensure!(
        listed.is_clean(),
        "room registry leaked: {:?}",
        listed.rooms
    );
    ensure!(process_set("firecracker") == processes.firecracker);
    ensure!(process_set("tcpdump") == processes.tcpdump);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn one_snapshot_clone_is_namespaced_scoped_and_exactly_reaped() -> Result<()> {
    let host = preflight()?;
    let processes = ProcessBaseline {
        firecracker: process_set("firecracker"),
        tcpdump: process_set("tcpdump"),
    };
    let temp = tempfile::tempdir()?;
    let home = prepare_home(temp.path(), &host.key)?;
    let mut cleanup = StateCleanup::new(home.clone());
    let snapshot_dir = temp.path().join("snapshot");
    let _snapshot_cleanup = SnapshotArtifactCleanup::new(snapshot_dir.clone());
    let artifact_dir = temp.path().join("clone-out");
    fs::create_dir_all(&artifact_dir)?;

    let snapshot = create_snapshot(&home, &host.rootfs, &snapshot_dir)?;
    let state = home.join(".local/state/rooms");
    let slot_file = state.join("slots").join(snapshot.slot.to_string());
    let slot_before = fs::read(&slot_file)?;

    // Command mode is the integrated pre-Resume custody proof: production
    // restore starts tcpdump in the namespace before loading/resuming the VM,
    // then persists the pcap + policy before its automatic exact teardown.
    let integrated_out = temp.path().join("integrated-witness");
    let integrated =
        run_integrated_witness_clone(&home, &host.rootfs, &snapshot_dir, &integrated_out)?;
    ensure!(integrated.snapshot_id == snapshot.snapshot_id);
    ensure!(integrated.slot == snapshot.slot);
    ensure!(integrated.guest_ip == snapshot.guest_ip);
    assert_clone_resources_absent(&state, &integrated, &slot_before)?;
    ensure!(process_set("firecracker") == processes.firecracker);
    ensure!(process_set("tcpdump") == processes.tcpdump);

    // Kept mode makes the actual allocating CLI exit while Firecracker stays
    // alive. This is the allocator-reconcile ownership-fence case.
    let clone = restore_one_clone(&home, &host.rootfs, &snapshot_dir)?;
    ensure!(clone.snapshot_id == snapshot.snapshot_id);
    ensure!(clone.slot == snapshot.slot);
    ensure!(clone.guest_ip == snapshot.guest_ip);

    let config = RoomsConfig {
        state_base: Some(state.clone()),
        ..RoomsConfig::default()
    };
    let (meta, net) = assert_topology(&config, &clone)?;
    let pid = meta.pid.context("clone pid")?;
    exercise_scoped_workload(&clone, &net, &host.key, &config, &artifact_dir).await?;
    assert_owner_exit_reconcile(&home, &state, &clone, pid)?;
    kill_and_assert_clean(&home, &config, &clone, pid, &slot_before, &processes)?;
    cleanup.disarm();
    Ok(())
}
