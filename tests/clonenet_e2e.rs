//! Host-only proof for the clone netns + veth substrate.
//!
//! Run on rooms-host as root after `scripts/setup-tap.sh --host`:
//! `sudo -E env HOME=$HOME cargo test --release --features e2e --test clonenet_e2e -- --nocapture`

#![cfg(all(target_os = "linux", feature = "e2e"))]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "host e2e test: setup failures are test failures"
)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use rooms::clonenet::{self, Claimer, CloneNet};
use rooms::error::CloneNetError;
use rooms::veth_isolation::{
    flat_chain_falls_through_for_veth, forward_jumps_ordered, rooms_veth_fwd_isolates,
    veth_input_drop_present,
};

const TARGETS: [u8; 3] = [51, 52, 53];
const GUEST_GATEWAY: &str = "172.16.0.21/30";
const GUEST_ADDRESS: &str = "172.16.0.22/30";

struct Cleanup {
    state: PathBuf,
    allocations: Vec<(CloneNet, String)>,
    guest_namespaces: Vec<String>,
    input_drops: Vec<String>,
    host_links: Vec<String>,
}

struct ForeignAllocation {
    state: PathBuf,
    net: CloneNet,
    owner: String,
}

impl Drop for ForeignAllocation {
    fn drop(&mut self) {
        let _ = clonenet::free(&self.state, self.net.index, &self.owner);
    }
}

impl Cleanup {
    fn new(state: &Path) -> Self {
        Self {
            state: state.to_owned(),
            allocations: Vec::new(),
            guest_namespaces: Vec::new(),
            input_drops: Vec::new(),
            host_links: Vec::new(),
        }
    }
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        for veth in &self.input_drops {
            best_effort("iptables", &["-D", "INPUT", "-i", veth, "-j", "DROP"]);
        }
        for namespace in &self.guest_namespaces {
            best_effort("ip", &["netns", "del", namespace]);
        }
        for (net, owner) in &self.allocations {
            let _ = clonenet::free(&self.state, net.index, owner);
        }
        for link in &self.host_links {
            best_effort("ip", &["link", "del", link]);
        }
        best_effort("ip", &["rule", "del", "priority", "10000"]);
        best_effort("ip", &["route", "flush", "table", "100"]);
    }
}

#[test]
fn three_clone_networks_are_routed_nat_isolated_and_reaped() {
    require_host_substrate();
    let state = tempfile::tempdir().unwrap();
    let me = Claimer::current().expect("Linux test process has a /proc starttime");
    let mut cleanup = Cleanup::new(state.path());
    best_effort("ip", &["rule", "del", "priority", "10000"]);
    best_effort("ip", &["route", "flush", "table", "100"]);
    assert_foreign_route_overlap_is_refused(state.path(), me, &mut cleanup);
    assert_policy_default_overlap_is_refused(state.path(), me, &mut cleanup);

    for (ordinal, target) in TARGETS.into_iter().enumerate() {
        let owner = owner_id(ordinal);
        let net = clonenet::allocate(state.path(), &owner, me, 63, Some(target)).unwrap();
        cleanup.allocations.push((net.clone(), owner));
        setup_guest_side(&net, &mut cleanup);
    }

    assert_reused_guest_network(&cleanup.allocations);
    assert_foreign_state_collision_is_nondestructive(&cleanup.allocations);
    assert_foreign_untargeted_walk_skips_global_collisions(&cleanup.allocations);
    assert_reconcile_cleans_binding_after_runtime_state_loss();
    assert_bidirectional_veth_reachability(&cleanup.allocations);
    assert_source_binding_rules(&cleanup.allocations);
    assert_two_hop_upstream_reachability(&cleanup);
    assert_cross_clone_isolation(&cleanup.allocations);
    assert_spoofed_source_is_dropped(&cleanup.allocations);
    assert_none_input_posture(&mut cleanup);

    cleanup_all(&mut cleanup);
    assert_no_leaks(state.path());
}

fn assert_foreign_route_overlap_is_refused(state: &Path, me: Claimer, cleanup: &mut Cleanup) {
    let link = "rooms-conflict";
    best_effort("ip", &["link", "del", link]);
    run("ip", &["link", "add", link, "type", "dummy"]);
    cleanup.host_links.push(link.to_owned());
    run("ip", &["link", "set", link, "up"]);
    run("ip", &["route", "add", "172.17.0.0/16", "dev", link]);

    let error = clonenet::allocate(state, &owner_id(200), me, 63, Some(50)).unwrap_err();
    assert!(matches!(error, CloneNetError::RouteOverlap { .. }));
    assert!(!state.join("clonenets").join("50").exists());
    assert_eq!(
        std::fs::symlink_metadata("/run/rooms/clonenet-owners/50")
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::NotFound
    );
    run("ip", &["link", "del", link]);
}

fn assert_policy_default_overlap_is_refused(state: &Path, me: Claimer, cleanup: &mut Cleanup) {
    let link = "rooms-policy";
    best_effort("ip", &["rule", "del", "priority", "10000"]);
    best_effort("ip", &["route", "flush", "table", "100"]);
    best_effort("ip", &["link", "del", link]);
    run("ip", &["link", "add", link, "type", "dummy"]);
    cleanup.host_links.push(link.to_owned());
    run("ip", &["link", "set", link, "up"]);
    run(
        "ip",
        &["route", "add", "default", "dev", link, "table", "100"],
    );
    run(
        "ip",
        &[
            "rule",
            "add",
            "priority",
            "10000",
            "to",
            "172.17.0.0/24",
            "table",
            "100",
        ],
    );

    let error = clonenet::allocate(state, &owner_id(201), me, 63, Some(50)).unwrap_err();
    assert!(matches!(error, CloneNetError::PolicyRouting { .. }));
    assert!(!state.join("clonenets").join("50").exists());
    assert_eq!(
        std::fs::symlink_metadata("/run/rooms/clonenet-owners/50")
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::NotFound
    );
    run("ip", &["rule", "del", "priority", "10000"]);
    run("ip", &["route", "flush", "table", "100"]);
    run("ip", &["link", "del", link]);
}

fn require_host_substrate() {
    assert_eq!(run_text("id", &["-u"]), "0", "test must run as root");
    let forward = run_text("iptables", &["-S", "FORWARD"]);
    let chain = run_text("iptables", &["-S", "ROOMS_VETH_FWD"]);
    let flat_chain = run_text("iptables", &["-S", "ROOMS_FWD"]);
    assert!(
        forward_jumps_ordered(&forward),
        "veth FORWARD jump is not second:\n{forward}"
    );
    assert!(
        rooms_veth_fwd_isolates(&chain),
        "veth chain is incomplete:\n{chain}"
    );
    assert!(
        flat_chain_falls_through_for_veth(&flat_chain),
        "flat chain can terminate clone-veth traffic:\n{flat_chain}"
    );
}

fn owner_id(ordinal: usize) -> String {
    format!("{:026}", 9_000 + ordinal)
}

fn guest_namespace(net: &CloneNet) -> String {
    format!("rooms-gt{}", net.index)
}

fn setup_guest_side(net: &CloneNet, cleanup: &mut Cleanup) {
    let guest_ns = guest_namespace(net);
    run("ip", &["netns", "add", &guest_ns]);
    cleanup.guest_namespaces.push(guest_ns.clone());
    run(
        "ip",
        &[
            "-n", &net.netns, "link", "add", "tap-fc5", "type", "veth", "peer", "name", "guest0",
        ],
    );
    run(
        "ip",
        &[
            "-n", &net.netns, "link", "set", "guest0", "netns", &guest_ns,
        ],
    );
    run(
        "ip",
        &[
            "-n",
            &net.netns,
            "addr",
            "add",
            GUEST_GATEWAY,
            "dev",
            "tap-fc5",
        ],
    );
    run("ip", &["-n", &net.netns, "link", "set", "tap-fc5", "up"]);
    run(
        "ip",
        &[
            "netns",
            "exec",
            &net.netns,
            "sysctl",
            "-w",
            "net.ipv4.conf.tap-fc5.forwarding=1",
        ],
    );
    run(
        "ip",
        &[
            "-n",
            &guest_ns,
            "addr",
            "add",
            GUEST_ADDRESS,
            "dev",
            "guest0",
        ],
    );
    run("ip", &["-n", &guest_ns, "link", "set", "guest0", "up"]);
    run("ip", &["-n", &guest_ns, "link", "set", "lo", "up"]);
    run(
        "ip",
        &[
            "-n",
            &guest_ns,
            "route",
            "replace",
            "default",
            "via",
            "172.16.0.21",
        ],
    );
}

fn assert_reused_guest_network(allocations: &[(CloneNet, String)]) {
    for (net, _) in allocations {
        let tap = run_text("ip", &["-n", &net.netns, "-4", "addr", "show", "tap-fc5"]);
        assert!(
            tap.contains(GUEST_GATEWAY),
            "{} did not reuse the frozen gateway /30",
            net.netns
        );
        let guest = run_text(
            "ip",
            &["-n", &guest_namespace(net), "-4", "addr", "show", "guest0"],
        );
        assert!(
            guest.contains(GUEST_ADDRESS),
            "{} did not reuse the frozen guest /30",
            net.netns
        );
    }
}

fn assert_foreign_state_collision_is_nondestructive(allocations: &[(CloneNet, String)]) {
    let (first, first_owner) = &allocations[0];
    let marker = Path::new("/run/rooms/clonenet-owners").join(first.index.to_string());
    assert_eq!(std::fs::read_link(&marker).unwrap(), Path::new(first_owner));
    let foreign_state = tempfile::tempdir().unwrap();
    let me = Claimer::current().expect("Linux test process has a /proc starttime");
    let result = clonenet::allocate(
        foreign_state.path(),
        &owner_id(99),
        me,
        63,
        Some(first.index),
    );
    assert!(result.is_err(), "host-global namespace collision must fail");
    assert_eq!(std::fs::read_link(&marker).unwrap(), Path::new(first_owner));
    let namespaces = run_text("ip", &["netns", "list"]);
    assert!(
        namespaces.contains(&first.netns),
        "foreign state root tore down the live namespace"
    );
    assert!(!foreign_state
        .path()
        .join("clonenets")
        .join(first.index.to_string())
        .exists());
}

fn assert_foreign_untargeted_walk_skips_global_collisions(allocations: &[(CloneNet, String)]) {
    let first = allocations.first().unwrap().0.index;
    let expected = allocations.last().unwrap().0.index + 1;
    let foreign_state = tempfile::tempdir().unwrap();
    let claims = foreign_state.path().join("clonenets");
    std::fs::create_dir_all(&claims).unwrap();
    for index in 1..first {
        std::fs::write(claims.join(index.to_string()), "reservation\n").unwrap();
    }
    let owner = owner_id(100);
    let me = Claimer::current().expect("Linux test process has a /proc starttime");
    let net = clonenet::allocate(foreign_state.path(), &owner, me, 63, None).unwrap();
    let cleanup = ForeignAllocation {
        state: foreign_state.path().to_path_buf(),
        net: net.clone(),
        owner: owner.clone(),
    };
    assert_eq!(net.index, expected);
    assert_eq!(
        clonenet::free(foreign_state.path(), net.index, &owner).unwrap(),
        clonenet::Freed::Removed
    );
    assert!(!run_text("ip", &["netns", "list"]).contains(&net.netns));
    let marker = Path::new("/run/rooms/clonenet-owners").join(net.index.to_string());
    assert_eq!(
        std::fs::symlink_metadata(marker).unwrap_err().kind(),
        std::io::ErrorKind::NotFound
    );
    drop(cleanup);
}

fn assert_reconcile_cleans_binding_after_runtime_state_loss() {
    let state = tempfile::tempdir().unwrap();
    let owner = owner_id(101);
    let dead = Claimer {
        pid: 4_194_305,
        starttime: 1,
    };
    let net = clonenet::allocate(state.path(), &owner, dead, 63, Some(55)).unwrap();
    let cleanup = ForeignAllocation {
        state: state.path().to_path_buf(),
        net: net.clone(),
        owner,
    };
    let marker = Path::new("/run/rooms/clonenet-owners").join(net.index.to_string());
    std::fs::remove_file(marker).unwrap();
    run("ip", &["netns", "del", &net.netns]);
    delete_link_if_present(&net.veth_host);
    delete_link_if_present(&net.veth_guest);
    let binding = format!(
        "-A ROOMS_VETH_FWD ! -s {}/32 -i {} -j DROP",
        net.netns_ip, net.veth_host
    );
    assert!(run_text("iptables", &["-S", "ROOMS_VETH_FWD"]).contains(&binding));
    let reclaimed = clonenet::reconcile(state.path());
    assert_eq!(reclaimed.len(), 1);
    assert!(reclaimed[0].removed);
    assert!(!run_text("iptables", &["-S", "ROOMS_VETH_FWD"]).contains(&binding));
    let marker = Path::new("/run/rooms/clonenet-owners").join(net.index.to_string());
    assert_eq!(
        std::fs::symlink_metadata(marker).unwrap_err().kind(),
        std::io::ErrorKind::NotFound
    );
    assert!(!state
        .path()
        .join("clonenets")
        .join(net.index.to_string())
        .exists());
    drop(cleanup);
}

fn delete_link_if_present(interface: &str) {
    if output("ip", &["link", "show", interface]).status.success() {
        run("ip", &["link", "del", interface]);
    }
    expect_failure("ip", &["link", "show", interface]);
}

fn assert_bidirectional_veth_reachability(allocations: &[(CloneNet, String)]) {
    for (net, _) in allocations {
        run("ping", &["-c", "1", "-W", "2", &net.netns_ip.to_string()]);
        run(
            "ip",
            &[
                "netns",
                "exec",
                &net.netns,
                "ping",
                "-c",
                "1",
                "-W",
                "2",
                &net.host_ip.to_string(),
            ],
        );
        let route = run_text(
            "ip",
            &["route", "show", &format!("172.17.0.{}/30", 4 * net.index)],
        );
        assert!(
            route.contains(&net.veth_host),
            "host reverse /30 route is missing: {route}"
        );
    }
}

fn assert_source_binding_rules(allocations: &[(CloneNet, String)]) {
    let chain = run_text("iptables", &["-S", "ROOMS_VETH_FWD"]);
    assert!(
        rooms_veth_fwd_isolates(&chain),
        "dynamic source bindings invalidate the chain:\n{chain}"
    );
    for (net, _) in allocations {
        let expected = format!(
            "-A ROOMS_VETH_FWD ! -s {}/32 -i {} -j DROP",
            net.netns_ip, net.veth_host
        );
        assert!(
            chain.contains(&expected),
            "missing per-veth source binding: {expected}\n{chain}"
        );
    }
}

fn assert_two_hop_upstream_reachability(cleanup: &Cleanup) {
    for (net, _) in &cleanup.allocations {
        run(
            "ip",
            &[
                "netns",
                "exec",
                &guest_namespace(net),
                "ping",
                "-c",
                "2",
                "-W",
                "3",
                "1.1.1.1",
            ],
        );
    }
}

fn assert_cross_clone_isolation(allocations: &[(CloneNet, String)]) {
    for (source, _) in allocations {
        for (target, _) in allocations {
            if source.index == target.index {
                continue;
            }
            expect_failure(
                "ip",
                &[
                    "netns",
                    "exec",
                    &guest_namespace(source),
                    "ping",
                    "-c",
                    "1",
                    "-W",
                    "1",
                    &target.netns_ip.to_string(),
                ],
            );
        }
    }
}

fn assert_spoofed_source_is_dropped(allocations: &[(CloneNet, String)]) {
    let mut nets = allocations.iter().map(|(net, _)| net);
    let source = nets.next().expect("three allocations");
    let target = nets.next().expect("three allocations");
    let source_ns = guest_namespace(source);
    run(
        "ip",
        &[
            "-n",
            &source_ns,
            "addr",
            "add",
            "192.0.2.1/32",
            "dev",
            "guest0",
        ],
    );
    run(
        "ip",
        &[
            "netns",
            "exec",
            &target.netns,
            "iptables",
            "-I",
            "INPUT",
            "1",
            "-s",
            "192.0.2.1/32",
            "-j",
            "ACCEPT",
        ],
    );
    expect_failure(
        "ip",
        &[
            "netns",
            "exec",
            &source_ns,
            "ping",
            "-I",
            "192.0.2.1",
            "-c",
            "1",
            "-W",
            "1",
            &target.netns_ip.to_string(),
        ],
    );
    let counter = run_text(
        "ip",
        &[
            "netns",
            "exec",
            &target.netns,
            "iptables",
            "-L",
            "INPUT",
            "1",
            "-v",
            "-n",
            "-x",
        ],
    );
    let packets: u64 = counter
        .lines()
        .last()
        .and_then(|line| line.split_whitespace().next())
        .and_then(|value| value.parse().ok())
        .expect("INPUT rule packet counter");
    assert_eq!(packets, 0, "spoofed packet reached target namespace");

    let forged = target.netns_ip.to_string();
    run(
        "ip",
        &[
            "-n",
            &source_ns,
            "addr",
            "add",
            &format!("{forged}/32"),
            "dev",
            "guest0",
        ],
    );
    let allowed = source.netns_ip.to_string();
    let before = source_binding_packets(&source.veth_host, &allowed);
    expect_failure(
        "ip",
        &[
            "netns", "exec", &source_ns, "ping", "-I", &forged, "-c", "1", "-W", "1", "1.1.1.1",
        ],
    );
    let after = source_binding_packets(&source.veth_host, &allowed);
    assert!(
        after > before,
        "in-pool forged source did not hit {}'s binding DROP",
        source.veth_host
    );
}

fn source_binding_packets(veth: &str, allowed_source: &str) -> u64 {
    let rules = run_text(
        "iptables",
        &["-L", "ROOMS_VETH_FWD", "-v", "-n", "-x", "--line-numbers"],
    );
    rules
        .lines()
        .find(|line| line.contains(veth) && line.contains(allowed_source))
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse().ok())
        .expect("per-veth source-binding packet counter")
}

fn assert_none_input_posture(cleanup: &mut Cleanup) {
    let net = &cleanup.allocations[0].0;
    run(
        "iptables",
        &["-I", "INPUT", "1", "-i", &net.veth_host, "-j", "DROP"],
    );
    cleanup.input_drops.push(net.veth_host.clone());
    let input = run_text("iptables", &["-S", "INPUT"]);
    assert!(
        veth_input_drop_present(&input, &net.veth_host),
        "none INPUT drop not provable:\n{input}"
    );
    expect_failure(
        "ip",
        &[
            "netns",
            "exec",
            &net.netns,
            "ping",
            "-c",
            "1",
            "-W",
            "1",
            &net.host_ip.to_string(),
        ],
    );
}

fn cleanup_all(cleanup: &mut Cleanup) {
    for veth in cleanup.input_drops.drain(..) {
        run("iptables", &["-D", "INPUT", "-i", &veth, "-j", "DROP"]);
    }
    for namespace in cleanup.guest_namespaces.drain(..) {
        run("ip", &["netns", "del", &namespace]);
    }
    for (net, owner) in cleanup.allocations.drain(..) {
        assert_eq!(
            clonenet::free(&cleanup.state, net.index, &owner).unwrap(),
            clonenet::Freed::Removed
        );
    }
}

fn assert_no_leaks(state: &Path) {
    let namespaces = run_text("ip", &["netns", "list"]);
    for target in TARGETS {
        assert!(!namespaces.contains(&format!("rooms-c{target}")));
        assert!(!namespaces.contains(&format!("rooms-gt{target}")));
        assert!(!output("ip", &["link", "show", &format!("veth-h{target}")])
            .status
            .success());
        assert!(!output("ip", &["link", "show", &format!("veth-g{target}")])
            .status
            .success());
        assert!(!state.join("clonenets").join(target.to_string()).exists());
        let marker = Path::new("/run/rooms/clonenet-owners").join(target.to_string());
        assert_eq!(
            std::fs::symlink_metadata(marker).unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );
    }
}

fn run(program: &str, args: &[&str]) {
    let result = output(program, args);
    assert!(
        result.status.success(),
        "command failed: {program} {}\nstdout: {}\nstderr: {}",
        args.join(" "),
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}

fn expect_failure(program: &str, args: &[&str]) {
    let result = output(program, args);
    assert!(
        !result.status.success(),
        "command unexpectedly succeeded: {program} {}",
        args.join(" ")
    );
}

fn run_text(program: &str, args: &[&str]) -> String {
    let result = output(program, args);
    assert!(
        result.status.success(),
        "command failed: {program} {}",
        args.join(" ")
    );
    String::from_utf8(result.stdout).unwrap().trim().to_owned()
}

fn output(program: &str, args: &[&str]) -> Output {
    Command::new(program).args(args).output().unwrap()
}

fn best_effort(program: &str, args: &[&str]) {
    let _ = Command::new(program).args(args).output();
}
