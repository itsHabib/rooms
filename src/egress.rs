//! Per-room egress policy: parse, chain synthesis, and the enforcement
//! predicate — plus the host-side install/remove mechanism.
//!
//! The witness ([`crate::witness`]) *observes* what a room's tap emitted; this
//! module turns that surface into an *enforcer* of what the room can reach. A
//! room's egress is policed on the host by a dedicated `ROOMS_EG_<k>` filter
//! chain, jumped from `ROOMS_FWD` on the room's **ingress tap** `tap-fc<k>`.
//!
//! Keying on the tap, not the guest's source IP, is load-bearing. The guest
//! runs untrusted, root-capable code and can forge its IPv4 source — a rule
//! keyed on `-s <guest_ip>` is a spoofing bypass (a forged sibling source dodges
//! the per-room DROP and falls through to the shared supernet ACCEPT). The one
//! thing the guest cannot forge is which interface its packets physically
//! transit; the tap name is therefore the anchor, the same unforgeable surface
//! the witness captures on.
//!
//! The synthesis and ordering logic is pure functions over `iptables -S` dumps,
//! mirroring [`crate::isolation`], so the negative assertions — every way the
//! enforcement can silently fail to bite — are unit-tested against
//! deliberately-broken inputs in CI, not merely observed live on the host.

use crate::isolation::SUPERNET;

/// The host `ROOMS_FWD` chain the per-room jump is spliced into.
const FWD_CHAIN: &str = "ROOMS_FWD";

/// Prefix of a pool tap name (`tap-fc<k>`); the `<k>` suffix keys the chain.
const TAP_PREFIX: &str = "tap-fc";

/// Prefix of a per-room egress chain name (`ROOMS_EG_<k>`).
const EG_CHAIN_PREFIX: &str = "ROOMS_EG_";

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

/// Ordered rules for the per-room sub-chain, **appended** top-to-bottom.
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
    let k = chain.strip_prefix(EG_CHAIN_PREFIX).unwrap_or(chain);
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
    remove(tap);
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
    run_iptables(&jump_rule(pos, tap, &chain))
}

#[cfg(not(unix))]
pub fn install(_tap: &str, _plan: &Plan) -> Result<(), String> {
    Err("egress enforcement requires iptables (unix only)".to_owned())
}

/// Remove a room's egress chain and its `ROOMS_FWD` jump, idempotently.
///
/// Scoped to the tap's `<k>` — so a double teardown or a gc race never touches a
/// live sibling's chain, and a tap that never enforced is a clean no-op.
#[cfg(unix)]
pub fn remove(tap: &str) {
    let Some(chain) = chain_for_tap(tap) else {
        return;
    };
    // Delete the jump while present — a stale install could have left more than
    // one; the bounded loop clears every copy.
    while iptables_ok(&jump_args("-C", tap, &chain)) {
        if run_iptables(&jump_args("-D", tap, &chain)).is_err() {
            break;
        }
    }
    let _ = run_iptables(&["-F".to_owned(), chain.clone()]);
    let _ = run_iptables(&["-X".to_owned(), chain]);
}

#[cfg(not(unix))]
pub const fn remove(_tap: &str) {}

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
    let out = std::process::Command::new("iptables")
        .args(["-S", FWD_CHAIN])
        .output()
        .map_err(|e| format!("iptables -S {FWD_CHAIN}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "iptables -S {FWD_CHAIN} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Run `iptables <args>`, mapping a non-zero exit to a descriptive error.
#[cfg(unix)]
fn run_iptables(args: &[String]) -> Result<(), String> {
    let out = std::process::Command::new("iptables")
        .args(args)
        .output()
        .map_err(|e| format!("iptables {}: {e}", args.join(" ")))?;
    if out.status.success() {
        return Ok(());
    }
    Err(format!(
        "iptables {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr).trim()
    ))
}

/// True when `iptables <args>` exits zero (used for the `-C` existence probe).
#[cfg(unix)]
fn iptables_ok(args: &[String]) -> bool {
    std::process::Command::new("iptables")
        .args(args)
        .output()
        .is_ok_and(|o| o.status.success())
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
        chain_for_tap, insert_position, jump_rule, out_iface, parse, resolve, room_egress_enforced,
        subchain_rules, Dest, Plan, Policy,
    };

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
