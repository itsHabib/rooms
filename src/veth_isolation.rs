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
        if line.split_whitespace().any(|token| token == "!") {
            return false;
        }
        if !has_token_pair(line, "-s", FLAT_SUPERNET) && !has_token_pair(line, "-d", FLAT_SUPERNET)
        {
            return false;
        }
    }
    seen
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
    while rules.peek().is_some_and(|line| per_veth_source_drop(line)) {
        rules.next();
    }
    let fixed_prefix = rules.next() == Some(VETH_ANTISPOOF_DROP)
        && rules.next() == Some(VETH_ISOLATION_DROP)
        && rules.next() == Some(PRIVATE_DROPS[0])
        && rules.next() == Some(PRIVATE_DROPS[1])
        && rules.next() == Some(PRIVATE_DROPS[2]);
    let egress = rules.next().and_then(egress_interface);
    let returned = rules.next().and_then(return_interface);
    fixed_prefix
        && matches!((egress, returned), (Some(out), Some(input)) if out == input)
        && rules.next() == Some(VETH_TAIL_DROP)
        && rules.next().is_none()
}

fn per_veth_source_drop(line: &str) -> bool {
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
    let Some((interface, source)) = parsed else {
        return false;
    };
    let Some(index) = interface
        .strip_prefix("veth-h")
        .and_then(|value| value.parse::<u8>().ok())
        .filter(|index| (1..=63).contains(index))
    else {
        return false;
    };
    source == format!("172.17.0.{}/32", 4 * index + 2)
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
        flat_chain_falls_through_for_veth, forward_jumps_ordered, rooms_veth_fwd_isolates,
        veth_input_drop_present, VETH_ANTISPOOF_DROP, VETH_ISOLATION_DROP,
    };

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

    #[test]
    fn complete_chain_and_ordered_jumps_pass() {
        assert!(rooms_veth_fwd_isolates(GOOD_CHAIN));
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
        assert!(flat_chain_falls_through_for_veth(FLAT_CHAIN));
        assert!(forward_jumps_ordered(concat!(
            "-P FORWARD ACCEPT\n",
            "-A FORWARD -j ROOMS_FWD\n",
            "-A FORWARD -i veth-h+ -j ROOMS_VETH_FWD\n",
            "-A FORWARD -o veth-h+ -j ROOMS_VETH_FWD",
        )));
    }

    #[test]
    fn malformed_or_misbound_per_veth_source_drop_is_caught() {
        for rule in [
            "-A ROOMS_VETH_FWD ! -s 172.17.0.18/32 -i veth-h3 -j DROP",
            "-A ROOMS_VETH_FWD ! -s 172.17.0.14/32 -i veth-h+ -j DROP",
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
