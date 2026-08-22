//! Pure verification of the additive clone-veth firewall substrate.
//!
//! `ROOMS_VETH_FWD` is deliberately separate from the flat `ROOMS_FWD` path.
//! These predicates prove the veth chain is entered only for rooms interfaces,
//! blocks clone-to-clone traffic before any ACCEPT, retains private-network
//! deny rules, and carries both halves of the upstream forwarding path. The
//! per-clone INPUT predicate covers the `none` posture's guest-to-host boundary.

macro_rules! veth_supernet {
    () => {
        "172.17.0.0/24"
    };
}

pub const VETH_SUPERNET: &str = veth_supernet!();
pub const VETH_ISOLATION_DROP: &str = concat!(
    "-A ROOMS_VETH_FWD -s ",
    veth_supernet!(),
    " -d ",
    veth_supernet!(),
    " -j DROP"
);
pub const VETH_ANTISPOOF_DROP: &str = concat!(
    "-A ROOMS_VETH_FWD ! -s ",
    veth_supernet!(),
    " -i veth-h+ -j DROP"
);
pub const FLAT_FORWARD_JUMP: &str = "-A FORWARD -j ROOMS_FWD";
pub const VETH_INGRESS_JUMP: &str = "-A FORWARD -i veth-h+ -j ROOMS_VETH_FWD";
pub const VETH_EGRESS_JUMP: &str = "-A FORWARD -o veth-h+ -j ROOMS_VETH_FWD";
const FLAT_SUPERNET: &str = "172.16.0.0/24";
const VETH_TAIL_DROP: &str = concat!("-A ROOMS_VETH_FWD -s ", veth_supernet!(), " -j DROP");
const CLONE_EGRESS_CHAIN_PREFIX: &str = "ROOMS_CEG_";
const CLONE_VETH_PREFIX: &str = "veth-h";
const MAX_CLONE_VETH_INDEX: u8 = 63;
const PRIVATE_DROPS: [&str; 3] = [
    concat!(
        "-A ROOMS_VETH_FWD -s ",
        veth_supernet!(),
        " -d 10.0.0.0/8 -j DROP"
    ),
    concat!(
        "-A ROOMS_VETH_FWD -s ",
        veth_supernet!(),
        " -d 192.168.0.0/16 -j DROP"
    ),
    concat!(
        "-A ROOMS_VETH_FWD -s ",
        veth_supernet!(),
        " -d 172.16.0.0/12 -j DROP"
    ),
];

#[must_use]
pub fn forward_jumps_ordered(forward_dump: &str) -> bool {
    let mut rules = forward_dump
        .lines()
        .filter(|line| line.starts_with("-A FORWARD "))
        .map(str::trim);
    let ordered = matches!(
        (rules.next(), rules.next(), rules.next()),
        (
            Some(FLAT_FORWARD_JUMP),
            Some(VETH_INGRESS_JUMP),
            Some(VETH_EGRESS_JUMP)
        )
    );
    ordered && rules.all(|rule| !jumps_to(rule, "ROOMS_FWD") && !jumps_to(rule, "ROOMS_VETH_FWD"))
}

#[must_use]
pub fn flat_chain_falls_through_for_veth(chain_dump: &str) -> bool {
    let mut seen = false;
    for line in chain_dump
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("-A ROOMS_FWD "))
    {
        seen = true;
        if !flat_rule_is_disjoint_from_veth(line) {
            return false;
        }
    }
    seen
}

/// Prove the complete root-namespace path clone traffic must traverse.
///
/// The three predicates are intentionally composed at the live custody
/// boundary rather than checked independently by host setup alone: an exact
/// `ROOMS_VETH_FWD` chain is inert if a preceding FORWARD or flat-chain rule
/// can terminate clone traffic before it reaches that chain.
#[must_use]
pub fn clone_forwarding_substrate(forward_dump: &str, flat_dump: &str, veth_dump: &str) -> bool {
    forward_jumps_ordered(forward_dump)
        && flat_chain_falls_through_for_veth(flat_dump)
        && rooms_veth_fwd_isolates(veth_dump)
}

/// A flat rule is safe ahead of the clone-veth jump only when it is provably
/// disjoint from clone traffic. Flat-source rules cannot match a clone's
/// 172.17/24 outer identity. Dynamic flat egress jumps are keyed to one exact
/// `tap-fcN`. The flat return rule is destination-scoped and keyed to one exact
/// upstream interface. Negation and wildcard interfaces are rejected because
/// their complement can include a clone veth.
fn flat_rule_is_disjoint_from_veth(line: &str) -> bool {
    if line.split_whitespace().any(|token| token == "!") {
        return false;
    }
    if has_token_pair(line, "-s", FLAT_SUPERNET) {
        return true;
    }
    let input = input_interface(line);
    if input.is_some_and(is_exact_flat_tap) {
        return true;
    }
    has_token_pair(line, "-d", FLAT_SUPERNET)
        && input
            .is_some_and(|interface| !interface.contains('+') && !interface.starts_with("veth-h"))
}

fn is_exact_flat_tap(interface: &str) -> bool {
    interface.strip_prefix("tap-fc").is_some_and(|suffix| {
        !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
    })
}

fn has_token_pair(line: &str, key: &str, value: &str) -> bool {
    let mut tokens = line.split_whitespace();
    while let Some(token) = tokens.next() {
        if token == key && tokens.next() == Some(value) {
            return true;
        }
    }
    false
}

fn jumps_to(line: &str, chain: &str) -> bool {
    has_token_pair(line, "-j", chain) || has_token_pair(line, "-g", chain)
}

#[must_use]
pub fn rooms_veth_fwd_isolates(chain_dump: &str) -> bool {
    let mut rules = chain_dump
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("-A ROOMS_VETH_FWD "))
        .peekable();
    let mut bindings = Vec::new();
    while let Some(index) = rules.peek().and_then(|line| per_veth_source_index(line)) {
        if bindings.contains(&index) {
            return false;
        }
        bindings.push(index);
        rules.next();
    }
    let fixed_prefix = rules.next() == Some(VETH_ANTISPOOF_DROP)
        && rules.next() == Some(VETH_ISOLATION_DROP)
        && rules.next() == Some(PRIVATE_DROPS[0])
        && rules.next() == Some(PRIVATE_DROPS[1])
        && rules.next() == Some(PRIVATE_DROPS[2]);
    let mut clone_jumps = Vec::new();
    while let Some(index) = rules.peek().and_then(|line| clone_egress_jump_index(line)) {
        if clone_jumps.contains(&index) || !bindings.contains(&index) {
            return false;
        }
        clone_jumps.push(index);
        rules.next();
    }
    let egress = rules.next().and_then(egress_interface);
    let returned = rules.next().and_then(return_interface);
    fixed_prefix
        && matches!((egress, returned), (Some(out), Some(input)) if out == input)
        && rules.next() == Some(VETH_TAIL_DROP)
        && rules.next().is_none()
}

fn per_veth_source_index(line: &str) -> Option<u8> {
    let mut tokens = line.split_whitespace();
    let parsed = match (
        tokens.next(),
        tokens.next(),
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
            Some("ROOMS_VETH_FWD"),
            Some("!"),
            Some("-s"),
            Some(source),
            Some("-i"),
            Some(interface),
            Some("-j"),
            Some("DROP"),
            None,
            None,
        ) => Some((interface, source)),
        _ => None,
    };
    let (interface, source) = parsed?;
    let index = clone_veth_index(interface)?;
    (source == format!("172.17.0.{}/32", 4 * index + 2)).then_some(index)
}

/// Whether the shared chain contains one canonical binding for this clone veth.
///
/// Callers pair this with [`rooms_veth_fwd_isolates`], whose grammar rejects
/// duplicate and malformed bindings globally.
#[must_use]
pub fn clone_source_binding_present(chain_dump: &str, veth: &str) -> bool {
    let Some(index) = clone_veth_index(veth) else {
        return false;
    };
    chain_dump
        .lines()
        .map(str::trim)
        .filter_map(per_veth_source_index)
        .filter(|candidate| *candidate == index)
        .count()
        == 1
}

pub(crate) fn clone_veth_index(veth: &str) -> Option<u8> {
    let suffix = veth.strip_prefix(CLONE_VETH_PREFIX)?;
    if suffix.is_empty() || !suffix.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let index = suffix.parse::<u8>().ok()?;
    if !(1..=MAX_CLONE_VETH_INDEX).contains(&index) || suffix != index.to_string() {
        return None;
    }
    Some(index)
}

pub(crate) fn clone_egress_chain(index: u8) -> String {
    format!("{CLONE_EGRESS_CHAIN_PREFIX}{index}")
}

pub(crate) fn clone_egress_jump_index(line: &str) -> Option<u8> {
    let mut tokens = line.split_whitespace();
    let (Some("-A"), Some("ROOMS_VETH_FWD"), Some("-i"), Some(veth), Some("-j"), Some(chain), None) = (
        tokens.next(),
        tokens.next(),
        tokens.next(),
        tokens.next(),
        tokens.next(),
        tokens.next(),
        tokens.next(),
    ) else {
        return None;
    };
    let index = clone_veth_index(veth)?;
    (chain == clone_egress_chain(index)).then_some(index)
}

#[must_use]
pub fn veth_input_drop_present(input_dump: &str, veth: &str) -> bool {
    if veth.is_empty() || veth.len() > 15 || veth.bytes().any(|byte| byte.is_ascii_whitespace()) {
        return false;
    }
    let expected = format!("-A INPUT -i {veth} -j DROP");
    let Some(drop) = input_dump.lines().position(|line| line.trim() == expected) else {
        return false;
    };
    input_dump
        .lines()
        .take(drop)
        .all(|line| prior_rule_preserves_drop(line, veth))
}

fn prior_rule_preserves_drop(line: &str, veth: &str) -> bool {
    if rule_is_disjoint_from_veth(line, veth) {
        return true;
    }
    matches!(rule_target(line), None | Some("DROP" | "REJECT" | "LOG"))
}

fn rule_is_disjoint_from_veth(line: &str, veth: &str) -> bool {
    if line.split_whitespace().any(|token| token == "!") {
        return false;
    }
    matches!(input_interface(line), Some(interface) if interface != veth && !interface.contains('+'))
}

fn rule_target(line: &str) -> Option<&str> {
    let mut tokens = line.split_whitespace();
    while let Some(token) = tokens.next() {
        if matches!(token, "-j" | "-g") {
            return tokens.next();
        }
    }
    None
}

fn input_interface(line: &str) -> Option<&str> {
    let mut tokens = line.split_whitespace();
    while let Some(token) = tokens.next() {
        if token == "-i" {
            return tokens.next();
        }
    }
    None
}

fn egress_interface(line: &str) -> Option<&str> {
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
            Some("ROOMS_VETH_FWD"),
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

fn return_interface(line: &str) -> Option<&str> {
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
        tokens.next(),
        tokens.next(),
        tokens.next(),
        tokens.next(),
    ) {
        (
            Some("-A"),
            Some("ROOMS_VETH_FWD"),
            Some("-d"),
            Some(VETH_SUPERNET),
            Some("-i"),
            Some(interface),
            Some("-m"),
            Some("state"),
            Some("--state"),
            Some("RELATED,ESTABLISHED"),
            Some("-j"),
            Some("ACCEPT"),
            None,
        ) if !interface.contains('+') => Some(interface),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        clone_egress_chain, clone_egress_jump_index, clone_forwarding_substrate,
        clone_source_binding_present, clone_veth_index, flat_chain_falls_through_for_veth,
        forward_jumps_ordered, rooms_veth_fwd_isolates, veth_input_drop_present,
        VETH_ANTISPOOF_DROP, VETH_INGRESS_JUMP, VETH_ISOLATION_DROP,
    };

    const GOOD_FORWARD: &str = concat!(
        "-P FORWARD ACCEPT\n",
        "-A FORWARD -j ROOMS_FWD\n",
        "-A FORWARD -i veth-h+ -j ROOMS_VETH_FWD\n",
        "-A FORWARD -o veth-h+ -j ROOMS_VETH_FWD",
    );

    const FLAT_CHAIN: &str = concat!(
        "-N ROOMS_FWD\n",
        "-A ROOMS_FWD -s 172.16.0.0/24 -d 172.16.0.0/24 -j DROP\n",
        "-A ROOMS_FWD -s 172.16.0.0/24 -d 10.0.0.0/8 -j DROP\n",
        "-A ROOMS_FWD -s 172.16.0.0/24 -o eth0 -j ACCEPT\n",
        "-A ROOMS_FWD -d 172.16.0.0/24 -i eth0 -j ACCEPT\n",
        "-A ROOMS_FWD -s 172.16.0.0/24 -j DROP",
    );

    const GOOD_CHAIN: &str = concat!(
        "-N ROOMS_VETH_FWD\n",
        "-A ROOMS_VETH_FWD ! -s 172.17.0.0/24 -i veth-h+ -j DROP\n",
        "-A ROOMS_VETH_FWD -s 172.17.0.0/24 -d 172.17.0.0/24 -j DROP\n",
        "-A ROOMS_VETH_FWD -s 172.17.0.0/24 -d 10.0.0.0/8 -j DROP\n",
        "-A ROOMS_VETH_FWD -s 172.17.0.0/24 -d 192.168.0.0/16 -j DROP\n",
        "-A ROOMS_VETH_FWD -s 172.17.0.0/24 -d 172.16.0.0/12 -j DROP\n",
        "-A ROOMS_VETH_FWD -s 172.17.0.0/24 -o eth0 -j ACCEPT\n",
        "-A ROOMS_VETH_FWD -d 172.17.0.0/24 -i eth0 -m state --state RELATED,ESTABLISHED -j ACCEPT\n",
        "-A ROOMS_VETH_FWD -s 172.17.0.0/24 -j DROP",
    );

    const GOOD_CHAIN_WITH_CLONE_EGRESS: &str = concat!(
        "-N ROOMS_VETH_FWD\n",
        "-A ROOMS_VETH_FWD ! -s 172.17.0.34/32 -i veth-h8 -j DROP\n",
        "-A ROOMS_VETH_FWD ! -s 172.17.0.14/32 -i veth-h3 -j DROP\n",
        "-A ROOMS_VETH_FWD ! -s 172.17.0.0/24 -i veth-h+ -j DROP\n",
        "-A ROOMS_VETH_FWD -s 172.17.0.0/24 -d 172.17.0.0/24 -j DROP\n",
        "-A ROOMS_VETH_FWD -s 172.17.0.0/24 -d 10.0.0.0/8 -j DROP\n",
        "-A ROOMS_VETH_FWD -s 172.17.0.0/24 -d 192.168.0.0/16 -j DROP\n",
        "-A ROOMS_VETH_FWD -s 172.17.0.0/24 -d 172.16.0.0/12 -j DROP\n",
        "-A ROOMS_VETH_FWD -i veth-h8 -j ROOMS_CEG_8\n",
        "-A ROOMS_VETH_FWD -i veth-h3 -j ROOMS_CEG_3\n",
        "-A ROOMS_VETH_FWD -s 172.17.0.0/24 -o eth0 -j ACCEPT\n",
        "-A ROOMS_VETH_FWD -d 172.17.0.0/24 -i eth0 -m state --state RELATED,ESTABLISHED -j ACCEPT\n",
        "-A ROOMS_VETH_FWD -s 172.17.0.0/24 -j DROP",
    );

    #[test]
    fn complete_chain_and_ordered_jumps_pass() {
        assert!(rooms_veth_fwd_isolates(GOOD_CHAIN));
        assert!(rooms_veth_fwd_isolates(GOOD_CHAIN_WITH_CLONE_EGRESS));
        let with_bindings = GOOD_CHAIN.replacen(
            "-A ROOMS_VETH_FWD ! -s 172.17.0.0/24",
            concat!(
                "-A ROOMS_VETH_FWD ! -s 172.17.0.14/32 -i veth-h3 -j DROP\n",
                "-A ROOMS_VETH_FWD ! -s 172.17.0.34/32 -i veth-h8 -j DROP\n",
                "-A ROOMS_VETH_FWD ! -s 172.17.0.0/24"
            ),
            1,
        );
        assert!(rooms_veth_fwd_isolates(&with_bindings));
        assert!(clone_source_binding_present(&with_bindings, "veth-h3"));
        assert!(clone_source_binding_present(&with_bindings, "veth-h8"));
        assert!(!clone_source_binding_present(&with_bindings, "veth-h7"));
        assert!(flat_chain_falls_through_for_veth(FLAT_CHAIN));
        assert!(forward_jumps_ordered(GOOD_FORWARD));
        assert!(clone_forwarding_substrate(
            GOOD_FORWARD,
            FLAT_CHAIN,
            GOOD_CHAIN
        ));
    }

    #[test]
    fn clone_veth_and_chain_identity_are_canonical_and_bounded() {
        assert_eq!(clone_veth_index("veth-h1"), Some(1));
        assert_eq!(clone_veth_index("veth-h63"), Some(63));
        assert_eq!(clone_egress_chain(3), "ROOMS_CEG_3");
        for invalid in [
            "veth-h", "veth-h0", "veth-h03", "veth-h64", "veth-h+", "veth-g3", "veth-h3 ",
        ] {
            assert_eq!(clone_veth_index(invalid), None, "accepted {invalid}");
        }
    }

    #[test]
    fn clone_jump_parser_requires_an_exact_interface_chain_pair() {
        assert_eq!(
            clone_egress_jump_index("-A ROOMS_VETH_FWD -i veth-h3 -j ROOMS_CEG_3"),
            Some(3)
        );
        for invalid in [
            "-A ROOMS_VETH_FWD -i veth-h3 -j ROOMS_CEG_4",
            "-A ROOMS_VETH_FWD -i veth-h03 -j ROOMS_CEG_3",
            "-A ROOMS_VETH_FWD -i veth-h3 -j ROOMS_CEG_03",
            "-A ROOMS_VETH_FWD -i veth-h+ -j ROOMS_CEG_3",
            "-A ROOMS_VETH_FWD -j ROOMS_CEG_3",
            "-A ROOMS_VETH_FWD -s 172.17.0.14/32 -i veth-h3 -j ROOMS_CEG_3",
            "-A ROOMS_VETH_FWD -i veth-h3 -g ROOMS_CEG_3",
        ] {
            assert_eq!(clone_egress_jump_index(invalid), None, "accepted {invalid}");
        }
    }

    #[test]
    fn malformed_broad_mismatched_or_duplicate_clone_jumps_are_caught() {
        let exact = "-A ROOMS_VETH_FWD -i veth-h3 -j ROOMS_CEG_3";
        for replacement in [
            "-A ROOMS_VETH_FWD -i veth-h3 -j ROOMS_CEG_4".to_owned(),
            "-A ROOMS_VETH_FWD -i veth-h03 -j ROOMS_CEG_3".to_owned(),
            "-A ROOMS_VETH_FWD -i veth-h+ -j ROOMS_CEG_3".to_owned(),
            "-A ROOMS_VETH_FWD -j ROOMS_CEG_3".to_owned(),
            format!("{exact}\n{exact}"),
        ] {
            let broken = GOOD_CHAIN_WITH_CLONE_EGRESS.replace(exact, &replacement);
            assert!(!rooms_veth_fwd_isolates(&broken), "accepted {replacement}");
        }
    }

    #[test]
    fn clone_jump_requires_its_exact_source_binding() {
        let broken = GOOD_CHAIN_WITH_CLONE_EGRESS.replace(
            "-A ROOMS_VETH_FWD ! -s 172.17.0.14/32 -i veth-h3 -j DROP\n",
            "",
        );
        assert!(!rooms_veth_fwd_isolates(&broken));
    }

    #[test]
    fn duplicate_clone_source_binding_is_caught() {
        let binding = "-A ROOMS_VETH_FWD ! -s 172.17.0.14/32 -i veth-h3 -j DROP";
        let broken =
            GOOD_CHAIN_WITH_CLONE_EGRESS.replace(binding, &format!("{binding}\n{binding}"));
        assert!(!rooms_veth_fwd_isolates(&broken));
    }

    #[test]
    fn clone_jumps_must_stay_after_private_drops_and_before_upstream_accept() {
        let jump = "-A ROOMS_VETH_FWD -i veth-h3 -j ROOMS_CEG_3\n";
        let before_private = GOOD_CHAIN_WITH_CLONE_EGRESS
            .replace(jump, "")
            .replace(VETH_ISOLATION_DROP, &format!("{jump}{VETH_ISOLATION_DROP}"));
        assert!(!rooms_veth_fwd_isolates(&before_private));

        let after_accept = GOOD_CHAIN_WITH_CLONE_EGRESS.replace(jump, "").replace(
            "-A ROOMS_VETH_FWD -d 172.17.0.0/24 -i eth0",
            &format!("{jump}-A ROOMS_VETH_FWD -d 172.17.0.0/24 -i eth0"),
        );
        assert!(!rooms_veth_fwd_isolates(&after_accept));
    }

    #[test]
    fn malformed_or_misbound_per_veth_source_drop_is_caught() {
        for rule in [
            "-A ROOMS_VETH_FWD ! -s 172.17.0.18/32 -i veth-h3 -j DROP",
            "-A ROOMS_VETH_FWD ! -s 172.17.0.14/32 -i veth-h+ -j DROP",
            "-A ROOMS_VETH_FWD ! -s 172.17.0.14/32 -i veth-h03 -j DROP",
            "-A ROOMS_VETH_FWD -s 172.17.0.14/32 -i veth-h3 -j DROP",
        ] {
            let broken = GOOD_CHAIN.replacen(
                "-A ROOMS_VETH_FWD ! -s 172.17.0.0/24",
                &format!("{rule}\n-A ROOMS_VETH_FWD ! -s 172.17.0.0/24"),
                1,
            );
            assert!(!rooms_veth_fwd_isolates(&broken));
        }
    }

    #[test]
    fn flat_chain_terminators_that_can_match_veth_traffic_are_caught() {
        for rule in [
            "-A ROOMS_FWD -j ACCEPT",
            "-A ROOMS_FWD -s 172.17.0.0/24 -j DROP",
            "-A ROOMS_FWD ! -s 172.16.0.0/24 -j ACCEPT",
            "-A ROOMS_FWD -j TRUSTED",
        ] {
            assert!(!flat_chain_falls_through_for_veth(&format!(
                "{FLAT_CHAIN}\n{rule}"
            )));
        }
        assert!(!flat_chain_falls_through_for_veth("-N ROOMS_FWD"));
    }

    #[test]
    fn exact_flat_dynamic_jump_and_return_path_are_disjoint_from_veth() {
        let with_dynamic_jump = FLAT_CHAIN.replacen(
            "-A ROOMS_FWD -s 172.16.0.0/24 -o eth0 -j ACCEPT",
            concat!(
                "-A ROOMS_FWD -i tap-fc3 -j ROOMS_EG_3\n",
                "-A ROOMS_FWD -s 172.16.0.0/24 -o eth0 -j ACCEPT"
            ),
            1,
        );
        assert!(flat_chain_falls_through_for_veth(&with_dynamic_jump));
        assert!(!flat_chain_falls_through_for_veth(&FLAT_CHAIN.replace(
            "-A ROOMS_FWD -d 172.16.0.0/24 -i eth0 -j ACCEPT",
            "-A ROOMS_FWD -d 172.16.0.0/24 -j ACCEPT"
        )));
    }

    #[test]
    fn complete_substrate_rejects_a_break_at_each_shared_layer() {
        let broad_flat = format!("-N ROOMS_FWD\n-A ROOMS_FWD -j ACCEPT\n{FLAT_CHAIN}");
        let missing_ingress = GOOD_FORWARD.replace(VETH_INGRESS_JUMP, "");
        let missing_isolation = GOOD_CHAIN.replace(&format!("{VETH_ISOLATION_DROP}\n"), "");
        for (forward, flat, veth) in [
            (GOOD_FORWARD, broad_flat.as_str(), GOOD_CHAIN),
            (missing_ingress.as_str(), FLAT_CHAIN, GOOD_CHAIN),
            (GOOD_FORWARD, FLAT_CHAIN, missing_isolation.as_str()),
        ] {
            assert!(!clone_forwarding_substrate(forward, flat, veth));
        }
    }

    #[test]
    fn missing_cross_clone_drop_is_caught() {
        let broken = GOOD_CHAIN.replace(
            "-A ROOMS_VETH_FWD -s 172.17.0.0/24 -d 172.17.0.0/24 -j DROP\n",
            "",
        );
        assert!(!rooms_veth_fwd_isolates(&broken));
    }

    #[test]
    fn missing_or_late_antispoof_drop_is_caught() {
        assert!(!rooms_veth_fwd_isolates(
            &GOOD_CHAIN.replace(&format!("{VETH_ANTISPOOF_DROP}\n"), "")
        ));
        let late = GOOD_CHAIN
            .replace(&format!("{VETH_ANTISPOOF_DROP}\n"), "")
            .replace(
                VETH_ISOLATION_DROP,
                &format!("{VETH_ISOLATION_DROP}\n{VETH_ANTISPOOF_DROP}"),
            );
        assert!(!rooms_veth_fwd_isolates(&late));
    }

    #[test]
    fn supernet_accept_above_drop_is_caught() {
        let broken = GOOD_CHAIN.replacen(
            "-N ROOMS_VETH_FWD\n",
            "-N ROOMS_VETH_FWD\n-A ROOMS_VETH_FWD -s 172.17.0.0/24 -j ACCEPT\n",
            1,
        );
        assert!(!rooms_veth_fwd_isolates(&broken));
    }

    #[test]
    fn broad_matchless_accept_above_drop_is_caught() {
        let broken = GOOD_CHAIN.replacen(
            "-N ROOMS_VETH_FWD\n",
            "-N ROOMS_VETH_FWD\n-A ROOMS_VETH_FWD -j ACCEPT\n",
            1,
        );
        assert!(!rooms_veth_fwd_isolates(&broken));
    }

    #[test]
    fn every_accept_class_above_the_isolation_drop_is_caught() {
        for rule in [
            "-A ROOMS_VETH_FWD -p tcp --dport 22 -j ACCEPT\n",
            "-A ROOMS_VETH_FWD -s 10.0.0.0/8 -j ACCEPT\n",
            "-A ROOMS_VETH_FWD -o eth0 -j ACCEPT\n",
        ] {
            let broken = GOOD_CHAIN.replacen(
                "-N ROOMS_VETH_FWD\n",
                &format!("-N ROOMS_VETH_FWD\n{rule}"),
                1,
            );
            assert!(!rooms_veth_fwd_isolates(&broken));
        }
    }

    #[test]
    fn accept_between_cross_clone_and_private_drops_is_caught() {
        let private = "-A ROOMS_VETH_FWD -s 172.17.0.0/24 -d 10.0.0.0/8 -j DROP";
        let broken =
            GOOD_CHAIN.replace(private, &format!("-A ROOMS_VETH_FWD -j ACCEPT\n{private}"));
        assert!(!rooms_veth_fwd_isolates(&broken));
    }

    #[test]
    fn drop_after_egress_is_caught() {
        let drop = "-A ROOMS_VETH_FWD -s 172.17.0.0/24 -d 172.17.0.0/24 -j DROP\n";
        let broken = GOOD_CHAIN.replace(drop, "").replacen(
            "-A ROOMS_VETH_FWD -d 172.17.0.0/24 -i eth0",
            concat!(
                "-A ROOMS_VETH_FWD -s 172.17.0.0/24 -d 172.17.0.0/24 -j DROP\n",
                "-A ROOMS_VETH_FWD -d 172.17.0.0/24 -i eth0"
            ),
            1,
        );
        assert!(!rooms_veth_fwd_isolates(&broken));
    }

    #[test]
    fn missing_egress_return_or_private_drop_is_caught() {
        for missing in [
            "-A ROOMS_VETH_FWD -s 172.17.0.0/24 -o eth0 -j ACCEPT\n",
            "-A ROOMS_VETH_FWD -d 172.17.0.0/24 -i eth0 -m state --state RELATED,ESTABLISHED -j ACCEPT\n",
            "-A ROOMS_VETH_FWD -s 172.17.0.0/24 -d 10.0.0.0/8 -j DROP\n",
        ] {
            assert!(!rooms_veth_fwd_isolates(&GOOD_CHAIN.replace(missing, "")));
        }
    }

    #[test]
    fn narrowed_or_inverted_private_drop_is_caught() {
        let private = "-A ROOMS_VETH_FWD -s 172.17.0.0/24 -d 10.0.0.0/8 -j DROP";
        for replacement in [
            "-A ROOMS_VETH_FWD -s 172.17.0.0/24 ! -d 10.0.0.0/8 -j DROP",
            "-A ROOMS_VETH_FWD -s 172.17.0.0/24 -d 10.0.0.0/8 -p tcp -j DROP",
        ] {
            assert!(!rooms_veth_fwd_isolates(
                &GOOD_CHAIN.replace(private, replacement)
            ));
        }
    }

    #[test]
    fn private_drops_below_egress_and_missing_or_early_tail_are_caught() {
        let private = "-A ROOMS_VETH_FWD -s 172.17.0.0/24 -d 10.0.0.0/8 -j DROP\n";
        let below = GOOD_CHAIN.replace(private, "").replace(
            "-A ROOMS_VETH_FWD -s 172.17.0.0/24 -j DROP",
            &format!("{private}-A ROOMS_VETH_FWD -s 172.17.0.0/24 -j DROP"),
        );
        assert!(!rooms_veth_fwd_isolates(&below));
        assert!(!rooms_veth_fwd_isolates(
            &GOOD_CHAIN.replace("-A ROOMS_VETH_FWD -s 172.17.0.0/24 -j DROP", "",)
        ));
        let tail = "-A ROOMS_VETH_FWD -s 172.17.0.0/24 -j DROP";
        let early = GOOD_CHAIN.replace(tail, "").replacen(
            VETH_ISOLATION_DROP,
            &format!("{VETH_ISOLATION_DROP}\n{tail}"),
            1,
        );
        assert!(!rooms_veth_fwd_isolates(&early));
    }

    #[test]
    fn wrong_or_missing_jump_order_is_caught() {
        assert!(!forward_jumps_ordered(
            "-A FORWARD -j ROOMS_VETH_FWD\n-A FORWARD -j ROOMS_FWD"
        ));
        assert!(!forward_jumps_ordered("-A FORWARD -j ROOMS_FWD"));
        assert!(!forward_jumps_ordered(concat!(
            "-A FORWARD -j ROOMS_FWD\n",
            "-A FORWARD -j ACCEPT\n",
            "-A FORWARD -i veth-h+ -j ROOMS_VETH_FWD\n",
            "-A FORWARD -o veth-h+ -j ROOMS_VETH_FWD",
        )));
        assert!(!forward_jumps_ordered(concat!(
            "-A FORWARD -j ROOMS_FWD\n",
            "-A FORWARD -o veth-h+ -j ROOMS_VETH_FWD\n",
            "-A FORWARD -i veth-h+ -j ROOMS_VETH_FWD",
        )));
    }

    #[test]
    fn broad_or_duplicate_veth_jump_is_caught() {
        for rules in [
            concat!(
                "-A FORWARD -j ROOMS_FWD\n",
                "-A FORWARD -j ROOMS_VETH_FWD\n",
                "-A FORWARD -o veth-h+ -j ROOMS_VETH_FWD",
            ),
            concat!(
                "-A FORWARD -j ROOMS_FWD\n",
                "-A FORWARD -i docker0 -j ROOMS_VETH_FWD\n",
                "-A FORWARD -o veth-h+ -j ROOMS_VETH_FWD",
            ),
            concat!(
                "-A FORWARD -j ROOMS_FWD\n",
                "-A FORWARD -i veth-h+ -j ROOMS_VETH_FWD\n",
                "-A FORWARD -o veth-h+ -j ROOMS_VETH_FWD\n",
                "-A FORWARD -j ROOMS_VETH_FWD",
            ),
        ] {
            assert!(!forward_jumps_ordered(rules));
        }
    }

    #[test]
    fn exact_input_drop_before_accepts_passes() {
        let dump = concat!(
            "-P INPUT ACCEPT\n",
            "-A INPUT -i veth-h3 -j DROP\n",
            "-A INPUT -j ACCEPT",
        );
        assert!(veth_input_drop_present(dump, "veth-h3"));
    }

    #[test]
    fn missing_wrong_or_unsafe_input_drop_is_caught() {
        assert!(!veth_input_drop_present("-P INPUT ACCEPT", "veth-h3"));
        assert!(!veth_input_drop_present(
            "-A INPUT -i veth-h2 -j DROP",
            "veth-h3"
        ));
        assert!(!veth_input_drop_present(
            "-A INPUT -i veth-h3 -j ACCEPT\n-A INPUT -i veth-h3 -j DROP",
            "veth-h3"
        ));
        assert!(!veth_input_drop_present(
            "-A INPUT -j ACCEPT\n-A INPUT -i veth-h3 -j DROP",
            "veth-h3"
        ));
        assert!(!veth_input_drop_present(
            "-A INPUT -s 172.16.0.0/24 -j ACCEPT\n-A INPUT -i veth-h3 -j DROP",
            "veth-h3"
        ));
        assert!(!veth_input_drop_present(
            "-A INPUT -i veth-h+ -j ACCEPT\n-A INPUT -i veth-h3 -j DROP",
            "veth-h3"
        ));
        assert!(!veth_input_drop_present(
            "-A INPUT ! -i lo -j ACCEPT\n-A INPUT -i veth-h3 -j DROP",
            "veth-h3"
        ));
        assert!(!veth_input_drop_present(
            "-A INPUT -i veth-h3 -j RETURN\n-A INPUT -i veth-h3 -j DROP",
            "veth-h3"
        ));
        assert!(!veth_input_drop_present(
            "-A INPUT -i veth-h3 -j TRUSTED\n-A INPUT -i veth-h3 -j DROP",
            "veth-h3"
        ));
        assert!(!veth_input_drop_present(
            "-A INPUT -i veth-h3 -g TRUSTED\n-A INPUT -i veth-h3 -j DROP",
            "veth-h3"
        ));
    }

    #[test]
    fn unrelated_input_accept_does_not_preempt_the_veth_drop() {
        let dump = concat!(
            "-A INPUT -i lo -j ACCEPT\n",
            "-A INPUT -i veth-h30 -j ACCEPT\n",
            "-A INPUT -i veth-h3 -j DROP",
        );
        assert!(veth_input_drop_present(dump, "veth-h3"));
    }

    #[test]
    fn nonterminating_log_before_the_veth_drop_is_safe() {
        let dump = concat!(
            "-P INPUT ACCEPT\n",
            "-A INPUT -i veth-h3 -j LOG\n",
            "-A INPUT -i veth-h3 -j DROP",
        );
        assert!(veth_input_drop_present(dump, "veth-h3"));
    }
}
