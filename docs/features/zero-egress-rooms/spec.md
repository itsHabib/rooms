**Status**: draft
**Owner**: @mh
**Date**: 2026-07-23
**Related**: dossier task `zero-egress-rooms` (id: `tsk_01KY6SM30MGXV2DTHF8A4RE1AG`), phase `03-custody-plane`. Builds on the host witness ([`docs/features/host-witness/spec.md`](../host-witness/spec.md), #77). Prerequisite for `egress-control-test-harness` (sibling task).

# Zero-egress rooms: `--egress none|allowlist` enforcement — design spec

## Scope

| Bucket | Files | Est. LOC | Weighted |
| --- | --- | --- | --- |
| Production source | `src/egress.rs` (new: policy type, parse, rule synthesis, install/remove), `src/main.rs` (clap arg + admission + threading), `src/firecracker.rs` (install after tap-up, remove at teardown), `src/artifacts.rs` (Witness gains egress-policy record), `src/lib.rs` (module decl) | ~320 | 320 |
| Tests | `src/egress.rs` unit tests (parse + pure rule-synthesis, isolation.rs style), `src/artifacts.rs` (policy-record summary), `tests/egress_e2e.rs` (host-only) | ~280 | 140 |
| **Total** | | | **~460** |

Band: **ideal** per the repo PR-sizing convention. **Split risk**: if the blocked-attempt *receipt* record (§ Behavior 5) balloons, split it into a follow-up and land enforcement + proof-of-absence first — the enforcement wall is the load-bearing half.

## Goal

Add a per-room egress policy, enforced on the **host** side of the room's network slot, that turns the witness (#77) from an *observer* of what left into an *enforcer* of what cannot. A new `--egress` flag admits three modes: `none` (no outbound traffic leaves the room's slot), `allowlist:<host-or-cidr>[,...]` (only the listed destinations are reachable, everything else dropped), and absent (today's observe-only behavior — non-breaking). The headline artifact is a **proof of absence**: a receipt showing an agent worked in a room and provably zero bytes left the host. Paired with a host-loopback model endpoint, `--egress none` is a capability no sandbox-as-a-service offers.

## Where it hooks (confirmed against the witness + pool impl)

The pool already gives each room an isolated network slot: slot *k* owns `tap-fc<k>` on the `172.16.0.4k/30`, guest IP `172.16.0.(4k+2)` (`src/slot.rs::derive`). The once-per-host substrate (`scripts/setup-tap.sh --host`, modelled in `src/isolation.rs`) installs the `ROOMS_FWD` filter chain, jumped from `FORWARD` position 1, whose relevant tail is:

```
-A ROOMS_FWD -s 172.16.0.0/24 -d 172.16.0.0/24 -j DROP        # guest↔guest isolation
-A ROOMS_FWD -s 172.16.0.0/24 -d 10.0.0.0/8    -j DROP        # RFC1918 blocks
... (192.168/16, 172.16/12)
-A ROOMS_FWD -s 172.16.0.0/24 -o <out_iface>   -j ACCEPT      # ← the supernet egress ACCEPT
-A ROOMS_FWD -i <out_iface> -d 172.16.0.0/24 -m state --state RELATED,ESTABLISHED -j ACCEPT
-A ROOMS_FWD -s 172.16.0.0/24 -m comment --comment "rooms:fwd:v1:..." -j DROP   # default-deny tail
```

Per-room policy is a small set of rules **keyed on this room's guest IP**, inserted **ahead of the supernet egress ACCEPT** so they take precedence for this one room while every other slot still hits the shared ACCEPT unchanged. The guest source IP — not the tap name — is the key: `isolation.rs` already establishes that source-IP matching is the load-bearing predicate, and a compromised guest cannot spoof its way past a rule that also constrains the outbound interface (the return path is state-scoped).

- **Install seam**: `firecracker::create_slot_tap` (`src/firecracker.rs:582`) — right after the tap is up and forwarding is enabled, *before* the VMM can transmit. Same fail-closed-before-guest-transmits posture the witness start already holds: a room asked for `--egress none` that cannot install its DROP must not boot.
- **Remove seam**: `firecracker::release_tap` / `delete_tap` (`src/firecracker.rs:1521`/`1540`) — the per-room rules are torn down beside the tap, on both the normal teardown path and the `gc` orphan sweep. Removal is idempotent (compare-and-delete style: only remove a rule that names *this* guest IP), so a double-teardown or a gc race never deletes a live sibling's rule.

## Behavior / fix

1. **Flag surface.** New `--egress <policy>` on the `run`/`exec` path (`src/main.rs` `RunArgs`), parsed into an `egress::Policy` enum:
   - `none` → `Policy::None`
   - `allowlist:api.anthropic.com,10.1.2.0/24` → `Policy::Allowlist(Vec<Dest>)`
   - flag absent → `Policy::Observe` (today's behavior; **non-breaking default**).
   Parse errors (empty allowlist, unresolvable host at launch, malformed CIDR) fail fast, before any slot is claimed — mirroring the `--secret` / `--witness` pre-claim admission in `run_room_inner`.

2. **`none` enforcement.** For the room's guest IP, insert a DROP for all forwarded egress out the outbound interface, above the supernet ACCEPT:
   ```
   -I ROOMS_FWD <pos> -s <guest_ip> -o <out_iface> -j LOG --log-prefix "rooms-egress-drop:<k> "
   -I ROOMS_FWD <pos> -s <guest_ip> -o <out_iface> -j DROP
   ```
   Loopback inside the guest and vsock-to-host (`--secret`, #79) never transit `FORWARD`, so they are unaffected — `none` blocks *external* egress, not host-loopback. A host-loopback model endpoint remains reachable under `none` (the task's stated allowance; wiring the endpoint itself is a separate follow-up).

3. **`allowlist:X` enforcement.** Resolve each host to IPs **once at launch** and pin them (avoid DNS rebinding); insert an ACCEPT per pinned dest for the guest IP, then a catch-all DROP for the guest IP, all above the supernet ACCEPT:
   ```
   -I ROOMS_FWD <pos> -s <guest_ip> -d <pinned_ip> -o <out_iface> -j ACCEPT   # per dest
   -I ROOMS_FWD <pos> -s <guest_ip> -o <out_iface> -j LOG --log-prefix ...
   -I ROOMS_FWD <pos> -s <guest_ip> -o <out_iface> -j DROP
   ```
   **DNS honesty**: resolution is pinned at launch, so a dynamic-IP endpoint that rotates addresses mid-run will start dropping — documented as a v1 limitation, with CIDR form (`-d 1.2.3.0/24`) as the escape hatch for such endpoints. Do not paper over this; the allowlist is honest about what it pins. A hostname the guest resolves must still reach the *same* pinned IP the host allowed — DNS itself (to the configured resolver) is only reachable if the resolver's IP is in the allowlist, so `allowlist` with hostnames implies allowing the resolver too; call this out in `--help` and the operator doc.

4. **Rule-ordering invariant (the load-bearing part).** The per-room rules MUST sit above the supernet egress ACCEPT, or `none`/`allowlist` silently leak (the broad ACCEPT matches first). This is exactly the failure class `isolation.rs` exists to catch, so the synthesis + ordering logic lives in `egress.rs` as **pure functions over rule dumps**, unit-tested against deliberately-broken orderings — the negative assertions ("an egress rule below the supernet ACCEPT does not enforce") must be able to fail. Mirror `isolation.rs`'s structure: a `room_egress_enforced(guest_ip, dump) -> bool` predicate and its refutations.

5. **Host-recorded blocked attempts → receipt.** A dropped attempt must be host-recorded and land in the room's artifact, tamper-evident:
   - The witness `tcpdump` already sits on `tap-fc<k>` and captures the guest's *attempted* SYN even when `FORWARD` drops the packet downstream — so **attempted ∧ ¬permitted = blocked**, derivable from the existing pcap plus the policy with no new capture path.
   - Cross-check with the iptables DROP rule's **packet counter** (`iptables -nvL ROOMS_FWD`) read at teardown: a host-side count of packets that hit *this room's* drop rule, which the guest cannot alter.
   - `artifacts::Witness` (witness.json, `src/artifacts.rs:274`) gains an egress-policy record: the policy applied (`observe|none|allowlist`), the `permitted` destination set, and the `blocked` attempts (dest + packet count, from the pcap-∖-policy set intersected with the DROP counter). For a `--egress none` run with no attempts, `permitted` is empty and `blocked` is empty — **the proof-of-absence artifact**: policy `none`, permitted `[]`, and a `capture_complete: true` witness showing zero egress destinations.

6. **Doctor / degraded mode.** `run_room_inner` already fails fast when `ROOMS_FWD` isn't installed (`doctor::ensure_rooms_fwd_installed`, `src/main.rs:628`). `--egress` depends on that same chain; no new host-substrate requirement, but note in the error path that an `--egress` request against a host missing the chain fails with the existing remediation.

## Acceptance

- A room launched `--egress none` cannot reach any external host; a room with `--egress allowlist:X` reaches X and nothing else — verified **from inside the guest** by attempting both (a permitted dest succeeds, a denied dest times out / is refused).
- Every blocked attempt appears host-side in `witness.json` with dest + count; the record is derived outside the guest trust boundary (pcap + iptables counter), never from guest self-report.
- Absent flag ⇒ unchanged behavior; existing witness and pool tests stay green.
- A `--egress none` run's receipt shows policy `none`, an empty permitted-egress set, and `capture_complete: true` with zero destinations — the proof-of-absence artifact.
- `make check` green. E2e (`tests/egress_e2e.rs`, host-only, `#[cfg(feature = "e2e")]`) covers the `none`, `allowlist`, and blocked-attempt paths on the rooms-host.

## Test plan

Pure/unit (run in CI, any platform):
- `egress::parse` — `none`, `allowlist:host`, `allowlist:host,cidr`, empty allowlist errors, malformed CIDR errors, unknown mode errors.
- `egress` rule synthesis — `none_synthesizes_a_guest_scoped_drop`, `allowlist_synthesizes_accept_per_dest_then_drop`, rules name the guest IP and the outbound interface.
- `egress` ordering predicate (isolation.rs style) — `room_egress_enforced_when_rules_precede_supernet_accept`, and refutations: `a_drop_below_the_supernet_accept_does_not_enforce`, `a_missing_catch_all_drop_leaks`, `an_accept_for_the_wrong_guest_ip_does_not_apply`.
- `artifacts` — `witness_records_none_policy_as_proof_of_absence`, `blocked_set_is_pcap_minus_permitted`.

E2e (rooms-host only, gated behind `e2e`):
- `egress_none_blocks_all_external`, `egress_allowlist_permits_only_listed`, `blocked_attempt_lands_in_receipt`, `absent_flag_is_observe_only`.

## Non-goals

- **Not** TLS interception or payload inspection — connection-level allow/deny by destination only.
- **Not** per-process policy inside the guest — the boundary is the room's network slot.
- **Not** local-model wiring — this task only *permits* a host-loopback endpoint under `none`; wiring the endpoint is a separate follow-up.
- **Not** dynamic mid-run allowlist changes or DNS-rebinding-resistant re-resolution — v1 pins at launch and documents the limitation; CIDR form is the escape hatch.
