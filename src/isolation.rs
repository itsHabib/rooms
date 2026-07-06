//! Pure analysis of the host firewall's guest-isolation invariant over
//! `iptables -S` dumps.
//!
//! The pool's cross-talk guarantee — one room cannot reach another — rests on the
//! `ROOMS_FWD` chain being wired correctly: jumped from `FORWARD` **first**, with
//! the guest→guest DROP present, unpreempted by any supernet ACCEPT, and ahead of
//! the egress ACCEPT. These predicates encode that invariant as pure string
//! analysis, so the *negative* assertions — the load-bearing "inter-slot traffic
//! is blocked" claims — are unit-tested in CI against deliberately-broken chains,
//! not merely observed live on the rooms-host. A test that cannot fail is
//! worthless; the tests below prove each way isolation can break is caught.

/// The allocator supernet literal, single-sourced so the predicate constants and
/// the test fixtures all derive from one place. A macro rather than a `const`
/// because `concat!` only accepts literals, not const references.
macro_rules! supernet {
    () => {
        "172.16.0.0/24"
    };
}

/// The allocator supernet every slot's /30 is carved from.
pub const SUPERNET: &str = supernet!();

/// The guest→guest DROP `setup-tap.sh --host` installs into `ROOMS_FWD`.
pub const ISOLATION_DROP: &str = concat!(
    "-A ROOMS_FWD -s ",
    supernet!(),
    " -d ",
    supernet!(),
    " -j DROP"
);

/// The jump into `ROOMS_FWD` from the built-in `FORWARD` chain.
pub const FORWARD_JUMP: &str = "-A FORWARD -j ROOMS_FWD";

/// Supernet-source / supernet-dest match fragments an ACCEPT would carry.
const MATCH_SRC: &str = concat!("-s ", supernet!());
const MATCH_DST: &str = concat!("-d ", supernet!());

/// True when the `ROOMS_FWD` jump is the **first** rule in the `FORWARD` chain,
/// so no pre-existing broad ACCEPT preempts guest isolation.
///
/// `forward_dump` is an `iptables -S FORWARD` dump; the policy line
/// (`-P FORWARD ...`) is not a rule and is skipped.
#[must_use]
pub fn forward_jump_is_first(forward_dump: &str) -> bool {
    forward_dump
        .lines()
        .find(|line| line.starts_with("-A FORWARD "))
        .map(str::trim)
        == Some(FORWARD_JUMP)
}

/// Line index of the guest→guest isolation DROP in a `ROOMS_FWD` dump. Per-line
/// exact match, shared by the presence and both ordering checks, so a decorated
/// line can never make presence (loose) and ordering (strict) disagree.
fn isolation_drop_line(rooms_fwd_dump: &str) -> Option<usize> {
    rooms_fwd_dump
        .lines()
        .position(|line| line.trim() == ISOLATION_DROP)
}

/// True when the guest→guest DROP is present in the `ROOMS_FWD` dump.
#[must_use]
pub fn has_isolation_drop(rooms_fwd_dump: &str) -> bool {
    isolation_drop_line(rooms_fwd_dump).is_some()
}

/// True when no ACCEPT that could match inter-slot traffic precedes the DROP.
///
/// A dangerous ACCEPT above the DROP is one that would match an inter-slot packet
/// (both endpoints in the supernet): either it **names the supernet** on a side,
/// or it's **broad** — no address (`-s`/`-d`) or interface (`-i`/`-o`) constraint
/// that would exclude the packet, so a bare `-j ACCEPT` matches everything. An
/// ACCEPT scoped to a non-supernet address is harmless. The legitimate egress and
/// return-path ACCEPTs name the supernet but sit after the DROP, so they pass;
/// only a dangerous ACCEPT *above* the DROP fails.
#[must_use]
pub fn no_accept_before_drop(rooms_fwd_dump: &str) -> bool {
    let accept = rooms_fwd_dump.lines().position(|line| {
        if !line.contains("-j ACCEPT") {
            return false;
        }
        let names_supernet = line.contains(MATCH_SRC) || line.contains(MATCH_DST);
        let broad = !line.contains("-s ")
            && !line.contains("-d ")
            && !line.contains("-i ")
            && !line.contains("-o ");
        names_supernet || broad
    });
    match (isolation_drop_line(rooms_fwd_dump), accept) {
        (Some(drop), Some(accept)) => accept > drop,
        (Some(_), None) => true,
        _ => false,
    }
}

/// True when the DROP sits before the egress ACCEPT — the order that makes it
/// bite (a preceding broad ACCEPT would let inter-slot traffic through first).
///
/// A dump with the DROP and no egress ACCEPT still passes: nothing to slip past.
#[must_use]
pub fn drop_precedes_egress(rooms_fwd_dump: &str) -> bool {
    let egress = rooms_fwd_dump.lines().position(|line| {
        line.contains(MATCH_SRC) && line.contains("-o ") && line.contains("-j ACCEPT")
    });
    match (isolation_drop_line(rooms_fwd_dump), egress) {
        (Some(drop), Some(egress)) => drop < egress,
        (Some(_), None) => true,
        _ => false,
    }
}

/// True when the `ROOMS_FWD` chain fully isolates guest↔guest traffic: the DROP
/// is present, unpreempted by a supernet ACCEPT, and ahead of the egress ACCEPT.
///
/// Pair with [`forward_jump_is_first`] on the `FORWARD` dump for the whole path:
/// a chain that isolates but isn't reached (jump missing or not first) still
/// leaks, and a chain that's reached first but doesn't isolate also leaks.
#[must_use]
pub fn rooms_fwd_isolates(rooms_fwd_dump: &str) -> bool {
    has_isolation_drop(rooms_fwd_dump)
        && no_accept_before_drop(rooms_fwd_dump)
        && drop_precedes_egress(rooms_fwd_dump)
}

#[cfg(test)]
mod tests {
    use super::{
        drop_precedes_egress, forward_jump_is_first, has_isolation_drop, no_accept_before_drop,
        rooms_fwd_isolates,
    };

    /// A correctly-ordered `ROOMS_FWD` dump — what `setup-tap.sh --host` installs
    /// (verified byte-for-byte against the real chain on the rooms-host).
    const GOOD_ROOMS_FWD: &str = concat!(
        "-N ROOMS_FWD\n",
        "-A ROOMS_FWD -s 172.16.0.0/24 -d 172.16.0.0/24 -j DROP\n",
        "-A ROOMS_FWD -s 172.16.0.0/24 -d 10.0.0.0/8 -j DROP\n",
        "-A ROOMS_FWD -s 172.16.0.0/24 -o eth0 -j ACCEPT\n",
        "-A ROOMS_FWD -d 172.16.0.0/24 -i eth0 -m state --state RELATED,ESTABLISHED -j ACCEPT\n",
        "-A ROOMS_FWD -s 172.16.0.0/24 -m comment --comment \"rooms:fwd:v1:172.16.0.0/24\" -j DROP",
    );

    const GOOD_FORWARD: &str = "-P FORWARD ACCEPT\n-A FORWARD -j ROOMS_FWD";

    #[test]
    fn a_correctly_wired_chain_isolates() {
        assert!(forward_jump_is_first(GOOD_FORWARD));
        assert!(has_isolation_drop(GOOD_ROOMS_FWD));
        assert!(no_accept_before_drop(GOOD_ROOMS_FWD));
        assert!(drop_precedes_egress(GOOD_ROOMS_FWD));
        assert!(rooms_fwd_isolates(GOOD_ROOMS_FWD));
    }

    // --- the load-bearing negative assertions: every way isolation breaks is caught ---

    #[test]
    fn a_missing_drop_is_caught() {
        let broken = concat!(
            "-N ROOMS_FWD\n",
            "-A ROOMS_FWD -s 172.16.0.0/24 -o eth0 -j ACCEPT",
        );
        assert!(!has_isolation_drop(broken));
        assert!(
            !rooms_fwd_isolates(broken),
            "no DROP at all must not isolate"
        );
    }

    #[test]
    fn a_dest_accept_above_the_drop_is_caught() {
        // A supernet-dest ACCEPT placed above the DROP matches an inter-slot
        // packet first — cross-talk slips through while the DROP reads present.
        let broken = concat!(
            "-N ROOMS_FWD\n",
            "-A ROOMS_FWD -s 172.16.0.0/24 -d 172.16.0.0/24 -j ACCEPT\n",
            "-A ROOMS_FWD -s 172.16.0.0/24 -d 172.16.0.0/24 -j DROP\n",
            "-A ROOMS_FWD -s 172.16.0.0/24 -o eth0 -j ACCEPT",
        );
        assert!(has_isolation_drop(broken), "the DROP is present...");
        assert!(
            !no_accept_before_drop(broken),
            "...but an ACCEPT precedes it"
        );
        assert!(!rooms_fwd_isolates(broken));
    }

    #[test]
    fn a_source_only_accept_above_the_drop_is_caught() {
        // The subtle one: a source-only supernet ACCEPT (no dest match) still
        // matches inter-slot traffic (both endpoints are in the supernet).
        let broken = concat!(
            "-N ROOMS_FWD\n",
            "-A ROOMS_FWD -s 172.16.0.0/24 -j ACCEPT\n",
            "-A ROOMS_FWD -s 172.16.0.0/24 -d 172.16.0.0/24 -j DROP\n",
            "-A ROOMS_FWD -s 172.16.0.0/24 -o eth0 -j ACCEPT",
        );
        assert!(!no_accept_before_drop(broken));
        assert!(!rooms_fwd_isolates(broken));
    }

    #[test]
    fn a_broad_matchless_accept_above_the_drop_is_caught() {
        // A match-less ACCEPT accepts everything — inter-slot traffic included —
        // even though it names no supernet address.
        let broken = concat!(
            "-N ROOMS_FWD\n",
            "-A ROOMS_FWD -j ACCEPT\n",
            "-A ROOMS_FWD -s 172.16.0.0/24 -d 172.16.0.0/24 -j DROP\n",
            "-A ROOMS_FWD -s 172.16.0.0/24 -o eth0 -j ACCEPT",
        );
        assert!(!no_accept_before_drop(broken));
        assert!(!rooms_fwd_isolates(broken));
    }

    #[test]
    fn a_drop_after_the_egress_accept_is_caught() {
        let broken = concat!(
            "-N ROOMS_FWD\n",
            "-A ROOMS_FWD -s 172.16.0.0/24 -o eth0 -j ACCEPT\n",
            "-A ROOMS_FWD -s 172.16.0.0/24 -d 172.16.0.0/24 -j DROP",
        );
        assert!(!drop_precedes_egress(broken));
        assert!(!rooms_fwd_isolates(broken));
    }

    #[test]
    fn a_forward_jump_that_isnt_first_is_caught() {
        // A broad ACCEPT ahead of the jump preempts the whole chain.
        let broken = "-P FORWARD ACCEPT\n-A FORWARD -j ACCEPT\n-A FORWARD -j ROOMS_FWD";
        assert!(!forward_jump_is_first(broken));
    }

    #[test]
    fn a_missing_forward_jump_is_caught() {
        let broken = "-P FORWARD ACCEPT\n-A FORWARD -o docker0 -j ACCEPT";
        assert!(!forward_jump_is_first(broken));
    }
}
