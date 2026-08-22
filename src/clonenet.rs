//! Per-clone host-network identity allocation.
//!
//! Every clone reuses its snapshot's frozen guest `/30` inside a private
//! network namespace. This allocator supplies the independent host-side axis:
//! one namespace and one veth `/30` from `172.17.0.0/24` per live clone. Claim
//! files share the slot allocator's crash-safe indexed-claim engine; allocation
//! of the namespace and veth mechanism is layered on that durable ownership.
//!
//! The `none` egress posture must also install an INPUT drop for the host-side
//! veth. This module owns the veth identity; restore integration installs that
//! posture before resume in the follow-on clone-path task.

use std::net::Ipv4Addr;
use std::path::Path;
#[cfg(target_os = "linux")]
use std::path::PathBuf;
#[cfg(target_os = "linux")]
use std::process::Command;

use crate::error::CloneNetError;
pub use crate::indexed_claim::Claimer;
use crate::indexed_claim::{
    self, ClaimDecision, ClaimOutcome, ClaimSpec, FreeOutcome, Pool, ReconcileAction, ReleaseError,
};

pub const CLONENETS_DIR: &str = "clonenets";
const FREE_LOCK: &str = "clonenets.lock";
pub const MAX_CLONENET: u8 = 63;
pub const DEFAULT_MAX_POOL: u8 = 8;

const CLONENET_POOL: Pool = Pool {
    dir_name: CLONENETS_DIR,
    lock_name: FREE_LOCK,
    max_index: MAX_CLONENET,
    label: "clone network",
};

/// One clone's distinct host-side namespace and veth identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloneNet {
    pub index: u8,
    pub netns: String,
    pub veth_host: String,
    pub veth_guest: String,
    pub host_ip: Ipv4Addr,
    pub netns_ip: Ipv4Addr,
    pub prefix: u8,
}

impl CloneNet {
    /// Derive the host identity for one claimable `/30` index.
    pub fn derive(index: u8) -> Result<Self, CloneNetError> {
        if !indexed_claim::valid_index(CLONENET_POOL, index) {
            return Err(CloneNetError::InvalidIndex {
                index,
                max: MAX_CLONENET,
            });
        }
        let base = 4 * index;
        Ok(Self {
            index,
            netns: format!("rooms-c{index}"),
            veth_host: format!("veth-h{index}"),
            veth_guest: format!("veth-g{index}"),
            host_ip: Ipv4Addr::new(172, 17, 0, base + 1),
            netns_ip: Ipv4Addr::new(172, 17, 0, base + 2),
            prefix: 30,
        })
    }
}

/// Claim one host-network identity without creating its Linux devices yet.
///
/// Durable ownership commits before lifecycle commands run, so a crash can be
/// reconciled from the claim file even when namespace creation was partial.
#[cfg(test)]
fn claim_identity(
    state: &Path,
    owner_id: &str,
    me: Claimer,
    cap: u8,
    target: Option<u8>,
) -> Result<CloneNet, CloneNetError> {
    map_claim(indexed_claim::claim(
        state,
        CLONENET_POOL,
        owner_id,
        me,
        cap,
        target,
    )?)
}

fn claim_host_identity(
    request: ClaimRequest<'_>,
    ops: &mut impl NetworkOps,
) -> Result<CloneNet, CloneNetError> {
    let spec = ClaimSpec {
        state: request.state,
        pool: CLONENET_POOL,
        owner_id: request.owner_id,
        me: request.me,
        cap: request.cap,
        target: request.target,
    };
    let outcome =
        indexed_claim::claim_with(spec, |index| ops.claim_global(index, request.owner_id))?;
    map_claim(outcome)
}

fn map_claim(outcome: ClaimOutcome) -> Result<CloneNet, CloneNetError> {
    match outcome {
        ClaimOutcome::Claimed(index) => CloneNet::derive(index),
        ClaimOutcome::PoolFull { cap } => Err(CloneNetError::PoolFull { cap }),
        ClaimOutcome::InvalidIndex { index, max } => {
            Err(CloneNetError::InvalidIndex { index, max })
        }
        ClaimOutcome::TargetTaken { index } => Err(CloneNetError::TargetTaken { index }),
    }
}

/// Result of releasing a clone-network claim by exact owner identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freed {
    Removed,
    AlreadyFree,
    AlreadyReassigned,
}

fn release_claim(state: &Path, index: u8, expected_owner: &str) -> Result<Freed, CloneNetError> {
    if !indexed_claim::valid_index(CLONENET_POOL, index) {
        return Err(CloneNetError::InvalidIndex {
            index,
            max: MAX_CLONENET,
        });
    }
    match indexed_claim::free(state, CLONENET_POOL, index, expected_owner)? {
        FreeOutcome::Removed => Ok(Freed::Removed),
        FreeOutcome::AlreadyFree => Ok(Freed::AlreadyFree),
        FreeOutcome::AlreadyReassigned => Ok(Freed::AlreadyReassigned),
    }
}

/// One dead clone-network claim processed by [`reconcile`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reclaimed {
    pub index: u8,
    pub owner_id: String,
    pub removed: bool,
}

/// Claim and create one complete Linux network namespace and veth path.
pub fn allocate(
    state: &Path,
    owner_id: &str,
    me: Claimer,
    cap: u8,
    target: Option<u8>,
) -> Result<CloneNet, CloneNetError> {
    let request = ClaimRequest {
        state,
        owner_id,
        me,
        cap,
        target,
    };
    allocate_with(request, &mut SystemNetworkOps)
}

/// Tear down an exact owner's namespace and release its durable claim.
pub fn free(state: &Path, index: u8, expected_owner: &str) -> Result<Freed, CloneNetError> {
    let net = CloneNet::derive(index)?;
    let mut ops = SystemNetworkOps;
    let result =
        indexed_claim::free_with(state, CLONENET_POOL, index, expected_owner, || {
            match ops.teardown(&net, expected_owner).map_err(cleanup_detail)? {
                ReconcileAction::Remove => Ok(()),
                ReconcileAction::Keep => {
                    Err("host resources are owned by another claim".to_owned())
                }
            }
        });
    map_release(result)
}

fn cleanup_detail(error: CloneNetError) -> String {
    match error {
        CloneNetError::Command { command, detail } => format!("{command}: {detail}"),
        other => other.to_string(),
    }
}

/// Reclaim confirmed-dead allocations through the shared indexed reconciler.
/// Cleanup runs under the free-lock before the claim is removed.
pub fn reconcile(state: &Path) -> Vec<Reclaimed> {
    let mut ops = SystemNetworkOps;
    reconcile_with(state, &mut ops)
}

fn reconcile_with(state: &Path, ops: &mut impl NetworkOps) -> Vec<Reclaimed> {
    indexed_claim::reconcile(state, CLONENET_POOL, |index, owner| {
        let net = CloneNet::derive(index).map_err(|error| error.to_string())?;
        ops.teardown(&net, owner).map_err(|error| error.to_string())
    })
    .into_iter()
    .map(|entry| Reclaimed {
        index: entry.index,
        owner_id: entry.owner_id,
        removed: entry.removed,
    })
    .collect()
}

fn map_release(result: Result<FreeOutcome, ReleaseError>) -> Result<Freed, CloneNetError> {
    match result {
        Ok(FreeOutcome::Removed) => Ok(Freed::Removed),
        Ok(FreeOutcome::AlreadyFree) => Ok(Freed::AlreadyFree),
        Ok(FreeOutcome::AlreadyReassigned) => Ok(Freed::AlreadyReassigned),
        Err(ReleaseError::Io(error)) => Err(CloneNetError::Io(error)),
        Err(ReleaseError::Cleanup(detail)) => Err(CloneNetError::Command {
            command: "clone network teardown".to_owned(),
            detail,
        }),
    }
}

#[derive(Clone, Copy)]
struct ClaimRequest<'a> {
    state: &'a Path,
    owner_id: &'a str,
    me: Claimer,
    cap: u8,
    target: Option<u8>,
}

fn allocate_with(
    request: ClaimRequest<'_>,
    ops: &mut impl NetworkOps,
) -> Result<CloneNet, CloneNetError> {
    ops.preflight()?;
    let net = claim_host_identity(request, ops)?;
    let Err(failure) = ops.create(&net, request.owner_id) else {
        return Ok(net);
    };
    rollback_failed_create(request.state, request.owner_id, &net, ops, failure)
}

fn rollback_failed_create(
    state: &Path,
    owner_id: &str,
    net: &CloneNet,
    ops: &mut impl NetworkOps,
    failure: CreateFailure,
) -> Result<CloneNet, CloneNetError> {
    if let Err(rollback) = ops.rollback(net, owner_id, failure.stage) {
        return Err(CloneNetError::Rollback {
            setup: failure.error.to_string(),
            rollback: rollback.to_string(),
        });
    }
    release_claim(state, net.index, owner_id)?;
    Err(failure.error)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
enum CreationStage {
    #[default]
    None,
    #[cfg(any(target_os = "linux", test))]
    OwnerMarker,
    #[cfg(any(target_os = "linux", test))]
    Namespace,
    #[cfg(any(target_os = "linux", test))]
    Veth,
    #[cfg(any(target_os = "linux", test))]
    SourceRule,
}

struct CreateFailure {
    error: CloneNetError,
    stage: CreationStage,
}

trait NetworkOps {
    fn preflight(&mut self) -> Result<(), CloneNetError>;
    fn claim_global(&mut self, index: u8, owner: &str) -> Result<ClaimDecision, CloneNetError>;
    fn create(&mut self, net: &CloneNet, owner: &str) -> Result<(), CreateFailure>;
    fn rollback(
        &mut self,
        net: &CloneNet,
        owner: &str,
        stage: CreationStage,
    ) -> Result<(), CloneNetError>;
    fn teardown(&mut self, net: &CloneNet, owner: &str) -> Result<ReconcileAction, CloneNetError>;
}

struct SystemNetworkOps;

#[cfg(target_os = "linux")]
#[derive(Clone, Copy, PartialEq, Eq)]
enum TeardownOwnership {
    Existing,
    Reserved,
    Foreign,
}

#[cfg(target_os = "linux")]
const HOST_OWNER_DIR: &str = "/run/rooms/clonenet-owners";

#[cfg(target_os = "linux")]
fn owner_marker(index: u8) -> PathBuf {
    Path::new(HOST_OWNER_DIR).join(index.to_string())
}

#[cfg(target_os = "linux")]
fn claim_owner_marker(index: u8, owner: &str) -> Result<ClaimDecision, CloneNetError> {
    std::fs::create_dir_all(HOST_OWNER_DIR)?;
    match std::os::unix::fs::symlink(owner, owner_marker(index)) {
        Ok(()) => Ok(ClaimDecision::Accept),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(ClaimDecision::Skip),
        Err(error) => Err(CloneNetError::Command {
            command: format!("claim host-global clone network {index}"),
            detail: error.to_string(),
        }),
    }
}

#[cfg(target_os = "linux")]
fn marker_owner(index: u8) -> Result<Option<PathBuf>, CloneNetError> {
    match std::fs::read_link(owner_marker(index)) {
        Ok(target) => Ok(Some(target)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(CloneNetError::Io(error)),
    }
}

#[cfg(target_os = "linux")]
fn release_owner_marker(index: u8, owner: &str) -> Result<(), CloneNetError> {
    if marker_owner(index)?.as_deref() != Some(Path::new(owner)) {
        return Ok(());
    }
    match std::fs::remove_file(owner_marker(index)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(CloneNetError::Io(error)),
    }
}

#[cfg(target_os = "linux")]
fn teardown_ownership(index: u8, owner: &str) -> Result<TeardownOwnership, CloneNetError> {
    match marker_owner(index)? {
        Some(actual) if actual == Path::new(owner) => Ok(TeardownOwnership::Existing),
        Some(_) => Ok(TeardownOwnership::Foreign),
        None => match claim_owner_marker(index, owner)? {
            ClaimDecision::Accept => Ok(TeardownOwnership::Reserved),
            ClaimDecision::Skip => Ok(TeardownOwnership::Foreign),
        },
    }
}

#[cfg(target_os = "linux")]
fn reserved_resources_exist(net: &CloneNet, owner: &str) -> Result<bool, CloneNetError> {
    match resources_exist(net) {
        Ok(exists) => Ok(exists),
        Err(error) => {
            release_owner_marker(net.index, owner)?;
            Err(error)
        }
    }
}

#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Clone, PartialEq, Eq)]
struct CommandSpec {
    program: &'static str,
    args: Vec<String>,
}

#[cfg(any(target_os = "linux", test))]
impl CommandSpec {
    const fn new(program: &'static str, args: Vec<String>) -> Self {
        Self { program, args }
    }

    fn display(&self) -> String {
        format!("{} {}", self.program, self.args.join(" "))
    }
}

#[cfg(target_os = "linux")]
impl NetworkOps for SystemNetworkOps {
    fn preflight(&mut self) -> Result<(), CloneNetError> {
        check_clone_subnet_available()
    }

    fn claim_global(&mut self, index: u8, owner: &str) -> Result<ClaimDecision, CloneNetError> {
        claim_owner_marker(index, owner)
    }

    fn create(&mut self, net: &CloneNet, _owner: &str) -> Result<(), CreateFailure> {
        let mut stage = CreationStage::OwnerMarker;
        for (step, command) in create_plan(net).into_iter().enumerate() {
            if let Err(error) = run_command(&command) {
                return Err(CreateFailure { error, stage });
            }
            match step {
                0 => stage = CreationStage::Namespace,
                1 => stage = CreationStage::Veth,
                7 => stage = CreationStage::SourceRule,
                _ => {}
            }
        }
        Ok(())
    }

    fn rollback(
        &mut self,
        net: &CloneNet,
        owner: &str,
        stage: CreationStage,
    ) -> Result<(), CloneNetError> {
        for command in rollback_plan(net, stage) {
            run_allow_absent(&command)?;
        }
        if stage >= CreationStage::OwnerMarker {
            release_owner_marker(net.index, owner)?;
        }
        Ok(())
    }

    fn teardown(&mut self, net: &CloneNet, owner: &str) -> Result<ReconcileAction, CloneNetError> {
        let ownership = teardown_ownership(net.index, owner)?;
        if ownership == TeardownOwnership::Foreign {
            return Ok(ReconcileAction::Keep);
        }
        if ownership == TeardownOwnership::Reserved && reserved_resources_exist(net, owner)? {
            release_owner_marker(net.index, owner)?;
            return Ok(ReconcileAction::Keep);
        }
        for command in teardown_plan(net) {
            run_allow_absent(&command)?;
        }
        release_owner_marker(net.index, owner)?;
        Ok(ReconcileAction::Remove)
    }
}

#[cfg(not(target_os = "linux"))]
impl NetworkOps for SystemNetworkOps {
    fn preflight(&mut self) -> Result<(), CloneNetError> {
        Err(CloneNetError::UnsupportedPlatform)
    }

    fn claim_global(&mut self, _index: u8, _owner: &str) -> Result<ClaimDecision, CloneNetError> {
        Err(CloneNetError::UnsupportedPlatform)
    }

    fn create(&mut self, _net: &CloneNet, _owner: &str) -> Result<(), CreateFailure> {
        Err(CreateFailure {
            error: CloneNetError::UnsupportedPlatform,
            stage: CreationStage::None,
        })
    }

    fn rollback(
        &mut self,
        _net: &CloneNet,
        _owner: &str,
        _stage: CreationStage,
    ) -> Result<(), CloneNetError> {
        Err(CloneNetError::UnsupportedPlatform)
    }

    fn teardown(
        &mut self,
        _net: &CloneNet,
        _owner: &str,
    ) -> Result<ReconcileAction, CloneNetError> {
        Err(CloneNetError::UnsupportedPlatform)
    }
}

#[cfg(any(target_os = "linux", test))]
fn create_plan(net: &CloneNet) -> Vec<CommandSpec> {
    let mut plan = host_create_plan(net);
    plan.extend(namespace_create_plan(net));
    plan
}

#[cfg(any(target_os = "linux", test))]
fn host_create_plan(net: &CloneNet) -> Vec<CommandSpec> {
    let host_cidr = format!("{}/{}", net.host_ip, net.prefix);
    let route_cidr = format!("172.17.0.{}/{}", 4 * net.index, net.prefix);
    vec![
        CommandSpec::new("ip", vec!["netns".into(), "add".into(), net.netns.clone()]),
        CommandSpec::new(
            "ip",
            vec![
                "link".into(),
                "add".into(),
                net.veth_host.clone(),
                "type".into(),
                "veth".into(),
                "peer".into(),
                "name".into(),
                net.veth_guest.clone(),
            ],
        ),
        CommandSpec::new(
            "ip",
            vec![
                "link".into(),
                "set".into(),
                net.veth_guest.clone(),
                "netns".into(),
                net.netns.clone(),
            ],
        ),
        CommandSpec::new(
            "ip",
            vec![
                "addr".into(),
                "add".into(),
                host_cidr,
                "dev".into(),
                net.veth_host.clone(),
            ],
        ),
        CommandSpec::new(
            "ip",
            vec![
                "link".into(),
                "set".into(),
                net.veth_host.clone(),
                "up".into(),
            ],
        ),
        CommandSpec::new(
            "ip",
            vec![
                "route".into(),
                "replace".into(),
                route_cidr,
                "dev".into(),
                net.veth_host.clone(),
                "src".into(),
                net.host_ip.to_string(),
            ],
        ),
        CommandSpec::new(
            "sysctl",
            vec![
                "-w".into(),
                format!("net.ipv4.conf.{}.forwarding=1", net.veth_host),
            ],
        ),
        source_binding_command("-I", net),
    ]
}

#[cfg(any(target_os = "linux", test))]
fn source_binding_command(operation: &str, net: &CloneNet) -> CommandSpec {
    let mut args = vec![operation.to_owned()];
    if operation == "-I" {
        args.extend(["ROOMS_VETH_FWD".to_owned(), "1".to_owned()]);
    } else {
        args.push("ROOMS_VETH_FWD".to_owned());
    }
    args.extend([
        "-i".to_owned(),
        net.veth_host.clone(),
        "!".to_owned(),
        "-s".to_owned(),
        format!("{}/32", net.netns_ip),
        "-j".to_owned(),
        "DROP".to_owned(),
    ]);
    CommandSpec::new("iptables", args)
}

#[cfg(any(target_os = "linux", test))]
fn namespace_create_plan(net: &CloneNet) -> Vec<CommandSpec> {
    let address = format!("{}/{}", net.netns_ip, net.prefix);
    let gateway = net.host_ip.to_string();
    let prefix = vec!["-n".to_owned(), net.netns.clone()];
    vec![
        ip_in_namespace(&prefix, ["addr", "add", &address, "dev", &net.veth_guest]),
        ip_in_namespace(&prefix, ["link", "set", &net.veth_guest, "up"]),
        ip_in_namespace(&prefix, ["link", "set", "lo", "up"]),
        ip_in_namespace(&prefix, ["route", "replace", "default", "via", &gateway]),
        exec_in_namespace(
            net,
            "sysctl",
            ["-w".to_owned(), "net.ipv4.ip_forward=1".to_owned()],
        ),
        exec_in_namespace(
            net,
            "sysctl",
            [
                "-w".to_owned(),
                format!("net.ipv4.conf.{}.forwarding=1", net.veth_guest),
            ],
        ),
        exec_in_namespace(
            net,
            "iptables",
            [
                "-t".to_owned(),
                "nat".to_owned(),
                "-A".to_owned(),
                "POSTROUTING".to_owned(),
                "-s".to_owned(),
                "172.16.0.0/24".to_owned(),
                "-o".to_owned(),
                net.veth_guest.clone(),
                "-j".to_owned(),
                "MASQUERADE".to_owned(),
            ],
        ),
    ]
}

#[cfg(any(target_os = "linux", test))]
fn ip_in_namespace<const N: usize>(prefix: &[String], args: [&str; N]) -> CommandSpec {
    let mut command_args = prefix.to_vec();
    command_args.extend(args.into_iter().map(str::to_owned));
    CommandSpec::new("ip", command_args)
}

#[cfg(any(target_os = "linux", test))]
fn exec_in_namespace<const N: usize>(
    net: &CloneNet,
    program: &'static str,
    args: [String; N],
) -> CommandSpec {
    let mut command_args = vec!["netns".to_owned(), "exec".to_owned(), net.netns.clone()];
    command_args.push(program.to_owned());
    command_args.extend(args);
    CommandSpec::new("ip", command_args)
}

#[cfg(any(target_os = "linux", test))]
fn teardown_plan(net: &CloneNet) -> Vec<CommandSpec> {
    vec![
        CommandSpec::new("ip", vec!["netns".into(), "del".into(), net.netns.clone()]),
        CommandSpec::new(
            "ip",
            vec!["link".into(), "del".into(), net.veth_host.clone()],
        ),
        CommandSpec::new(
            "ip",
            vec!["link".into(), "del".into(), net.veth_guest.clone()],
        ),
        source_binding_command("-D", net),
    ]
}

#[cfg(any(target_os = "linux", test))]
fn rollback_plan(net: &CloneNet, stage: CreationStage) -> Vec<CommandSpec> {
    let mut plan = Vec::new();
    if stage >= CreationStage::Namespace {
        plan.push(CommandSpec::new(
            "ip",
            vec!["netns".into(), "del".into(), net.netns.clone()],
        ));
    }
    if stage >= CreationStage::Veth {
        plan.push(CommandSpec::new(
            "ip",
            vec!["link".into(), "del".into(), net.veth_host.clone()],
        ));
        plan.push(CommandSpec::new(
            "ip",
            vec!["link".into(), "del".into(), net.veth_guest.clone()],
        ));
    }
    if stage >= CreationStage::SourceRule {
        plan.push(source_binding_command("-D", net));
    }
    plan
}

#[cfg(target_os = "linux")]
fn run_command(command: &CommandSpec) -> Result<(), CloneNetError> {
    let output = Command::new(command.program).args(&command.args).output()?;
    if output.status.success() {
        return Ok(());
    }
    Err(command_error(command, &output.stderr))
}

#[cfg(target_os = "linux")]
fn run_allow_absent(command: &CommandSpec) -> Result<(), CloneNetError> {
    let output = Command::new(command.program).args(&command.args).output()?;
    if output.status.success() || absent_resource(&output.stderr) {
        return Ok(());
    }
    Err(command_error(command, &output.stderr))
}

#[cfg(target_os = "linux")]
fn check_clone_subnet_available() -> Result<(), CloneNetError> {
    check_default_policy_rules()?;
    let command = CommandSpec::new(
        "ip",
        vec![
            "-4".into(),
            "route".into(),
            "show".into(),
            "table".into(),
            "all".into(),
        ],
    );
    let output = Command::new(command.program).args(&command.args).output()?;
    if !output.status.success() {
        return Err(command_error(&command, &output.stderr));
    }
    let routes = String::from_utf8_lossy(&output.stdout);
    for route in top_level_routes(&routes) {
        match classify_route(route) {
            RouteClass::Disjoint => {}
            RouteClass::Rooms(index) if marker_owner(index)?.is_some() => {}
            RouteClass::Rooms(_) | RouteClass::Foreign => {
                return Err(CloneNetError::RouteOverlap {
                    route: route.to_owned(),
                });
            }
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn check_default_policy_rules() -> Result<(), CloneNetError> {
    let command = CommandSpec::new("ip", vec!["-4".into(), "rule".into(), "show".into()]);
    let output = Command::new(command.program).args(&command.args).output()?;
    if !output.status.success() {
        return Err(command_error(&command, &output.stderr));
    }
    validate_policy_rules(&String::from_utf8_lossy(&output.stdout))
}

/// Accept only the kernel's untouched RPDB: every observed rule is one of
/// [`DEFAULT_POLICY_RULES`], and each appears exactly once.
///
/// Rejecting unknown rules is only half the check — a *deleted* rule leaves the
/// survivors individually allowed. Losing `32766: from all lookup main` is the
/// dangerous one: the RPDB then never consults the main table, so the `/30` route
/// allocation installs there is dead and clone traffic is unroutable while
/// allocation reports success.
#[cfg(any(target_os = "linux", test))]
fn validate_policy_rules(rules: &str) -> Result<(), CloneNetError> {
    let mut seen = [0usize; DEFAULT_POLICY_RULES.len()];
    for rule in rules.lines().map(str::trim).filter(|line| !line.is_empty()) {
        let counted = default_policy_rule_index(rule).and_then(|index| seen.get_mut(index));
        let Some(count) = counted else {
            return Err(CloneNetError::PolicyRouting {
                rule: rule.to_owned(),
            });
        };
        *count += 1;
    }
    let mismatch = seen
        .iter()
        .zip(DEFAULT_POLICY_RULES)
        .find(|(count, _)| **count != 1);
    let Some((count, expected)) = mismatch else {
        return Ok(());
    };
    Err(CloneNetError::PolicyRouting {
        rule: format!("expected exactly one `{expected}`, found {count}"),
    })
}

/// The kernel's untouched RPDB, in priority order. Allocation's routing model
/// assumes exactly this set: no rule may be added, and none may be missing.
#[cfg(any(target_os = "linux", test))]
const DEFAULT_POLICY_RULES: [&str; 3] = [
    "0: from all lookup local",
    "32766: from all lookup main",
    "32767: from all lookup default",
];

/// Position of `rule` within [`DEFAULT_POLICY_RULES`], or `None` if it is not one
/// of them. Compared token-wise so `ip` spacing differences do not matter.
#[cfg(any(target_os = "linux", test))]
fn default_policy_rule_index(rule: &str) -> Option<usize> {
    let tokens: Vec<&str> = rule.split_whitespace().collect();
    DEFAULT_POLICY_RULES
        .iter()
        .position(|expected| expected.split_whitespace().eq(tokens.iter().copied()))
}

/// Membership-only view of [`default_policy_rule_index`], for the tests that pin
/// which rule shapes are recognised. Production code needs the position.
#[cfg(test)]
fn default_policy_rule(rule: &str) -> bool {
    default_policy_rule_index(rule).is_some()
}

#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RouteClass {
    Disjoint,
    Rooms(u8),
    Foreign,
}

/// Column-zero route records from `ip route` output.
///
/// Multipath nexthops and cache details are emitted on indented continuation
/// lines. Their parent route has already been classified, so parsing them as
/// standalone routes would reject otherwise valid ECMP hosts.
#[cfg(any(target_os = "linux", test))]
fn top_level_routes(routes: &str) -> impl Iterator<Item = &str> {
    routes.lines().filter_map(|line| {
        let route = line.trim();
        if route.is_empty() || line.as_bytes().first().is_some_and(u8::is_ascii_whitespace) {
            return None;
        }
        Some(route)
    })
}

#[cfg(any(target_os = "linux", test))]
fn classify_route(route: &str) -> RouteClass {
    let Some(cidr) = route_cidr(route) else {
        // `ip -4 route` output should always be parseable. Treating an
        // unexpected form as disjoint would make every future grammar gap a
        // fail-open overlap check.
        return RouteClass::Foreign;
    };
    if !overlaps_clone_supernet(cidr.network, cidr.prefix) {
        return RouteClass::Disjoint;
    }
    // A throw in the priority-0 local table cannot shadow `main`: it terminates
    // that table's lookup as though no route was found, so the RPDB continues.
    // Do not extend this exemption to `main`, where a more-specific throw can
    // beat the Rooms /30 before lookup continues past the table.
    if cidr.kind == RouteKind::Throw && in_local_table(route) {
        return RouteClass::Disjoint;
    }
    // A catch-all does not claim the clone supernet: our /30s are longer and win
    // longest-prefix-match. `/1` covers the split-default pair (`0.0.0.0/1` +
    // `128.0.0.0/1`) that VPNs install to override the default without deleting
    // it. Anything narrower stays a claim — a `172.16.0.0/12` on vpn0 really does
    // route our addresses and must still be rejected.
    //
    // That reasoning only holds in tables consulted *after* `main`. The kernel's
    // priority-0 rule hits `local` first, and a match there ends RPDB processing,
    // so a catch-all in `local` shadows the /30 allocation installs in `main` no
    // matter how much longer that prefix is.
    if cidr.prefix <= 1 && !in_local_table(route) {
        return RouteClass::Disjoint;
    }
    rooms_route_index(route, cidr).map_or(RouteClass::Foreign, RouteClass::Rooms)
}

/// Whether a route lives in the `local` table, which the kernel's priority-0 rule
/// consults before `main`.
///
/// Only the printed `table` qualifier decides. Preflight reads
/// `ip -4 route show table all`, and iproute2 prints `table` for every table
/// except `main`, so an omitted qualifier means `main` even for a `local` or
/// `broadcast` route type: the type describes delivery, while the table
/// controls RPDB precedence. Inferring `local` from the type would reject a
/// `local default` installed in `main`, where the Rooms /30 wins longest-prefix
/// match.
#[cfg(any(target_os = "linux", test))]
fn in_local_table(route: &str) -> bool {
    matches!(route_pair(route, "table"), Some("local" | "255"))
}

#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RouteKind {
    Unicast,
    Local,
    Broadcast,
    Throw,
    ForeignSpecial,
}

#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RouteCidr {
    kind: RouteKind,
    network: u32,
    prefix: u8,
}

#[cfg(any(target_os = "linux", test))]
impl RouteCidr {
    /// What `ip` prints as the `default` destination keyword: `0.0.0.0/0`.
    const fn default_for(kind: RouteKind) -> Self {
        Self {
            kind,
            network: 0,
            prefix: 0,
        }
    }
}

#[cfg(any(target_os = "linux", test))]
fn route_cidr(route: &str) -> Option<RouteCidr> {
    let mut tokens = route.split_whitespace();
    let first = tokens.next()?;
    let (kind, destination) = match first {
        "default" => (RouteKind::Unicast, first),
        "local" => (RouteKind::Local, tokens.next()?),
        "broadcast" => (RouteKind::Broadcast, tokens.next()?),
        "throw" => (RouteKind::Throw, tokens.next()?),
        "anycast" | "multicast" | "blackhole" | "unreachable" | "prohibit" | "nat" => {
            (RouteKind::ForeignSpecial, tokens.next()?)
        }
        // Any other alphabetic first token may be a route type we do not model.
        // Consume its destination as a foreign special; if that is absent or
        // malformed, classify_route rejects the parse failure too.
        _ if first.starts_with(|c: char| c.is_ascii_alphabetic()) => {
            (RouteKind::ForeignSpecial, tokens.next()?)
        }
        _ => (RouteKind::Unicast, first),
    };
    // `default` is a destination as well as a possible first token. Preserve
    // an explicit route type (`local default`, `blackhole default`, …) so the
    // local-table precedence check can reject a catch-all that shadows main.
    if destination == "default" {
        return Some(RouteCidr::default_for(kind));
    }
    let (address, prefix) = match destination.split_once('/') {
        Some((address, prefix)) => (address, prefix.parse::<u8>().ok()?),
        None => (destination, 32),
    };
    if prefix > 32 {
        return None;
    }
    let address = u32::from(address.parse::<Ipv4Addr>().ok()?);
    Some(RouteCidr {
        kind,
        network: address & cidr_mask(prefix),
        prefix,
    })
}

#[cfg(any(target_os = "linux", test))]
fn overlaps_clone_supernet(network: u32, prefix: u8) -> bool {
    let mask = cidr_mask(prefix);
    let route_end = network | !mask;
    let clone_start = u32::from(Ipv4Addr::new(172, 17, 0, 0));
    let clone_end = u32::from(Ipv4Addr::new(172, 17, 0, 255));
    network <= clone_end && clone_start <= route_end
}

#[cfg(any(target_os = "linux", test))]
const fn cidr_mask(prefix: u8) -> u32 {
    if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    }
}

#[cfg(any(target_os = "linux", test))]
fn rooms_route_index(route: &str, cidr: RouteCidr) -> Option<u8> {
    if matches!(cidr.kind, RouteKind::Throw | RouteKind::ForeignSpecial)
        || route.split_whitespace().any(|token| token == "via")
    {
        return None;
    }
    let interface = route_pair(route, "dev")?;
    let index = interface
        .strip_prefix("veth-h")?
        .parse::<u8>()
        .ok()
        .filter(|index| (1..=MAX_CLONENET).contains(index))?;
    let net = CloneNet::derive(index).ok()?;
    let base = u32::from(net.host_ip) - 1;
    let expected_shape = match cidr.kind {
        RouteKind::Unicast => cidr.prefix == 30 && cidr.network == base,
        RouteKind::Local => cidr.prefix == 32 && cidr.network == u32::from(net.host_ip),
        RouteKind::Broadcast => {
            cidr.prefix == 32 && matches!(cidr.network, value if value == base || value == base + 3)
        }
        RouteKind::Throw | RouteKind::ForeignSpecial => false,
    };
    let expected_source = net.host_ip.to_string();
    if expected_shape && route_pair(route, "src") == Some(expected_source.as_str()) {
        Some(index)
    } else {
        None
    }
}

#[cfg(any(target_os = "linux", test))]
fn route_pair<'a>(route: &'a str, key: &str) -> Option<&'a str> {
    let mut tokens = route.split_whitespace();
    while let Some(token) = tokens.next() {
        if token == key {
            return tokens.next();
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn resources_exist(net: &CloneNet) -> Result<bool, CloneNetError> {
    let probes = [
        CommandSpec::new(
            "ip",
            vec![
                "netns".into(),
                "exec".into(),
                net.netns.clone(),
                "true".into(),
            ],
        ),
        CommandSpec::new(
            "ip",
            vec!["link".into(), "show".into(), net.veth_host.clone()],
        ),
        CommandSpec::new(
            "ip",
            vec!["link".into(), "show".into(), net.veth_guest.clone()],
        ),
    ];
    for probe in probes {
        let output = Command::new(probe.program).args(&probe.args).output()?;
        if output.status.success() {
            return Ok(true);
        }
        if !absent_resource(&output.stderr) {
            return Err(command_error(&probe, &output.stderr));
        }
    }
    Ok(false)
}

#[cfg(any(target_os = "linux", test))]
fn absent_resource(stderr: &[u8]) -> bool {
    let stderr = String::from_utf8_lossy(stderr);
    let known_absence = [
        "Cannot find device",
        "No such file or directory",
        "Bad rule (does a matching rule exist in that chain?)",
        "No chain/target/match by that name.",
    ]
    .iter()
    .any(|needle| stderr.contains(needle));
    known_absence || (stderr.contains("Device \"") && stderr.contains("\" does not exist"))
}

#[cfg(target_os = "linux")]
fn command_error(command: &CommandSpec, stderr: &[u8]) -> CloneNetError {
    CloneNetError::Command {
        command: command.display(),
        detail: String::from_utf8_lossy(stderr).trim().to_owned(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        reason = "test module"
    )]

    use std::collections::HashSet;
    use std::net::Ipv4Addr;

    use super::{
        absent_resource, allocate_with, claim_identity, cleanup_detail, create_plan,
        default_policy_rule, release_claim, rollback_plan, teardown_plan, ClaimDecision,
        ClaimRequest, Claimer, CloneNet, CloneNetError, CreateFailure, CreationStage, Freed,
        NetworkOps, ReconcileAction, RouteClass, CLONENETS_DIR, MAX_CLONENET,
    };

    const ME: Claimer = Claimer {
        pid: 1,
        starttime: 1,
    };

    fn owner_id(value: u32) -> String {
        format!("{value:026}")
    }

    #[derive(Clone, Copy, Default, PartialEq, Eq)]
    enum TeardownBehavior {
        #[default]
        Succeed,
        Fail,
        Keep,
    }

    #[derive(Default)]
    struct FakeOps {
        fail_create: bool,
        route_conflict: bool,
        taken_global: Option<u8>,
        teardown: TeardownBehavior,
        created: Vec<CloneNet>,
        torn_down: Vec<CloneNet>,
    }

    impl NetworkOps for FakeOps {
        fn preflight(&mut self) -> Result<(), CloneNetError> {
            if self.route_conflict {
                return Err(CloneNetError::RouteOverlap {
                    route: "172.17.0.0/16 dev docker0".to_owned(),
                });
            }
            Ok(())
        }

        fn claim_global(
            &mut self,
            index: u8,
            _owner: &str,
        ) -> Result<ClaimDecision, CloneNetError> {
            if self.taken_global == Some(index) {
                return Ok(ClaimDecision::Skip);
            }
            Ok(ClaimDecision::Accept)
        }

        fn create(&mut self, net: &CloneNet, _owner: &str) -> Result<(), CreateFailure> {
            self.created.push(net.clone());
            if self.fail_create {
                return Err(CreateFailure {
                    error: CloneNetError::Command {
                        command: "injected create".to_owned(),
                        detail: "failed".to_owned(),
                    },
                    stage: CreationStage::SourceRule,
                });
            }
            Ok(())
        }

        fn rollback(
            &mut self,
            net: &CloneNet,
            _owner: &str,
            stage: CreationStage,
        ) -> Result<(), CloneNetError> {
            if stage != CreationStage::None {
                self.torn_down.push(net.clone());
            }
            if self.teardown == TeardownBehavior::Fail {
                return Err(CloneNetError::Command {
                    command: "injected rollback".to_owned(),
                    detail: "failed".to_owned(),
                });
            }
            Ok(())
        }

        fn teardown(
            &mut self,
            net: &CloneNet,
            _owner: &str,
        ) -> Result<ReconcileAction, CloneNetError> {
            if self.teardown == TeardownBehavior::Keep {
                return Ok(ReconcileAction::Keep);
            }
            self.torn_down.push(net.clone());
            if self.teardown == TeardownBehavior::Fail {
                return Err(CloneNetError::Command {
                    command: "injected teardown".to_owned(),
                    detail: "failed".to_owned(),
                });
            }
            Ok(ReconcileAction::Remove)
        }
    }

    #[test]
    fn derive_uses_the_disjoint_veth_carve_and_short_names() {
        let first = CloneNet::derive(1).unwrap();
        assert_eq!(first.netns, "rooms-c1");
        assert_eq!(first.veth_host, "veth-h1");
        assert_eq!(first.veth_guest, "veth-g1");
        assert_eq!(first.host_ip, Ipv4Addr::new(172, 17, 0, 5));
        assert_eq!(first.netns_ip, Ipv4Addr::new(172, 17, 0, 6));
        assert_eq!(first.prefix, 30);
        assert!(first.veth_host.len() <= 15);
        assert!(first.veth_guest.len() <= 15);

        let top = CloneNet::derive(MAX_CLONENET).unwrap();
        assert_eq!(top.host_ip, Ipv4Addr::new(172, 17, 0, 253));
        assert_eq!(top.netns_ip, Ipv4Addr::new(172, 17, 0, 254));
        assert!(top.veth_host.len() <= 15);
        assert!(top.veth_guest.len() <= 15);
    }

    #[test]
    fn every_derived_identity_is_disjoint() {
        let mut namespaces = HashSet::new();
        let mut interfaces = HashSet::new();
        let mut addresses = HashSet::new();
        for index in 1..=MAX_CLONENET {
            let net = CloneNet::derive(index).unwrap();
            assert!(namespaces.insert(net.netns));
            assert!(interfaces.insert(net.veth_host));
            assert!(interfaces.insert(net.veth_guest));
            assert!(addresses.insert(net.host_ip));
            assert!(addresses.insert(net.netns_ip));
        }
    }

    #[test]
    fn invalid_indices_reject_before_touching_state() {
        let state = tempfile::tempdir().unwrap();
        for index in [0, MAX_CLONENET + 1, u8::MAX] {
            assert!(matches!(
                claim_identity(state.path(), &owner_id(1), ME, 8, Some(index)),
                Err(CloneNetError::InvalidIndex { index: got, .. }) if got == index
            ));
        }
        assert!(!state.path().join(CLONENETS_DIR).exists());
    }

    #[test]
    fn foreign_route_overlap_rejects_before_claiming() {
        let state = tempfile::tempdir().unwrap();
        let owner = owner_id(1);
        let request = ClaimRequest {
            state: state.path(),
            owner_id: &owner,
            me: ME,
            cap: 8,
            target: None,
        };
        let mut ops = FakeOps {
            route_conflict: true,
            ..FakeOps::default()
        };
        let error = allocate_with(request, &mut ops).unwrap_err();
        assert!(matches!(error, CloneNetError::RouteOverlap { .. }));
        assert!(!state.path().join(CLONENETS_DIR).exists());
        assert!(ops.created.is_empty());
    }

    #[test]
    fn route_scanner_skips_only_iproute_continuations() {
        let routes = concat!(
            "default proto static\n",
            "\tnexthop via 192.0.2.1 dev eth0 weight 1\n",
            "\tnexthop via 198.51.100.1 dev eth1 weight 1\n",
            "    cache expires 10sec\n",
            "nexthop via 203.0.113.1 dev eth2\n",
            "local not-an-address dev lo table local\n",
        );
        assert_eq!(
            super::top_level_routes(routes).collect::<Vec<_>>(),
            vec![
                "default proto static",
                "nexthop via 203.0.113.1 dev eth2",
                "local not-an-address dev lo table local",
            ]
        );
    }

    #[test]
    fn route_classifier_rejects_overlaps_but_recognizes_rooms_routes() {
        assert_eq!(
            super::route_cidr("local default dev lo table local"),
            Some(super::RouteCidr {
                kind: super::RouteKind::Local,
                network: 0,
                prefix: 0,
            })
        );
        for route in [
            "172.17.0.0/16 dev docker0 proto kernel scope link src 172.17.0.1",
            "172.17.0.0/24 dev bridge0 proto kernel scope link src 172.17.0.1",
            "172.17.0.6 dev bridge0 scope link",
            "172.16.0.0/12 dev vpn0",
            "local 172.17.0.6 dev bridge0 table local proto kernel scope host src 172.17.0.6",
            "blackhole 172.17.0.0/24 table 100",
            "172.17.0.4/30 via 192.0.2.1 dev eth0",
            "172.17.0.4/30 dev veth-h2 scope link src 172.17.0.5",
            "172.17.0.4/30 dev veth-h1 scope link src 172.17.0.9",
            // Route types whose keyword we must not mistake for a destination;
            // the local table they live in is consulted before main.
            "anycast 172.17.0.6 dev eth0 table local",
            "multicast 172.17.0.0/24 dev eth0 table local",
            "someday-new-type 172.17.0.6 dev eth0",
            // A catch-all is only harmless after `main`. In `local` it ends RPDB
            // processing first, so it shadows the /30 however long that prefix is.
            // Note it must be the `128.0.0.0/1` half: `0.0.0.0/1` stops at
            // 127.255.255.255 and never reaches the supernet at all.
            "local 128.0.0.0/1 dev lo table local",
            "128.0.0.0/1 dev lo table local",
            "128.0.0.0/1 via 10.8.0.1 dev tun0 table local",
            "local default dev lo table local",
            "local default dev lo table 255",
            "blackhole default table local",
            "someday-new-type default dev lo table local",
            // A more-specific throw in `main` can win over the Rooms /30 and
            // terminate that table's lookup, so it remains an overlap.
            "throw 172.17.0.6/32 table main",
            // A route line we cannot model must fail closed. `ip -4 route`
            // should be parseable; treating an unexpected form as disjoint
            // would let the next overlapping route grammar bypass preflight.
            "local not-an-address dev lo table local",
        ] {
            assert_eq!(super::classify_route(route), RouteClass::Foreign, "{route}");
        }
        assert_eq!(
            super::classify_route(
                "172.17.0.4/30 dev veth-h1 proto kernel scope link src 172.17.0.5"
            ),
            RouteClass::Rooms(1)
        );
        for route in [
            "local 172.17.0.5 dev veth-h1 table local proto kernel scope host src 172.17.0.5",
            "broadcast 172.17.0.4 dev veth-h1 table local proto kernel scope link src 172.17.0.5",
            "broadcast 172.17.0.7 dev veth-h1 table local proto kernel scope link src 172.17.0.5",
        ] {
            assert_eq!(
                super::classify_route(route),
                RouteClass::Rooms(1),
                "{route}"
            );
        }
        for route in [
            "default via 192.168.5.2 dev eth0",
            "blackhole default table main",
            // A throw lookup deliberately behaves as though the route was not
            // found, so the priority-0 local table always continues to `main`,
            // regardless of the throw prefix.
            "throw default table local",
            "throw 172.17.0.0/24 table local",
            "throw 172.17.0.6/32 table local",
            // An explicit table overrides the default table implied by the
            // local route type. `default` is consulted after `main`.
            "local default dev lo table default",
            "local default dev lo table 253",
            "local default dev lo table main",
            // `ip route show table all` omits the qualifier for `main`, so a
            // typed route without one lives there, not in `local`.
            "local default dev lo",
            "broadcast 128.0.0.0/1 dev lo",
            "0.0.0.0/0 via 192.168.5.2 dev eth0",
            "172.18.0.0/16 dev docker0",
            // VPN split-defaults: a catch-all pair, not a claim on clone space.
            "0.0.0.0/1 via 10.8.0.1 dev tun0",
            "128.0.0.0/1 via 10.8.0.1 dev tun0",
            "192.168.5.0/24 dev eth0",
        ] {
            assert_eq!(
                super::classify_route(route),
                RouteClass::Disjoint,
                "{route}"
            );
        }
    }

    #[test]
    fn only_the_kernel_default_policy_rules_are_accepted() {
        for rule in [
            "0: from all lookup local",
            "32766: from all lookup main",
            "32767: from all lookup default",
        ] {
            assert!(default_policy_rule(rule));
        }
        for rule in [
            "100: from all to 172.17.0.0/24 lookup 100",
            "100: from 172.17.0.0/24 lookup vpn",
            "100: from all fwmark 0x1 lookup 100",
            "100: from all iif veth-h1 lookup 100",
            "32766: from all lookup 100",
        ] {
            assert!(!default_policy_rule(rule));
        }
    }

    #[test]
    fn a_deleted_default_policy_rule_is_caught() {
        use super::DEFAULT_POLICY_RULES;

        let all = DEFAULT_POLICY_RULES.join("\n");
        super::validate_policy_rules(&all).expect("the untouched RPDB must pass");

        // Every single deletion must be rejected. Dropping `lookup main` is the one
        // that silently strands clone traffic: allocation installs each /30 in the
        // main table, which the RPDB would no longer consult.
        for (omitted, expected) in DEFAULT_POLICY_RULES.iter().enumerate() {
            let remaining: Vec<&str> = DEFAULT_POLICY_RULES
                .iter()
                .enumerate()
                .filter(|(index, _)| *index != omitted)
                .map(|(_, rule)| *rule)
                .collect();
            let err = super::validate_policy_rules(&remaining.join("\n")).unwrap_err();
            let CloneNetError::PolicyRouting { rule } = err else {
                panic!("expected PolicyRouting for a missing rule, got {err:?}");
            };
            assert!(
                rule.contains(expected),
                "error must name the missing rule, got {rule}"
            );
        }

        // A duplicate is not the untouched RPDB either.
        let main_rule = DEFAULT_POLICY_RULES
            .get(1)
            .expect("the RPDB set has a main-table rule");
        let duplicated = format!("{all}\n{main_rule}");
        super::validate_policy_rules(&duplicated).expect_err("a duplicated rule must be rejected");
    }

    #[test]
    fn claims_are_independent_and_refill_the_lowest_hole() {
        let state = tempfile::tempdir().unwrap();
        let first = claim_identity(state.path(), &owner_id(1), ME, 8, None).unwrap();
        let second = claim_identity(state.path(), &owner_id(2), ME, 8, None).unwrap();
        let third = claim_identity(state.path(), &owner_id(3), ME, 8, None).unwrap();
        assert_eq!((first.index, second.index, third.index), (1, 2, 3));
        assert_eq!(
            release_claim(state.path(), second.index, &owner_id(2)).unwrap(),
            Freed::Removed
        );
        let refill = claim_identity(state.path(), &owner_id(4), ME, 8, None).unwrap();
        assert_eq!(refill.index, 2);
    }

    #[test]
    fn an_existing_file_is_reservation_opaque_to_the_walk() {
        let state = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(state.path().join(CLONENETS_DIR)).unwrap();
        std::fs::write(state.path().join(CLONENETS_DIR).join("1"), "@reserved\n").unwrap();
        let allocation = claim_identity(state.path(), &owner_id(1), ME, 8, None).unwrap();
        assert_eq!(allocation.index, 2);
    }

    #[test]
    fn a_full_pool_and_taken_target_are_typed() {
        let state = tempfile::tempdir().unwrap();
        claim_identity(state.path(), &owner_id(1), ME, 1, None).unwrap();
        assert!(matches!(
            claim_identity(state.path(), &owner_id(2), ME, 1, None),
            Err(CloneNetError::PoolFull { cap: 1 })
        ));
        assert!(matches!(
            claim_identity(state.path(), &owner_id(2), ME, 8, Some(1)),
            Err(CloneNetError::TargetTaken { index: 1 })
        ));
    }

    #[test]
    fn stale_release_never_removes_a_reassigned_claim() {
        let state = tempfile::tempdir().unwrap();
        claim_identity(state.path(), &owner_id(1), ME, 8, None).unwrap();
        assert_eq!(
            release_claim(state.path(), 1, &owner_id(2)).unwrap(),
            Freed::AlreadyReassigned
        );
        assert!(state.path().join(CLONENETS_DIR).join("1").exists());
    }

    #[test]
    fn create_plan_enumerates_forwarding_route_and_first_hop_nat() {
        let net = CloneNet::derive(3).unwrap();
        let commands: Vec<_> = create_plan(&net)
            .into_iter()
            .map(|command| command.display())
            .collect();
        assert!(commands
            .contains(&"ip route replace 172.17.0.12/30 dev veth-h3 src 172.17.0.13".to_owned()));
        assert!(commands.contains(&"sysctl -w net.ipv4.conf.veth-h3.forwarding=1".to_owned()));
        assert!(commands.contains(
            &"iptables -I ROOMS_VETH_FWD 1 -i veth-h3 ! -s 172.17.0.14/32 -j DROP".to_owned()
        ));
        assert!(
            commands.contains(&"ip netns exec rooms-c3 sysctl -w net.ipv4.ip_forward=1".to_owned())
        );
        assert!(commands.contains(
            &"ip netns exec rooms-c3 sysctl -w net.ipv4.conf.veth-g3.forwarding=1".to_owned()
        ));
        assert!(commands.contains(
            &concat!(
                "ip netns exec rooms-c3 iptables -t nat -A POSTROUTING ",
                "-s 172.16.0.0/24 -o veth-g3 -j MASQUERADE"
            )
            .to_owned()
        ));
    }

    #[test]
    fn teardown_covers_namespace_and_both_partial_veth_ends() {
        let commands: Vec<_> = teardown_plan(&CloneNet::derive(3).unwrap())
            .into_iter()
            .map(|command| command.display())
            .collect();
        assert_eq!(
            commands,
            [
                "ip netns del rooms-c3",
                "ip link del veth-h3",
                "ip link del veth-g3",
                "iptables -D ROOMS_VETH_FWD -i veth-h3 ! -s 172.17.0.14/32 -j DROP"
            ]
        );
    }

    #[test]
    fn teardown_only_suppresses_real_absence_errors() {
        assert!(absent_resource(b"Cannot find device \"veth-h3\""));
        assert!(absent_resource(b"Device \"veth-h3\" does not exist."));
        assert!(absent_resource(
            b"Cannot remove namespace file /run/netns/rooms-c3: No such file or directory"
        ));
        assert!(!absent_resource(
            b"Cannot remove namespace file /run/netns/rooms-c3: Device or resource busy"
        ));
        assert!(absent_resource(
            b"Bad rule (does a matching rule exist in that chain?)."
        ));
        assert!(absent_resource(b"No chain/target/match by that name."));
    }

    #[test]
    fn cleanup_command_errors_are_wrapped_once() {
        let detail = cleanup_detail(CloneNetError::Command {
            command: "ip netns del rooms-c3".to_owned(),
            detail: "Device or resource busy".to_owned(),
        });
        assert_eq!(detail, "ip netns del rooms-c3: Device or resource busy");
    }

    #[test]
    fn failed_partial_create_tears_down_then_releases_the_claim() {
        let state = tempfile::tempdir().unwrap();
        let owner = owner_id(1);
        let request = ClaimRequest {
            state: state.path(),
            owner_id: &owner,
            me: ME,
            cap: 8,
            target: None,
        };
        let mut ops = FakeOps {
            fail_create: true,
            ..FakeOps::default()
        };
        assert!(allocate_with(request, &mut ops).is_err());
        assert_eq!(ops.created.len(), 1);
        assert_eq!(ops.torn_down.len(), 1);
        assert!(!state.path().join(CLONENETS_DIR).join("1").exists());
    }

    #[test]
    fn failed_rollback_keeps_the_claim_for_reconcile() {
        let state = tempfile::tempdir().unwrap();
        let owner = owner_id(1);
        let request = ClaimRequest {
            state: state.path(),
            owner_id: &owner,
            me: ME,
            cap: 8,
            target: None,
        };
        let mut ops = FakeOps {
            fail_create: true,
            teardown: TeardownBehavior::Fail,
            ..FakeOps::default()
        };
        let error = allocate_with(request, &mut ops).unwrap_err();
        assert!(matches!(error, CloneNetError::Rollback { .. }));
        assert!(state.path().join(CLONENETS_DIR).join("1").exists());
    }

    #[test]
    fn untargeted_global_collision_advances_without_foreign_teardown() {
        let state = tempfile::tempdir().unwrap();
        let owner = owner_id(1);
        let request = ClaimRequest {
            state: state.path(),
            owner_id: &owner,
            me: ME,
            cap: 8,
            target: None,
        };
        let mut ops = FakeOps {
            taken_global: Some(1),
            ..FakeOps::default()
        };
        let net = allocate_with(request, &mut ops).unwrap();
        assert_eq!(net.index, 2);
        assert_eq!(ops.created, [net]);
        assert!(ops.torn_down.is_empty());
        assert!(!state.path().join(CLONENETS_DIR).join("1").exists());
        assert!(state.path().join(CLONENETS_DIR).join("2").exists());
    }

    #[test]
    fn targeted_global_collision_fails_without_foreign_teardown() {
        let state = tempfile::tempdir().unwrap();
        let owner = owner_id(1);
        let request = ClaimRequest {
            state: state.path(),
            owner_id: &owner,
            me: ME,
            cap: 8,
            target: Some(1),
        };
        let mut ops = FakeOps {
            taken_global: Some(1),
            ..FakeOps::default()
        };
        let error = allocate_with(request, &mut ops).unwrap_err();
        assert!(matches!(error, CloneNetError::TargetTaken { index: 1 }));
        assert!(ops.created.is_empty());
        assert!(ops.torn_down.is_empty());
        assert!(!state.path().join(CLONENETS_DIR).join("1").exists());
    }

    #[test]
    fn rollback_plan_deletes_only_resources_created_by_this_attempt() {
        let net = CloneNet::derive(3).unwrap();
        let namespace_only: Vec<_> = rollback_plan(&net, CreationStage::Namespace)
            .into_iter()
            .map(|command| command.display())
            .collect();
        assert_eq!(namespace_only, ["ip netns del rooms-c3"]);
        assert!(rollback_plan(&net, CreationStage::None).is_empty());
        assert!(rollback_plan(&net, CreationStage::OwnerMarker).is_empty());

        let through_veth: Vec<_> = rollback_plan(&net, CreationStage::Veth)
            .into_iter()
            .map(|command| command.display())
            .collect();
        assert_eq!(
            through_veth,
            [
                "ip netns del rooms-c3",
                "ip link del veth-h3",
                "ip link del veth-g3"
            ]
        );

        let through_source_rule: Vec<_> = rollback_plan(&net, CreationStage::SourceRule)
            .into_iter()
            .map(|command| command.display())
            .collect();
        assert_eq!(
            through_source_rule.last(),
            Some(&"iptables -D ROOMS_VETH_FWD -i veth-h3 ! -s 172.17.0.14/32 -j DROP".to_owned())
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn dead_claim_reconcile_tears_down_before_removal() {
        const DEAD: Claimer = Claimer {
            pid: 4_194_305,
            starttime: 1,
        };
        let state = tempfile::tempdir().unwrap();
        claim_identity(state.path(), &owner_id(1), DEAD, 8, None).unwrap();
        let mut ops = FakeOps::default();
        let reclaimed = super::reconcile_with(state.path(), &mut ops);
        assert_eq!(reclaimed.len(), 1);
        assert!(reclaimed[0].removed);
        assert_eq!(ops.torn_down.len(), 1);
        assert!(!state.path().join(CLONENETS_DIR).join("1").exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn dead_foreign_claim_reports_retained_without_teardown() {
        const DEAD: Claimer = Claimer {
            pid: 4_194_305,
            starttime: 1,
        };
        let state = tempfile::tempdir().unwrap();
        claim_identity(state.path(), &owner_id(1), DEAD, 8, None).unwrap();
        let mut ops = FakeOps {
            teardown: TeardownBehavior::Keep,
            ..FakeOps::default()
        };
        let reclaimed = super::reconcile_with(state.path(), &mut ops);
        assert_eq!(reclaimed.len(), 1);
        assert!(!reclaimed[0].removed);
        assert!(ops.torn_down.is_empty());
        assert!(state.path().join(CLONENETS_DIR).join("1").exists());
    }
}
