//! Per-room egress policy: parse, chain synthesis, and the enforcement
//! predicate — plus the host-side install/remove mechanism.
//!
//! The witness ([`crate::witness`]) *observes* what a room's tap emitted; this
//! module turns that surface into an *enforcer* of what the room can reach. A
//! flat room's egress is policed on the host by a dedicated `ROOMS_EG_<k>`
//! filter chain, jumped from `ROOMS_FWD` on the room's **ingress tap**
//! `tap-fc<k>`. A clone uses the additive mirror: `ROOMS_CEG_<n>` is jumped
//! from `ROOMS_VETH_FWD` on its exact root-side `veth-h<n>`.
//!
//! Keying on the ingress interface, not the guest's source IP, is load-bearing.
//! The guest runs untrusted, root-capable code and can forge its IPv4 source —
//! a rule keyed on `-s <guest_ip>` is a spoofing bypass (a forged sibling source
//! dodges the per-room DROP and falls through to the shared supernet ACCEPT).
//! The guest cannot forge which host interface its packets physically transit;
//! the flat tap or clone veth is therefore the anchor.
//!
//! The synthesis and ordering logic is pure functions over `iptables -S` dumps,
//! mirroring [`crate::isolation`], so the negative assertions — every way the
//! enforcement can silently fail to bite — are unit-tested against
//! deliberately-broken inputs in CI, not merely observed live on the host.

use crate::isolation::SUPERNET;
use crate::veth_isolation::{
    clone_egress_chain, clone_egress_jump_index, clone_forwarding_substrate,
    clone_source_binding_present, clone_veth_index, rooms_veth_fwd_isolates,
    veth_input_drop_present, VETH_SUPERNET,
};

#[cfg(unix)]
use std::sync::{Mutex, MutexGuard};

/// The host `ROOMS_FWD` chain the per-room jump is spliced into.
const FWD_CHAIN: &str = "ROOMS_FWD";

/// Host-local traffic enters this chain instead of `ROOMS_FWD`.
const INPUT_CHAIN: &str = "INPUT";

/// Prefix of a pool tap name (`tap-fc<k>`); the `<k>` suffix keys the chain.
const TAP_PREFIX: &str = "tap-fc";

/// Prefix of a per-room egress chain name (`ROOMS_EG_<k>`).
const EG_CHAIN_PREFIX: &str = "ROOMS_EG_";

/// The clone-veth forwarding chain that owns per-clone egress jumps.
const VETH_FWD_CHAIN: &str = "ROOMS_VETH_FWD";

/// Built-in chain whose first three entries route flat and clone traffic.
const HOST_FWD_CHAIN: &str = "FORWARD";

/// Give an external xtables writer a bounded opportunity to complete. Every
/// read and mutation uses the same wait, so contention is an explicit failure
/// after this bound rather than a flaky immediate `xtables lock` error.
#[cfg(unix)]
const XTABLES_WAIT_SECONDS: &str = "2";

/// One process-wide critical section for a complete egress transaction. This
/// covers cleanup, live-state planning, mutations, and the final proof; sibling
/// clone restore tasks cannot invalidate one another's insertion positions.
#[cfg(unix)]
static IPTABLES_TRANSACTION: Mutex<()> = Mutex::new(());

/// A parsed `--egress` policy — the syntax the flag admits, before any host
/// resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Policy {
    /// Flag absent: today's observe-only behavior (non-breaking default).
    Observe,
    /// `--egress none`: drop all forwarded egress from the room's tap.
    None,
    /// `--egress allowlist:<host-or-cidr>[,...]`: permit only the listed
    /// destinations, drop everything else.
    Allowlist(Vec<Dest>),
}

/// One allowlist destination as parsed: a hostname (resolved to IPs at launch)
/// or an address/CIDR literal (pinned as written).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dest {
    /// A hostname, resolved to one or more IPv4 addresses at launch.
    Host(String),
    /// An IPv4 address or CIDR literal, used verbatim as an `-d` match.
    Cidr(String),
}

/// A launch-ready egress plan: the policy resolved against DNS, holding the
/// pinned `-d` destination strings an `allowlist` permits. What `main` threads
/// into the boot path and the witness record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// No enforcement — the witness stays observe-only.
    Observe,
    /// Drop all forwarded egress.
    None,
    /// Permit exactly these pinned destinations (IPs and/or CIDRs).
    Allowlist(Vec<String>),
}

impl Plan {
    /// Whether this plan installs a per-room chain (`none` and `allowlist` do;
    /// `observe` does not).
    #[must_use]
    pub const fn enforces(&self) -> bool {
        !matches!(self, Self::Observe)
    }

    /// The pinned destinations this plan permits — empty for `observe` and
    /// `none` (both permit nothing; `none` drops everything, `observe` installs
    /// no chain at all).
    #[must_use]
    pub fn permitted(&self) -> &[String] {
        match self {
            Self::Allowlist(dests) => dests,
            Self::Observe | Self::None => &[],
        }
    }
}

/// Parse a `--egress` flag value into a [`Policy`]. Fails fast on an empty
/// allowlist, a malformed CIDR, or an unknown mode — before any slot is claimed.
///
/// # Errors
/// A message naming the offending input when the value is not `none` or a
/// well-formed `allowlist:<host-or-cidr>[,...]`.
pub fn parse(value: &str) -> Result<Policy, String> {
    let value = value.trim();
    if value == "none" {
        return Ok(Policy::None);
    }
    if let Some(rest) = value.strip_prefix("allowlist:") {
        return parse_allowlist(rest);
    }
    Err(format!(
        "invalid --egress '{value}': want 'none' or 'allowlist:<host-or-cidr>[,...]'"
    ))
}

/// Parse the comma-separated body of an `allowlist:` policy. An empty entry
/// rejects — and since `split(',')` always yields at least one element, an empty
/// body (`allowlist:`) is caught by that same empty-entry check.
fn parse_allowlist(rest: &str) -> Result<Policy, String> {
    let mut dests = Vec::new();
    for entry in rest.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            return Err("invalid --egress allowlist: an empty destination in the list".to_owned());
        }
        dests.push(parse_dest(entry)?);
    }
    Ok(Policy::Allowlist(dests))
}

/// Classify one allowlist entry: a CIDR (`a.b.c.d/n`), a bare IPv4 literal (a
/// `/32`), or otherwise a hostname resolved at launch.
fn parse_dest(entry: &str) -> Result<Dest, String> {
    if entry.contains('/') {
        validate_cidr(entry)?;
        return Ok(Dest::Cidr(entry.to_owned()));
    }
    if entry.parse::<std::net::Ipv4Addr>().is_ok() {
        return Ok(Dest::Cidr(entry.to_owned()));
    }
    Ok(Dest::Host(entry.to_owned()))
}

/// Validate an IPv4 CIDR literal: a parseable address and a prefix in `0..=32`.
fn validate_cidr(entry: &str) -> Result<(), String> {
    let Some((addr, prefix)) = entry.split_once('/') else {
        return Err(format!("invalid --egress CIDR '{entry}': missing '/'"));
    };
    if addr.parse::<std::net::Ipv4Addr>().is_err() {
        return Err(format!("invalid --egress CIDR '{entry}': bad address"));
    }
    let ok = prefix.parse::<u8>().is_ok_and(|p| p <= 32);
    if !ok {
        return Err(format!(
            "invalid --egress CIDR '{entry}': prefix must be 0..=32"
        ));
    }
    Ok(())
}

/// Resolve a [`Policy`] into a launch-ready [`Plan`].
///
/// Each allowlist hostname is pinned to its IPv4 addresses **once** at launch (a
/// rotating endpoint that changes IPs mid-run will start dropping — the
/// documented v1 limitation, with CIDR form as the escape hatch).
///
/// # Errors
/// When a hostname cannot be resolved, or resolves to no IPv4 address.
pub fn resolve(policy: &Policy) -> Result<Plan, String> {
    match policy {
        Policy::Observe => Ok(Plan::Observe),
        Policy::None => Ok(Plan::None),
        Policy::Allowlist(dests) => resolve_allowlist(dests).map(Plan::Allowlist),
    }
}

/// Flatten an allowlist into its pinned `-d` destination strings.
fn resolve_allowlist(dests: &[Dest]) -> Result<Vec<String>, String> {
    let mut permitted = Vec::new();
    for dest in dests {
        match dest {
            Dest::Cidr(literal) => permitted.push(literal.clone()),
            Dest::Host(host) => permitted.extend(resolve_host(host)?),
        }
    }
    Ok(permitted)
}

/// Resolve a hostname to its sorted, deduplicated IPv4 addresses. IPv6 is
/// dropped — the pool is IPv4-only, so an AAAA-only host is an error here.
fn resolve_host(host: &str) -> Result<Vec<String>, String> {
    use std::net::ToSocketAddrs;
    let addrs = (host, 0u16)
        .to_socket_addrs()
        .map_err(|e| format!("--egress: cannot resolve host '{host}': {e}"))?;
    let mut ips: Vec<String> = addrs
        .filter_map(|sa| match sa.ip() {
            std::net::IpAddr::V4(v4) => Some(v4.to_string()),
            std::net::IpAddr::V6(_) => None,
        })
        .collect();
    ips.sort();
    ips.dedup();
    if ips.is_empty() {
        return Err(format!(
            "--egress: host '{host}' resolved to no IPv4 address"
        ));
    }
    Ok(ips)
}

/// The per-room egress chain name for a pool tap `tap-fc<k>` → `ROOMS_EG_<k>`.
///
/// `None` for anything that is not a pool tap (the shared/legacy tap, a stray
/// name), so teardown never touches an unrelated chain.
#[must_use]
pub fn chain_for_tap(tap: &str) -> Option<String> {
    let suffix = tap.strip_prefix(TAP_PREFIX)?;
    if suffix.is_empty() || !suffix.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(format!("{EG_CHAIN_PREFIX}{suffix}"))
}

/// The per-clone egress chain for a canonical root-side veth:
/// `veth-h<n>` -> `ROOMS_CEG_<n>` for `n` in `1..=63`.
///
/// The canonical spelling check is intentional. Broad (`veth-h+`),
/// zero-padded, out-of-range, or otherwise malformed identities must never
/// select a chain during install or teardown.
#[must_use]
pub fn clone_chain_for_veth(veth: &str) -> Option<String> {
    clone_veth_index(veth).map(clone_egress_chain)
}

/// Ordered rules for a flat-room or clone sub-chain, **appended** top-to-bottom.
///
/// Append never reverses, so the ordering-bug class is structurally impossible:
/// one `ACCEPT` per permitted destination, then a `LOG`, then a catch-all
/// `DROP` — all scoped by `-o <out_iface>`. An empty `permitted` yields just the
/// log + drop (the `none` shape). Each inner vec is the `iptables` argument list.
#[must_use]
pub fn subchain_rules(chain: &str, out_iface: &str, permitted: &[String]) -> Vec<Vec<String>> {
    let mut rules = Vec::with_capacity(permitted.len() + 2);
    for dest in permitted {
        rules.push(vec![
            "-A".to_owned(),
            chain.to_owned(),
            "-d".to_owned(),
            dest.clone(),
            "-o".to_owned(),
            out_iface.to_owned(),
            "-j".to_owned(),
            "ACCEPT".to_owned(),
        ]);
    }
    rules.push(vec![
        "-A".to_owned(),
        chain.to_owned(),
        "-o".to_owned(),
        out_iface.to_owned(),
        "-j".to_owned(),
        "LOG".to_owned(),
        "--log-prefix".to_owned(),
        drop_log_prefix(chain),
    ]);
    rules.push(vec![
        "-A".to_owned(),
        chain.to_owned(),
        "-o".to_owned(),
        out_iface.to_owned(),
        "-j".to_owned(),
        "DROP".to_owned(),
    ]);
    rules
}

/// The `LOG` prefix for a room's dropped egress, scoped by the chain's `<k>`.
fn drop_log_prefix(chain: &str) -> String {
    let k = chain
        .strip_prefix(EG_CHAIN_PREFIX)
        .or_else(|| chain.strip_prefix("ROOMS_CEG_"))
        .unwrap_or(chain);
    format!("rooms-egress-drop:{k} ")
}

/// The single position-sensitive jump into `ROOMS_FWD`, keyed on the unforgeable
/// tap.
///
/// `pos` is the 1-indexed rank the jump takes (pushing the supernet egress
/// ACCEPT down); [`insert_position`] computes it from the live dump.
#[must_use]
pub fn jump_rule(pos: usize, tap: &str, chain: &str) -> Vec<String> {
    vec![
        "-I".to_owned(),
        FWD_CHAIN.to_owned(),
        pos.to_string(),
        "-i".to_owned(),
        tap.to_owned(),
        "-j".to_owned(),
        chain.to_owned(),
    ]
}

/// Tap-keyed host-local drop check/delete used by [`Plan::None`].
#[must_use]
pub fn input_drop_rule(op: &str, tap: &str) -> Vec<String> {
    vec![
        op.to_owned(),
        INPUT_CHAIN.to_owned(),
        "-i".to_owned(),
        tap.to_owned(),
        "-j".to_owned(),
        "DROP".to_owned(),
    ]
}

/// Position-sensitive insertion form: the base's drop goes first in INPUT.
#[must_use]
pub fn input_drop_insert_rule(tap: &str) -> Vec<String> {
    vec![
        "-I".to_owned(),
        INPUT_CHAIN.to_owned(),
        "1".to_owned(),
        "-i".to_owned(),
        tap.to_owned(),
        "-j".to_owned(),
        "DROP".to_owned(),
    ]
}

/// The exact clone-veth jump inserted into `ROOMS_VETH_FWD`.
///
/// `None` rejects a non-canonical veth or the invalid iptables position zero;
/// callers never supply a chain name that could disagree with the interface.
#[must_use]
pub fn clone_jump_rule(pos: usize, veth: &str) -> Option<Vec<String>> {
    if pos == 0 {
        return None;
    }
    let chain = clone_chain_for_veth(veth)?;
    Some(vec![
        "-I".to_owned(),
        VETH_FWD_CHAIN.to_owned(),
        pos.to_string(),
        "-i".to_owned(),
        veth.to_owned(),
        "-j".to_owned(),
        chain,
    ])
}

/// Insertion rank for a clone jump: the first existing clone jump, or the
/// upstream ACCEPT when the block is empty.
///
/// The whole veth-chain grammar must be valid first. Inserting at the head of
/// this block keeps every clone jump below the fixed isolation/private DROP
/// prefix and above the permissive upstream ACCEPT.
#[must_use]
pub fn clone_insert_position(veth_dump: &str) -> Option<usize> {
    if !rooms_veth_fwd_isolates(veth_dump) {
        return None;
    }
    let mut rank = 0usize;
    for line in veth_dump.lines().map(str::trim) {
        if !line.starts_with("-A ROOMS_VETH_FWD ") {
            continue;
        }
        rank += 1;
        if clone_egress_jump_index(line).is_some() || veth_egress_interface(line).is_some() {
            return Some(rank);
        }
    }
    None
}

/// Upstream interface from the exact veth-supernet egress ACCEPT.
#[must_use]
pub fn clone_out_iface(veth_dump: &str) -> Option<String> {
    if !rooms_veth_fwd_isolates(veth_dump) {
        return None;
    }
    veth_dump
        .lines()
        .map(str::trim)
        .find_map(veth_egress_interface)
        .map(str::to_owned)
}

fn veth_egress_interface(line: &str) -> Option<&str> {
    let mut tokens = line.split_whitespace();
    match (
        tokens.next(),
        tokens.next(),
        tokens.next(),
        tokens.next(),
        tokens.next(),
        tokens.next(),
        tokens.next(),
        tokens.next(),
        tokens.next(),
    ) {
        (
            Some("-A"),
            Some(VETH_FWD_CHAIN),
            Some("-s"),
            Some(VETH_SUPERNET),
            Some("-o"),
            Some(interface),
            Some("-j"),
            Some("ACCEPT"),
            None,
        ) if !interface.contains('+') => Some(interface),
        _ => None,
    }
}

/// 1-indexed rank (among `-A ROOMS_FWD` rules) of the supernet egress ACCEPT.
///
/// Where the tap-keyed jump goes so it sits **above** the permissive ACCEPT and
/// (since the isolation/RFC1918 DROPs precede it) below them. `None` when the
/// dump has no such ACCEPT.
#[must_use]
pub fn insert_position(forward_dump: &str) -> Option<usize> {
    let mut rank = 0usize;
    for line in forward_dump.lines() {
        let line = line.trim();
        if !line.starts_with("-A ROOMS_FWD") {
            continue;
        }
        rank += 1;
        if is_supernet_egress_accept(line) {
            return Some(rank);
        }
    }
    None
}

/// The outbound interface the substrate egresses over — the `-o` of the supernet
/// egress ACCEPT.
///
/// Read from the live chain so this layer never re-detects the route the host
/// substrate already chose. `None` when absent.
#[must_use]
pub fn out_iface(forward_dump: &str) -> Option<String> {
    forward_dump
        .lines()
        .map(str::trim)
        .filter(|line| is_supernet_egress_accept(line))
        .find_map(iface_after_o)
}

/// True for the supernet egress ACCEPT rule (`-s <supernet> ... -o <iface> -j
/// ACCEPT`). Precise on `-o` and the supernet source so the return-path ACCEPT
/// (`-i <iface> -d <supernet> ... ACCEPT`, no `-s <supernet>`) is not mistaken
/// for it.
fn is_supernet_egress_accept(line: &str) -> bool {
    line.contains(&format!("-s {SUPERNET}")) && line.contains("-o ") && line.contains("-j ACCEPT")
}

/// The interface token following `-o` in a rule line.
fn iface_after_o(line: &str) -> Option<String> {
    let mut tokens = line.split_whitespace();
    while let Some(token) = tokens.next() {
        if token == "-o" {
            return tokens.next().map(str::to_owned);
        }
    }
    None
}

/// True when a room's egress is genuinely enforced.
///
/// The tap-keyed jump is present in `ROOMS_FWD`, sits **above** the supernet
/// egress ACCEPT (so the room can't reach the permissive ACCEPT) and **below**
/// the guest↔guest isolation DROP (so an allowlist can never override
/// isolation), and the sub-chain terminates in a catch-all DROP (so nothing
/// falls through).
///
/// The refutations this must catch (each a way enforcement silently fails): a
/// jump below the supernet ACCEPT leaks; a source-keyed jump is spoofable; a
/// sub-chain without its catch-all DROP falls through; a jump above the
/// isolation DROP overrides isolation. Mirrors [`crate::isolation`] — precise on
/// `-i`/`-o`, so a rule constraining only source misclassifies.
#[must_use]
pub fn room_egress_enforced(tap: &str, out_iface: &str, forward_dump: &str, eg_dump: &str) -> bool {
    let Some(chain) = chain_for_tap(tap) else {
        return false;
    };
    let Some(jump) = jump_line(forward_dump, tap, &chain) else {
        return false;
    };
    let Some(accept) = line_of(forward_dump, is_supernet_egress_accept) else {
        return false;
    };
    let Some(isolation_drop) = line_of(forward_dump, is_isolation_drop) else {
        return false;
    };
    jump < accept && jump > isolation_drop && subchain_terminates_in_drop(eg_dump, out_iface)
}

/// Line index of the tap-keyed jump into `chain`: a `ROOMS_FWD` rule matching on
/// `-i <tap>` (unforgeable) and targeting `-j <chain>`. A rule keyed only on
/// source (`-s <ip> -j <chain>`) is deliberately not a match — that is the
/// spoofable form the predicate exists to reject.
fn jump_line(forward_dump: &str, tap: &str, chain: &str) -> Option<usize> {
    let want_i = format!("-i {tap} ");
    let want_j = format!("-j {chain} ");
    forward_dump.lines().position(|line| {
        let padded = format!("{} ", line.trim());
        padded.contains(&want_i) && padded.contains(&want_j)
    })
}

/// True for the guest↔guest isolation DROP (`-s <supernet> -d <supernet> -j
/// DROP`), the substrate rule an allowlist must never be able to override.
fn is_isolation_drop(line: &str) -> bool {
    line.contains(&format!("-s {SUPERNET}"))
        && line.contains(&format!("-d {SUPERNET}"))
        && line.contains("-j DROP")
}

/// Line index of the first rule satisfying `pred` in a dump.
fn line_of(dump: &str, pred: impl Fn(&str) -> bool) -> Option<usize> {
    dump.lines().position(|line| pred(line.trim()))
}

/// True when the sub-chain's **last** rule is a catch-all DROP scoped by `-o
/// <out_iface>` — so a packet that matched no ACCEPT is dropped, never falling
/// off the chain's end back to the supernet ACCEPT.
fn subchain_terminates_in_drop(eg_dump: &str, out_iface: &str) -> bool {
    eg_dump
        .lines()
        .map(str::trim)
        .rfind(|line| line.starts_with("-A "))
        .is_some_and(|line| line.contains(&format!("-o {out_iface}")) && line.contains("-j DROP"))
}

/// True when a clone's veth-keyed policy is installed exactly and cannot fall
/// through to the shared veth-supernet ACCEPT.
///
/// The shared chain must satisfy its full grammar; the canonical
/// `veth-h<n>` -> `ROOMS_CEG_<n>` jump must appear exactly once in the allowed
/// jump block; the subchain must implement the resolved plan in order. `none`
/// additionally requires the exact, unpreempted host-local INPUT drop.
#[must_use]
pub fn clone_egress_enforced(
    veth: &str,
    out_iface: &str,
    veth_dump: &str,
    eg_dump: &str,
    input_dump: &str,
    plan: &Plan,
) -> bool {
    if !plan.enforces() || !rooms_veth_fwd_isolates(veth_dump) {
        return false;
    }
    let Some(index) = clone_veth_index(veth) else {
        return false;
    };
    let chain = clone_egress_chain(index);
    if clone_out_iface(veth_dump).as_deref() != Some(out_iface) {
        return false;
    }
    let jump_count = veth_dump
        .lines()
        .map(str::trim)
        .filter_map(clone_egress_jump_index)
        .filter(|candidate| *candidate == index)
        .count();
    if jump_count != 1 || !clone_subchain_matches_plan(&chain, out_iface, eg_dump, plan) {
        return false;
    }
    !matches!(plan, Plan::None) || veth_input_drop_present(input_dump, veth)
}

fn clone_observe_unenforced(veth: &str, veth_dump: &str, input_dump: &str) -> bool {
    let Some(index) = clone_veth_index(veth) else {
        return false;
    };
    let stale_jump = veth_dump
        .lines()
        .map(str::trim)
        .filter_map(clone_egress_jump_index)
        .any(|candidate| candidate == index);
    let expected_input = input_drop_rule("-A", veth).join(" ");
    let stale_input = input_dump
        .lines()
        .map(str::trim)
        .any(|line| line == expected_input);
    !stale_jump && !stale_input
}

fn clone_subchain_matches_plan(chain: &str, out_iface: &str, dump: &str, plan: &Plan) -> bool {
    let permitted = match plan {
        Plan::None => &[][..],
        Plan::Allowlist(destinations) => destinations.as_slice(),
        Plan::Observe => return false,
    };
    let mut lines = dump.lines().map(str::trim).filter(|line| !line.is_empty());
    if lines.next() != Some(format!("-N {chain}").as_str()) {
        return false;
    }
    for destination in permitted {
        let Some(line) = lines.next() else {
            return false;
        };
        if !clone_accept_matches(line, chain, out_iface, destination) {
            return false;
        }
    }
    let log_matches = lines
        .next()
        .is_some_and(|line| clone_log_matches(line, chain, out_iface));
    let drop = format!("-A {chain} -o {out_iface} -j DROP");
    log_matches && lines.next() == Some(drop.as_str()) && lines.next().is_none()
}

fn clone_accept_matches(line: &str, chain: &str, out_iface: &str, destination: &str) -> bool {
    let mut tokens = line.split_whitespace();
    let live_destination = match (
        tokens.next(),
        tokens.next(),
        tokens.next(),
        tokens.next(),
        tokens.next(),
        tokens.next(),
        tokens.next(),
        tokens.next(),
        tokens.next(),
    ) {
        (
            Some("-A"),
            Some(live_chain),
            Some("-d"),
            Some(live_destination),
            Some("-o"),
            Some(live_iface),
            Some("-j"),
            Some("ACCEPT"),
            None,
        ) if live_chain == chain && live_iface == out_iface => live_destination,
        _ => return false,
    };
    destinations_equivalent(destination, live_destination)
}

fn destinations_equivalent(expected: &str, live: &str) -> bool {
    expected == live
        || ipv4_network(expected).is_some_and(|network| Some(network) == ipv4_network(live))
}

fn ipv4_network(destination: &str) -> Option<(u32, u8)> {
    let (address, prefix) = match destination.split_once('/') {
        Some((address, prefix)) => (address, prefix.parse::<u8>().ok()?),
        None => (destination, 32),
    };
    if prefix > 32 {
        return None;
    }
    let address = u32::from(address.parse::<std::net::Ipv4Addr>().ok()?);
    let mask = u32::MAX
        .checked_shl(u32::from(32 - prefix))
        .map_or(0, |mask| mask);
    Some((address & mask, prefix))
}

fn clone_log_matches(line: &str, chain: &str, out_iface: &str) -> bool {
    let stem = format!("-A {chain} -o {out_iface} -j LOG --log-prefix ");
    let Some(prefix) = line.strip_prefix(&stem) else {
        return false;
    };
    prefix.trim().trim_matches('"').trim() == drop_log_prefix(chain).trim()
}

/// Install the per-room egress chain for `tap` and splice its tap-keyed jump into
/// `ROOMS_FWD`, above the supernet egress ACCEPT.
///
/// A no-op for a non-enforcing plan. Fail-closed: the caller installs this
/// before the VMM can transmit, so a room asked to enforce that cannot must not
/// boot.
///
/// # Errors
/// When the tap is not a pool tap, `ROOMS_FWD` lacks its supernet egress ACCEPT
/// (host substrate missing), or any `iptables` call fails.
#[cfg(unix)]
pub fn install(tap: &str, plan: &Plan) -> Result<(), String> {
    let _transaction = lock_iptables_transaction()?;
    install_locked(tap, plan)
}

#[cfg(unix)]
fn install_locked(tap: &str, plan: &Plan) -> Result<(), String> {
    if !plan.enforces() {
        return Ok(());
    }
    let chain = chain_for_tap(tap).ok_or_else(|| format!("not a pool tap: '{tap}'"))?;
    // Clean up any stale chain/jump from a prior reuse of this slot index BEFORE
    // reading the dump: `remove` deletes the stale jump, which shifts every rule
    // below it up by one. Computing the insert position against the pre-cleanup
    // dump would then place the fresh jump one rank too low — below the supernet
    // egress ACCEPT with the normal layout — and `none`/non-allowlisted traffic
    // would leak. The position must be computed against the post-cleanup chain.
    if let Err(error) = remove_checked_locked(tap) {
        tracing::warn!(tap, %error, "stale egress cleanup incomplete before install");
    }
    let dump = dump_forward()?;
    let out_iface = out_iface(&dump).ok_or_else(|| {
        format!("{FWD_CHAIN} has no supernet egress ACCEPT; run setup-tap.sh --host")
    })?;
    let pos = insert_position(&dump)
        .ok_or_else(|| format!("{FWD_CHAIN}: cannot locate the egress ACCEPT to place the jump"))?;
    run_iptables(&["-N".to_owned(), chain.clone()])?;
    for rule in subchain_rules(&chain, &out_iface, plan.permitted()) {
        run_iptables(&rule)?;
    }
    run_iptables(&jump_rule(pos, tap, &chain))?;
    if matches!(plan, Plan::None) {
        run_iptables(&input_drop_insert_rule(tap))?;
    }
    Ok(())
}

#[cfg(not(unix))]
pub fn install(_tap: &str, _plan: &Plan) -> Result<(), String> {
    Err("egress enforcement requires iptables (unix only)".to_owned())
}

/// Install and live-verify a clone's veth-keyed egress policy.
///
/// Flat rooms continue through [`install`] unchanged. Clone enforcement is
/// spliced only into `ROOMS_VETH_FWD`, after its fixed DROP prefix and before
/// its upstream ACCEPT. Observe mode performs no mutation but still proves the
/// complete shared forwarding substrate. Any failed command or failed
/// post-install proof is cleaned up using only the clone's exact identities.
///
/// # Errors
/// When `veth` is not canonical `veth-h1..63`, the veth substrate is malformed,
/// an iptables operation fails, or the live dumps do not prove enforcement.
#[cfg(unix)]
pub fn install_clone(veth: &str, plan: &Plan) -> Result<(), String> {
    let _transaction = lock_iptables_transaction()?;
    install_clone_locked(veth, plan)
}

#[cfg(unix)]
fn install_clone_locked(veth: &str, plan: &Plan) -> Result<(), String> {
    let chain = clone_chain_for_veth(veth)
        .ok_or_else(|| format!("not a canonical clone veth: '{veth}'"))?;
    // Reuse of an allocator index must never inherit a prior owner's policy.
    // This exact cleanup is also the Observe mode's proof that "no policy"
    // means no stale jump or INPUT drop remains for this veth.
    remove_clone_checked_locked(veth)?;
    if !plan.enforces() {
        return verify_clone_locked(veth, plan);
    }
    let result = install_clone_fresh(veth, &chain, plan);
    let Err(install_error) = result else {
        return Ok(());
    };
    match remove_clone_checked_locked(veth) {
        Ok(()) => Err(install_error),
        Err(cleanup_error) => Err(format!(
            "{install_error}; exact clone-egress cleanup also failed: {cleanup_error}"
        )),
    }
}

#[cfg(unix)]
fn install_clone_fresh(veth: &str, chain: &str, plan: &Plan) -> Result<(), String> {
    let (_, _, veth_dump) = dump_verified_clone_substrate()?;
    if !clone_source_binding_present(&veth_dump, veth) {
        return Err(format!(
            "clone forwarding substrate lacks the exact source binding for {veth}"
        ));
    }
    let out_iface = clone_out_iface(&veth_dump).ok_or_else(|| {
        format!("{VETH_FWD_CHAIN} is malformed or lacks its upstream egress ACCEPT")
    })?;
    let position = clone_insert_position(&veth_dump).ok_or_else(|| {
        format!("{VETH_FWD_CHAIN}: cannot locate the clone-egress insertion point")
    })?;
    run_iptables(&["-N".to_owned(), chain.to_owned()])?;
    for rule in subchain_rules(chain, &out_iface, plan.permitted()) {
        run_iptables(&rule)?;
    }
    let jump = clone_jump_rule(position, veth)
        .ok_or_else(|| format!("invalid clone jump identity for '{veth}'"))?;
    run_iptables(&jump)?;
    if matches!(plan, Plan::None) {
        run_iptables(&input_drop_insert_rule(veth))?;
    }
    verify_clone_locked(veth, plan)
}

/// Re-prove a clone's live forwarding custody without mutating it.
///
/// Observe mode proves the complete shared substrate. Enforcing modes also
/// prove the canonical veth jump, exact ordered subchain, and `none` INPUT
/// drop. Restore calls this again immediately before Resume.
#[cfg(unix)]
pub fn verify_clone(veth: &str, plan: &Plan) -> Result<(), String> {
    let _transaction = lock_iptables_transaction()?;
    verify_clone_locked(veth, plan)
}

#[cfg(unix)]
fn verify_clone_locked(veth: &str, plan: &Plan) -> Result<(), String> {
    let chain = clone_chain_for_veth(veth)
        .ok_or_else(|| format!("not a canonical clone veth: '{veth}'"))?;
    let (_, _, veth_dump) = dump_verified_clone_substrate()?;
    if !clone_source_binding_present(&veth_dump, veth) {
        return Err(format!(
            "clone forwarding substrate lacks the exact source binding for {veth}"
        ));
    }
    if !plan.enforces() {
        if !clone_observe_unenforced(veth, &veth_dump, &dump_input()?) {
            return Err(format!(
                "observe-mode clone {veth} still has stale enforcing firewall state"
            ));
        }
        return Ok(());
    }
    let out_iface = clone_out_iface(&veth_dump).ok_or_else(|| {
        format!("{VETH_FWD_CHAIN} is malformed or lacks its upstream egress ACCEPT")
    })?;
    let eg_dump = dump_named_chain(&chain)?;
    let input_dump = dump_input()?;
    if clone_egress_enforced(veth, &out_iface, &veth_dump, &eg_dump, &input_dump, plan) {
        return Ok(());
    }
    Err(format!(
        "clone egress for {veth} is not enforced by the live iptables state"
    ))
}

#[cfg(not(unix))]
pub fn install_clone(_veth: &str, _plan: &Plan) -> Result<(), String> {
    Err("clone egress enforcement requires iptables (unix only)".to_owned())
}

#[cfg(not(unix))]
pub fn verify_clone(_veth: &str, _plan: &Plan) -> Result<(), String> {
    Err("clone egress verification requires iptables (unix only)".to_owned())
}

/// Remove a room's egress chain and its `ROOMS_FWD` jump, idempotently.
///
/// Scoped to the tap's `<k>` — so a double teardown or a gc race never touches a
/// live sibling's chain, and a tap that never enforced is a clean no-op.
#[cfg(unix)]
pub fn remove(tap: &str) {
    if let Err(error) = remove_checked(tap) {
        tracing::warn!(tap, %error, "egress cleanup incomplete");
    }
}

/// Remove a room's egress state and report any residual cleanup failure.
///
/// Snapshot publication uses this checked form because a frozen slot cannot be
/// advertised while its old base still owns a host firewall path.
#[cfg(unix)]
pub fn remove_checked(tap: &str) -> Result<(), String> {
    let _transaction = lock_iptables_transaction()?;
    remove_checked_locked(tap)
}

#[cfg(unix)]
fn remove_checked_locked(tap: &str) -> Result<(), String> {
    let Some(chain) = chain_for_tap(tap) else {
        return Ok(());
    };
    while iptables_exists(&input_drop_rule("-C", tap))? {
        run_iptables(&input_drop_rule("-D", tap))?;
    }
    // Probe the per-room subchain's existence FIRST. A jump can only exist
    // while its target chain does — iptables refuses to delete a referenced
    // chain — so when the subchain is absent (an Observe-plan room that never
    // installed egress), there is provably no jump to remove. Skipping the
    // jump `-C` probe in that case avoids the "Chain '…' does not exist" error
    // it would otherwise raise, without broadening the strict absent-rule
    // classifier (which must still surface a genuine inspection failure).
    if iptables_exists(&["-S".to_owned(), chain.clone()])? {
        // Delete the jump while present — a stale install could have left more
        // than one; the bounded loop clears every copy.
        while iptables_exists(&jump_args("-C", tap, &chain))? {
            run_iptables(&jump_args("-D", tap, &chain))?;
        }
        run_iptables(&["-F".to_owned(), chain.clone()])?;
        run_iptables(&["-X".to_owned(), chain])?;
    }
    Ok(())
}

#[cfg(not(unix))]
pub const fn remove(_tap: &str) {}

#[cfg(not(unix))]
pub const fn remove_checked(_tap: &str) -> Result<(), String> {
    Ok(())
}

/// Remove one clone's exact veth-keyed egress state, logging incomplete
/// cleanup. No broad interface, source, or caller-provided chain selector is
/// ever used.
#[cfg(unix)]
pub fn remove_clone(veth: &str) {
    if let Err(error) = remove_clone_checked(veth) {
        tracing::warn!(veth, %error, "clone egress cleanup incomplete");
    }
}

/// Checked clone-egress teardown.
///
/// The chain name is derived from the canonical veth. Cleanup removes every
/// copy of only the exact INPUT drop and exact veth-to-chain jump, then deletes
/// that chain. A malformed or mismatched broader reference is deliberately not
/// removed; it leaves `-X` failing so custody cleanup cannot be reported as
/// complete after touching an unowned rule.
///
/// # Errors
/// When `veth` is non-canonical or an iptables inspection/mutation fails.
#[cfg(unix)]
pub fn remove_clone_checked(veth: &str) -> Result<(), String> {
    let _transaction = lock_iptables_transaction()?;
    remove_clone_checked_locked(veth)
}

#[cfg(unix)]
fn remove_clone_checked_locked(veth: &str) -> Result<(), String> {
    let chain = clone_chain_for_veth(veth)
        .ok_or_else(|| format!("not a canonical clone veth: '{veth}'"))?;
    while iptables_exists(&input_drop_rule("-C", veth))? {
        run_iptables(&input_drop_rule("-D", veth))?;
    }
    if !iptables_exists(&["-S".to_owned(), chain.clone()])? {
        return Ok(());
    }
    let jump_check = clone_jump_args("-C", veth)
        .ok_or_else(|| format!("not a canonical clone veth: '{veth}'"))?;
    let jump_delete = clone_jump_args("-D", veth)
        .ok_or_else(|| format!("not a canonical clone veth: '{veth}'"))?;
    while iptables_exists(&jump_check)? {
        run_iptables(&jump_delete)?;
    }
    run_iptables(&["-F".to_owned(), chain.clone()])?;
    run_iptables(&["-X".to_owned(), chain])
}

#[cfg(not(unix))]
pub const fn remove_clone(_veth: &str) {}

#[cfg(not(unix))]
pub fn remove_clone_checked(veth: &str) -> Result<(), String> {
    clone_chain_for_veth(veth)
        .map(|_| ())
        .ok_or_else(|| format!("not a canonical clone veth: '{veth}'"))
}

#[cfg(unix)]
fn clone_jump_args(op: &str, veth: &str) -> Option<Vec<String>> {
    let chain = clone_chain_for_veth(veth)?;
    Some(vec![
        op.to_owned(),
        VETH_FWD_CHAIN.to_owned(),
        "-i".to_owned(),
        veth.to_owned(),
        "-j".to_owned(),
        chain,
    ])
}

/// The `iptables <op>` argument list for the tap-keyed jump — `-C` to test its
/// presence, `-D` to delete it.
#[cfg(unix)]
fn jump_args(op: &str, tap: &str, chain: &str) -> Vec<String> {
    vec![
        op.to_owned(),
        FWD_CHAIN.to_owned(),
        "-i".to_owned(),
        tap.to_owned(),
        "-j".to_owned(),
        chain.to_owned(),
    ]
}

/// Dump the live `ROOMS_FWD` chain (`iptables -S ROOMS_FWD`).
#[cfg(unix)]
fn dump_forward() -> Result<String, String> {
    dump_named_chain(FWD_CHAIN)
}

#[cfg(unix)]
fn dump_clone_forward() -> Result<String, String> {
    dump_named_chain(VETH_FWD_CHAIN)
}

#[cfg(unix)]
fn dump_clone_substrate() -> Result<(String, String, String), String> {
    Ok((
        dump_named_chain(HOST_FWD_CHAIN)?,
        dump_forward()?,
        dump_clone_forward()?,
    ))
}

#[cfg(unix)]
fn dump_verified_clone_substrate() -> Result<(String, String, String), String> {
    let dumps = dump_clone_substrate()?;
    if clone_forwarding_substrate(&dumps.0, &dumps.1, &dumps.2) {
        return Ok(dumps);
    }
    Err(
        "clone forwarding substrate is not the exact FORWARD -> ROOMS_FWD fallthrough -> ROOMS_VETH_FWD path"
            .to_owned(),
    )
}

#[cfg(unix)]
fn dump_input() -> Result<String, String> {
    dump_named_chain(INPUT_CHAIN)
}

/// Dump one already-validated filter-chain name.
#[cfg(unix)]
fn dump_named_chain(chain: &str) -> Result<String, String> {
    let args = ["-S".to_owned(), chain.to_owned()];
    let output = iptables_output(&args).map_err(|error| format!("iptables -S {chain}: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "iptables -S {chain} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Run `iptables <args>`, mapping a non-zero exit to a descriptive error.
#[cfg(unix)]
fn run_iptables(args: &[String]) -> Result<(), String> {
    let out = iptables_output(args).map_err(|e| format!("iptables {}: {e}", args.join(" ")))?;
    if out.status.success() {
        return Ok(());
    }
    Err(format!(
        "iptables {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr).trim()
    ))
}

/// Checked existence probe: absence is false, inability to inspect is an error.
#[cfg(unix)]
fn iptables_exists(args: &[String]) -> Result<bool, String> {
    let output =
        iptables_output(args).map_err(|error| format!("iptables {}: {error}", args.join(" ")))?;
    if output.status.success() {
        return Ok(true);
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if iptables_rule_is_absent(&stderr) {
        return Ok(false);
    }
    Err(format!(
        "iptables {} probe failed: {}",
        args.join(" "),
        stderr.trim()
    ))
}

/// Execute one iptables command with a bounded wait for the host-wide xtables
/// lock. The process-local transaction mutex remains outside this helper: a
/// task holds it across the whole read/plan/mutate/verify sequence while each
/// subprocess independently waits for external package managers or operators.
#[cfg(unix)]
fn iptables_output(args: &[String]) -> std::io::Result<std::process::Output> {
    std::process::Command::new("iptables")
        .args(iptables_command_args(args))
        .output()
}

#[cfg(unix)]
fn iptables_command_args(args: &[String]) -> Vec<String> {
    let mut bounded = Vec::with_capacity(args.len() + 2);
    bounded.extend(["-w".to_owned(), XTABLES_WAIT_SECONDS.to_owned()]);
    bounded.extend_from_slice(args);
    bounded
}

#[cfg(unix)]
fn lock_iptables_transaction() -> Result<MutexGuard<'static, ()>, String> {
    IPTABLES_TRANSACTION.lock().map_err(|_| {
        "iptables transaction lock is poisoned; refusing a partial firewall transaction".to_owned()
    })
}

#[cfg(any(unix, test))]
fn iptables_rule_is_absent(stderr: &str) -> bool {
    stderr.contains("Bad rule")
        || stderr.contains("No chain/target/match")
        || stderr.contains("No rule exists at that position")
        || (stderr.contains("rule in chain") && stderr.contains("not found"))
        // The `-S <chain>` existence probe on a per-room subchain that was
        // never installed (an Observe-plan room — no egress enforcement)
        // reports the missing chain under nf_tables as `Chain '…' does not
        // exist`. That is unambiguously "absent": only a query about a chain
        // that isn't there produces it — a genuine inspection failure is
        // permission/netlink/table wording, none of which name a Chain. The
        // pair `Chain` + `does not exist` also excludes `Table does not exist`.
        || (stderr.contains("Chain") && stderr.contains("does not exist"))
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        reason = "test module"
    )]

    use super::{
        chain_for_tap, clone_chain_for_veth, clone_egress_enforced, clone_insert_position,
        clone_jump_rule, clone_observe_unenforced, clone_out_iface, destinations_equivalent,
        input_drop_insert_rule, input_drop_rule, insert_position, iptables_rule_is_absent,
        jump_rule, out_iface, parse, resolve, room_egress_enforced, subchain_rules, Dest, Plan,
        Policy,
    };
    #[cfg(unix)]
    use super::{clone_jump_args, iptables_command_args};

    /// A correctly-wired `ROOMS_FWD` dump — the substrate layout plus a
    /// tap-keyed jump for slot 1 placed above the supernet egress ACCEPT and
    /// below the isolation DROP.
    const ENFORCED_FORWARD: &str = concat!(
        "-N ROOMS_FWD\n",
        "-A ROOMS_FWD -s 172.16.0.0/24 -d 172.16.0.0/24 -j DROP\n",
        "-A ROOMS_FWD -s 172.16.0.0/24 -d 10.0.0.0/8 -j DROP\n",
        "-A ROOMS_FWD -i tap-fc1 -j ROOMS_EG_1\n",
        "-A ROOMS_FWD -s 172.16.0.0/24 -o eth0 -j ACCEPT\n",
        "-A ROOMS_FWD -i eth0 -d 172.16.0.0/24 -m state --state RELATED,ESTABLISHED -j ACCEPT\n",
        "-A ROOMS_FWD -s 172.16.0.0/24 -m comment --comment \"rooms:fwd:v1:172.16.0.0/24\" -j DROP",
    );

    /// A well-formed per-room sub-chain dump (allowlist of one dest).
    const GOOD_EG: &str = concat!(
        "-N ROOMS_EG_1\n",
        "-A ROOMS_EG_1 -d 1.2.3.4 -o eth0 -j ACCEPT\n",
        "-A ROOMS_EG_1 -o eth0 -j LOG --log-prefix \"rooms-egress-drop:1 \"\n",
        "-A ROOMS_EG_1 -o eth0 -j DROP",
    );

    const CLONE_FORWARD: &str = concat!(
        "-N ROOMS_VETH_FWD\n",
        "-A ROOMS_VETH_FWD ! -s 172.17.0.14/32 -i veth-h3 -j DROP\n",
        "-A ROOMS_VETH_FWD ! -s 172.17.0.0/24 -i veth-h+ -j DROP\n",
        "-A ROOMS_VETH_FWD -s 172.17.0.0/24 -d 172.17.0.0/24 -j DROP\n",
        "-A ROOMS_VETH_FWD -s 172.17.0.0/24 -d 10.0.0.0/8 -j DROP\n",
        "-A ROOMS_VETH_FWD -s 172.17.0.0/24 -d 192.168.0.0/16 -j DROP\n",
        "-A ROOMS_VETH_FWD -s 172.17.0.0/24 -d 172.16.0.0/12 -j DROP\n",
        "-A ROOMS_VETH_FWD -i veth-h3 -j ROOMS_CEG_3\n",
        "-A ROOMS_VETH_FWD -s 172.17.0.0/24 -o eth0 -j ACCEPT\n",
        "-A ROOMS_VETH_FWD -d 172.17.0.0/24 -i eth0 -m state --state RELATED,ESTABLISHED -j ACCEPT\n",
        "-A ROOMS_VETH_FWD -s 172.17.0.0/24 -j DROP",
    );

    const CLONE_NONE_EG: &str = concat!(
        "-N ROOMS_CEG_3\n",
        "-A ROOMS_CEG_3 -o eth0 -j LOG --log-prefix \"rooms-egress-drop:3 \"\n",
        "-A ROOMS_CEG_3 -o eth0 -j DROP",
    );

    const CLONE_ALLOWLIST_EG: &str = concat!(
        "-N ROOMS_CEG_3\n",
        "-A ROOMS_CEG_3 -d 1.2.3.4/32 -o eth0 -j ACCEPT\n",
        "-A ROOMS_CEG_3 -d 10.0.0.0/24 -o eth0 -j ACCEPT\n",
        "-A ROOMS_CEG_3 -o eth0 -j LOG --log-prefix \"rooms-egress-drop:3 \"\n",
        "-A ROOMS_CEG_3 -o eth0 -j DROP",
    );

    const CLONE_INPUT_DROP: &str = concat!(
        "-P INPUT ACCEPT\n",
        "-A INPUT -i lo -j ACCEPT\n",
        "-A INPUT -i veth-h3 -j DROP\n",
        "-A INPUT -j ACCEPT",
    );

    #[test]
    fn none_host_local_drop_is_keyed_on_the_tap() {
        assert_eq!(
            input_drop_insert_rule("tap-fc7"),
            ["-I", "INPUT", "1", "-i", "tap-fc7", "-j", "DROP"]
        );
        assert_eq!(
            input_drop_rule("-D", "tap-fc7"),
            ["-D", "INPUT", "-i", "tap-fc7", "-j", "DROP"]
        );
    }

    #[test]
    fn missing_rule_probe_accepts_legacy_and_nft_wording() {
        assert!(iptables_rule_is_absent(
            "iptables: Bad rule (does a matching rule exist in that chain?)."
        ));
        assert!(iptables_rule_is_absent(
            "iptables: No chain/target/match by that name."
        ));
        assert!(iptables_rule_is_absent(
            "iptables-nft: No rule exists at that position."
        ));
        assert!(iptables_rule_is_absent(
            "iptables v1.8.10 (nf_tables): rule in chain INPUT not found"
        ));
        // A `-C` probe for a jump whose target subchain was never installed
        // (an Observe-plan / non-egress restore) reports the missing chain, not
        // a missing rule. That must read as absent — otherwise checked teardown
        // errors and a restore lease is never returned.
        assert!(iptables_rule_is_absent(
            "iptables v1.8.10 (nf_tables): Chain 'ROOMS_EG_1' does not exist"
        ));
        assert!(
            !iptables_rule_is_absent("iptables: Permission denied (you must be root)"),
            "inspection failures must remain errors"
        );
    }

    // --- parse ---

    #[test]
    fn parse_none_and_allowlist_shapes() {
        assert_eq!(parse("none").unwrap(), Policy::None);
        assert_eq!(
            parse("allowlist:api.anthropic.com").unwrap(),
            Policy::Allowlist(vec![Dest::Host("api.anthropic.com".to_owned())])
        );
        assert_eq!(
            parse("allowlist:api.anthropic.com,10.1.2.0/24").unwrap(),
            Policy::Allowlist(vec![
                Dest::Host("api.anthropic.com".to_owned()),
                Dest::Cidr("10.1.2.0/24".to_owned()),
            ])
        );
        // A bare IPv4 literal pins as a CIDR-style `-d` match.
        assert_eq!(
            parse("allowlist:1.2.3.4").unwrap(),
            Policy::Allowlist(vec![Dest::Cidr("1.2.3.4".to_owned())])
        );
    }

    #[test]
    fn parse_rejects_bad_inputs() {
        assert!(parse("allowlist:").is_err(), "empty allowlist");
        assert!(parse("allowlist:a,,b").is_err(), "empty entry");
        assert!(
            parse("allowlist:1.2.3.0/99").is_err(),
            "prefix out of range"
        );
        assert!(
            parse("allowlist:1.2.3/24").is_err(),
            "malformed CIDR address"
        );
        assert!(parse("blocklist:x").is_err(), "unknown mode");
        assert!(parse("").is_err(), "empty");
    }

    #[test]
    fn resolve_maps_modes_and_pins_cidrs() {
        assert_eq!(resolve(&Policy::Observe).unwrap(), Plan::Observe);
        assert_eq!(resolve(&Policy::None).unwrap(), Plan::None);
        // A CIDR/IP literal resolves with no DNS — deterministic in CI.
        let policy = Policy::Allowlist(vec![
            Dest::Cidr("10.1.2.0/24".to_owned()),
            Dest::Cidr("1.2.3.4".to_owned()),
        ]);
        assert_eq!(
            resolve(&policy).unwrap(),
            Plan::Allowlist(vec!["10.1.2.0/24".to_owned(), "1.2.3.4".to_owned()])
        );
    }

    // --- chain synthesis ---

    #[test]
    fn chain_name_derives_from_the_tap_only_for_pool_taps() {
        assert_eq!(chain_for_tap("tap-fc1").as_deref(), Some("ROOMS_EG_1"));
        assert_eq!(chain_for_tap("tap-fc42").as_deref(), Some("ROOMS_EG_42"));
        assert_eq!(chain_for_tap("tap-fc0").as_deref(), Some("ROOMS_EG_0"));
        assert_eq!(chain_for_tap("eth0"), None, "not a pool tap");
        assert_eq!(chain_for_tap("tap-fc"), None, "no index");
        assert_eq!(chain_for_tap("tap-fcx"), None, "non-numeric index");
    }

    #[test]
    fn none_appends_log_then_drop() {
        let rules = subchain_rules("ROOMS_EG_1", "eth0", &[]);
        let lines: Vec<String> = rules.iter().map(|r| r.join(" ")).collect();
        assert_eq!(
            lines,
            vec![
                "-A ROOMS_EG_1 -o eth0 -j LOG --log-prefix rooms-egress-drop:1 ".to_owned(),
                "-A ROOMS_EG_1 -o eth0 -j DROP".to_owned(),
            ]
        );
    }

    #[test]
    fn allowlist_appends_accept_per_dest_then_drop() {
        let permitted = vec!["1.2.3.4".to_owned(), "10.0.0.0/24".to_owned()];
        let rules = subchain_rules("ROOMS_EG_2", "eth0", &permitted);
        let lines: Vec<String> = rules.iter().map(|r| r.join(" ")).collect();
        assert_eq!(
            lines,
            vec![
                "-A ROOMS_EG_2 -d 1.2.3.4 -o eth0 -j ACCEPT".to_owned(),
                "-A ROOMS_EG_2 -d 10.0.0.0/24 -o eth0 -j ACCEPT".to_owned(),
                "-A ROOMS_EG_2 -o eth0 -j LOG --log-prefix rooms-egress-drop:2 ".to_owned(),
                "-A ROOMS_EG_2 -o eth0 -j DROP".to_owned(),
            ],
            "accepts precede the terminal log+drop, all scoped by -o eth0"
        );
    }

    #[test]
    fn the_jump_names_the_tap_not_the_source() {
        let jump = jump_rule(4, "tap-fc1", "ROOMS_EG_1").join(" ");
        assert_eq!(jump, "-I ROOMS_FWD 4 -i tap-fc1 -j ROOMS_EG_1");
        assert!(!jump.contains("-s "), "the jump must not key on source");
    }

    #[test]
    fn insert_position_and_out_iface_read_the_supernet_accept() {
        // The egress ACCEPT is the 4th -A rule in ENFORCED_FORWARD.
        assert_eq!(insert_position(ENFORCED_FORWARD), Some(4));
        assert_eq!(out_iface(ENFORCED_FORWARD).as_deref(), Some("eth0"));
    }

    #[test]
    fn insert_position_shifts_once_a_stale_jump_is_removed() {
        // The P1 hazard: a stale jump above the supernet ACCEPT inflates the
        // ACCEPT's rank. Installing at that *pre-cleanup* rank after removing the
        // stale jump would land the fresh jump one row too low — below the
        // ACCEPT → leak. `install` must read the position from the post-cleanup
        // dump; this pins the arithmetic that makes the ordering matter.
        let with_stale = concat!(
            "-N ROOMS_FWD\n",
            "-A ROOMS_FWD -s 172.16.0.0/24 -d 172.16.0.0/24 -j DROP\n",
            "-A ROOMS_FWD -i tap-fc1 -j ROOMS_EG_1\n",
            "-A ROOMS_FWD -s 172.16.0.0/24 -o eth0 -j ACCEPT",
        );
        let post_cleanup = concat!(
            "-N ROOMS_FWD\n",
            "-A ROOMS_FWD -s 172.16.0.0/24 -d 172.16.0.0/24 -j DROP\n",
            "-A ROOMS_FWD -s 172.16.0.0/24 -o eth0 -j ACCEPT",
        );
        assert_eq!(insert_position(with_stale), Some(3));
        assert_eq!(
            insert_position(post_cleanup),
            Some(2),
            "removing the stale jump shifts the ACCEPT up one — install must use this rank"
        );
    }

    #[test]
    fn clone_chain_jump_and_insertion_identity_are_exact() {
        assert_eq!(
            clone_chain_for_veth("veth-h3").as_deref(),
            Some("ROOMS_CEG_3")
        );
        assert_eq!(clone_insert_position(CLONE_FORWARD), Some(7));
        assert_eq!(clone_out_iface(CLONE_FORWARD).as_deref(), Some("eth0"));
        assert_eq!(
            clone_jump_rule(7, "veth-h3").unwrap(),
            [
                "-I",
                "ROOMS_VETH_FWD",
                "7",
                "-i",
                "veth-h3",
                "-j",
                "ROOMS_CEG_3"
            ]
        );

        for invalid in ["veth-h", "veth-h0", "veth-h03", "veth-h64", "veth-h+"] {
            assert_eq!(clone_chain_for_veth(invalid), None, "accepted {invalid}");
            assert_eq!(clone_jump_rule(7, invalid), None, "jumped {invalid}");
        }
        assert_eq!(clone_jump_rule(0, "veth-h3"), None);
    }

    #[test]
    fn clone_subchain_reuses_ordered_policy_synthesis() {
        let permitted = vec!["1.2.3.4".to_owned()];
        let lines: Vec<String> = subchain_rules("ROOMS_CEG_3", "eth0", &permitted)
            .iter()
            .map(|rule| rule.join(" "))
            .collect();
        assert_eq!(
            lines,
            [
                "-A ROOMS_CEG_3 -d 1.2.3.4 -o eth0 -j ACCEPT".to_owned(),
                "-A ROOMS_CEG_3 -o eth0 -j LOG --log-prefix rooms-egress-drop:3 ".to_owned(),
                "-A ROOMS_CEG_3 -o eth0 -j DROP".to_owned(),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn clone_removal_selectors_are_exact_and_derived() {
        assert_eq!(
            clone_jump_args("-D", "veth-h3").unwrap(),
            ["-D", "ROOMS_VETH_FWD", "-i", "veth-h3", "-j", "ROOMS_CEG_3"]
        );
        assert_eq!(
            input_drop_rule("-D", "veth-h3"),
            ["-D", "INPUT", "-i", "veth-h3", "-j", "DROP"]
        );
        assert_eq!(clone_jump_args("-D", "veth-h+"), None);
        assert_eq!(clone_jump_args("-D", "veth-h03"), None);
    }

    #[cfg(unix)]
    #[test]
    fn every_iptables_subprocess_has_a_bounded_xtables_wait() {
        assert_eq!(
            iptables_command_args(&["-S".to_owned(), "ROOMS_VETH_FWD".to_owned()]),
            ["-w", "2", "-S", "ROOMS_VETH_FWD"]
        );
    }

    #[test]
    fn clone_insertion_targets_the_head_of_the_allowed_jump_block() {
        let without_jump =
            CLONE_FORWARD.replace("-A ROOMS_VETH_FWD -i veth-h3 -j ROOMS_CEG_3\n", "");
        assert_eq!(clone_insert_position(&without_jump), Some(7));

        let malformed = CLONE_FORWARD.replace(
            "-A ROOMS_VETH_FWD -i veth-h3 -j ROOMS_CEG_3",
            "-A ROOMS_VETH_FWD -i veth-h+ -j ROOMS_CEG_3",
        );
        assert_eq!(clone_insert_position(&malformed), None);
        assert_eq!(clone_out_iface(&malformed), None);
    }

    #[test]
    fn clone_none_requires_forward_subchain_and_exact_input_enforcement() {
        assert!(clone_egress_enforced(
            "veth-h3",
            "eth0",
            CLONE_FORWARD,
            CLONE_NONE_EG,
            CLONE_INPUT_DROP,
            &Plan::None,
        ));
        for input in [
            "-P INPUT ACCEPT",
            "-A INPUT -i veth-h8 -j DROP",
            "-A INPUT -j ACCEPT\n-A INPUT -i veth-h3 -j DROP",
            "-A INPUT -i veth-h+ -j DROP",
        ] {
            assert!(!clone_egress_enforced(
                "veth-h3",
                "eth0",
                CLONE_FORWARD,
                CLONE_NONE_EG,
                input,
                &Plan::None,
            ));
        }
    }

    #[test]
    fn clone_allowlist_accepts_only_the_resolved_ordered_destinations() {
        let plan = Plan::Allowlist(vec!["1.2.3.4".to_owned(), "10.0.0.1/24".to_owned()]);
        assert!(clone_egress_enforced(
            "veth-h3",
            "eth0",
            CLONE_FORWARD,
            CLONE_ALLOWLIST_EG,
            "",
            &plan,
        ));
        assert!(destinations_equivalent("1.2.3.4", "1.2.3.4/32"));
        assert!(destinations_equivalent("10.0.0.1/24", "10.0.0.0/24"));
        assert!(!destinations_equivalent("10.0.1.0/24", "10.0.0.0/24"));

        let reversed = Plan::Allowlist(vec!["10.0.0.0/24".to_owned(), "1.2.3.4".to_owned()]);
        assert!(!clone_egress_enforced(
            "veth-h3",
            "eth0",
            CLONE_FORWARD,
            CLONE_ALLOWLIST_EG,
            "",
            &reversed,
        ));
    }

    #[test]
    fn clone_enforcement_rejects_leaky_or_broadened_subchains() {
        let plan = Plan::Allowlist(vec!["1.2.3.4".to_owned(), "10.0.0.0/24".to_owned()]);
        let mutations = [
            CLONE_ALLOWLIST_EG.replace("-A ROOMS_CEG_3 -o eth0 -j DROP", ""),
            CLONE_ALLOWLIST_EG.replace(
                "-A ROOMS_CEG_3 -o eth0 -j LOG",
                "-A ROOMS_CEG_3 -j ACCEPT\n-A ROOMS_CEG_3 -o eth0 -j LOG",
            ),
            CLONE_ALLOWLIST_EG.replace("-d 1.2.3.4/32", "-d 0.0.0.0/0"),
            CLONE_ALLOWLIST_EG.replace("-o eth0 -j DROP", "-o eth1 -j DROP"),
            CLONE_ALLOWLIST_EG.replace("-N ROOMS_CEG_3", "-N ROOMS_CEG_4"),
            format!("{CLONE_ALLOWLIST_EG}\n-A ROOMS_CEG_3 -j ACCEPT"),
        ];
        for broken in mutations {
            assert!(!clone_egress_enforced(
                "veth-h3",
                "eth0",
                CLONE_FORWARD,
                &broken,
                "",
                &plan,
            ));
        }
    }

    #[test]
    fn clone_enforcement_rejects_wrong_interface_chain_or_observe_plan() {
        let mismatched_jump =
            CLONE_FORWARD.replace("-i veth-h3 -j ROOMS_CEG_3", "-i veth-h3 -j ROOMS_CEG_8");
        assert!(!clone_egress_enforced(
            "veth-h3",
            "eth0",
            &mismatched_jump,
            CLONE_NONE_EG,
            CLONE_INPUT_DROP,
            &Plan::None,
        ));
        assert!(!clone_egress_enforced(
            "veth-h8",
            "eth0",
            CLONE_FORWARD,
            CLONE_NONE_EG,
            CLONE_INPUT_DROP,
            &Plan::None,
        ));
        assert!(!clone_egress_enforced(
            "veth-h3",
            "eth1",
            CLONE_FORWARD,
            CLONE_NONE_EG,
            CLONE_INPUT_DROP,
            &Plan::None,
        ));
        assert!(!clone_egress_enforced(
            "veth-h3",
            "eth0",
            CLONE_FORWARD,
            CLONE_NONE_EG,
            CLONE_INPUT_DROP,
            &Plan::Observe,
        ));
    }

    #[test]
    fn clone_observe_requires_its_exact_enforcement_to_be_absent() {
        let without_jump =
            CLONE_FORWARD.replace("-A ROOMS_VETH_FWD -i veth-h3 -j ROOMS_CEG_3\n", "");
        assert!(clone_observe_unenforced(
            "veth-h3",
            &without_jump,
            "-P INPUT ACCEPT"
        ));
        assert!(!clone_observe_unenforced(
            "veth-h3",
            CLONE_FORWARD,
            "-P INPUT ACCEPT"
        ));
        assert!(!clone_observe_unenforced(
            "veth-h3",
            &without_jump,
            CLONE_INPUT_DROP
        ));
    }

    // --- enforcement predicate (isolation.rs style): the load-bearing refutations ---

    #[test]
    fn enforced_when_tap_jump_precedes_supernet_accept() {
        assert!(room_egress_enforced(
            "tap-fc1",
            "eth0",
            ENFORCED_FORWARD,
            GOOD_EG
        ));
    }

    #[test]
    fn a_jump_below_the_supernet_accept_leaks() {
        // The jump sits AFTER the supernet ACCEPT — the room reaches the
        // permissive ACCEPT before its own chain, so `none`/allowlist leaks.
        let broken = concat!(
            "-N ROOMS_FWD\n",
            "-A ROOMS_FWD -s 172.16.0.0/24 -d 172.16.0.0/24 -j DROP\n",
            "-A ROOMS_FWD -s 172.16.0.0/24 -o eth0 -j ACCEPT\n",
            "-A ROOMS_FWD -i tap-fc1 -j ROOMS_EG_1",
        );
        assert!(!room_egress_enforced("tap-fc1", "eth0", broken, GOOD_EG));
    }

    #[test]
    fn a_source_keyed_jump_is_spoofable() {
        // A jump keyed on the guest source, not the tap: a compromised guest
        // forging another 172.16.0.x source dodges it. Must NOT read enforced.
        let broken = concat!(
            "-N ROOMS_FWD\n",
            "-A ROOMS_FWD -s 172.16.0.0/24 -d 172.16.0.0/24 -j DROP\n",
            "-A ROOMS_FWD -s 172.16.0.6 -j ROOMS_EG_1\n",
            "-A ROOMS_FWD -s 172.16.0.0/24 -o eth0 -j ACCEPT",
        );
        assert!(!room_egress_enforced("tap-fc1", "eth0", broken, GOOD_EG));
    }

    #[test]
    fn a_subchain_without_catch_all_drop_falls_through() {
        // The sub-chain's last rule is an ACCEPT — a non-matching packet falls
        // off the end back to the supernet ACCEPT.
        let leaky_eg = concat!(
            "-N ROOMS_EG_1\n",
            "-A ROOMS_EG_1 -d 1.2.3.4 -o eth0 -j ACCEPT",
        );
        assert!(!room_egress_enforced(
            "tap-fc1",
            "eth0",
            ENFORCED_FORWARD,
            leaky_eg
        ));
    }

    #[test]
    fn a_jump_above_the_isolation_drop_overrides_isolation() {
        // The jump sits ABOVE the guest↔guest isolation DROP — an allowlist
        // ACCEPT could then let a room reach a sibling's /30 first.
        let broken = concat!(
            "-N ROOMS_FWD\n",
            "-A ROOMS_FWD -i tap-fc1 -j ROOMS_EG_1\n",
            "-A ROOMS_FWD -s 172.16.0.0/24 -d 172.16.0.0/24 -j DROP\n",
            "-A ROOMS_FWD -s 172.16.0.0/24 -o eth0 -j ACCEPT",
        );
        assert!(!room_egress_enforced("tap-fc1", "eth0", broken, GOOD_EG));
    }

    #[test]
    fn a_missing_jump_is_not_enforced() {
        let no_jump = concat!(
            "-N ROOMS_FWD\n",
            "-A ROOMS_FWD -s 172.16.0.0/24 -d 172.16.0.0/24 -j DROP\n",
            "-A ROOMS_FWD -s 172.16.0.0/24 -o eth0 -j ACCEPT",
        );
        assert!(!room_egress_enforced("tap-fc1", "eth0", no_jump, GOOD_EG));
    }

    #[test]
    fn plan_permitted_and_enforces() {
        assert!(!Plan::Observe.enforces());
        assert!(Plan::None.enforces());
        assert!(Plan::Allowlist(vec!["1.2.3.4".to_owned()]).enforces());
        assert!(Plan::None.permitted().is_empty());
        assert_eq!(
            Plan::Allowlist(vec!["1.2.3.4".to_owned()]).permitted(),
            &["1.2.3.4".to_owned()]
        );
    }
}
