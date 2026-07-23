**Status**: draft
**Owner**: @mh
**Date**: 2026-07-23
**Related**: dossier task `zero-egress-rooms` (id: `tsk_01KY6SM30MGXV2DTHF8A4RE1AG`), phase `03-custody-plane`. Builds on the host witness ([`docs/features/host-witness/spec.md`](../host-witness/spec.md), #77). Prerequisite for `egress-control-test-harness` (sibling task). Revised after PR #81 review (Codex P1 anti-spoof, Codex P2 / Claude ordering).

# Zero-egress rooms: `--egress none|allowlist` enforcement — design spec

## Scope

| Bucket | Files | Est. LOC | Weighted |
| --- | --- | --- | --- |
| Production source | `src/egress.rs` (new: policy type, parse, per-room chain synthesis, install/remove), `src/main.rs` (clap arg + admission + threading), `src/firecracker.rs` (install after tap-up, remove at teardown), `src/artifacts.rs` (Witness gains egress-policy record; `WITNESS_SCHEMA_VERSION` bump), `src/lib.rs` (module decl) | ~340 | 340 |
| Tests | `src/egress.rs` unit tests (parse + pure chain-synthesis + enforcement predicate, isolation.rs style), `src/artifacts.rs` (policy-record summary), `tests/egress_e2e.rs` (host-only) | ~290 | 145 |
| **Total** | | | **~485** |

Band: **ideal** per the repo PR-sizing convention. **Split risk**: if the blocked-attempt *receipt* record (§ Behavior 5) balloons, split it into a follow-up and land enforcement + proof-of-absence first — the enforcement wall is the load-bearing half.

## Goal

Add a per-room egress policy, enforced on the **host** side of the room's network slot, that turns the witness (#77) from an *observer* of what left into an *enforcer* of what cannot. A new `--egress` flag admits three modes: `none` (no outbound traffic leaves the room's slot), `allowlist:<host-or-cidr>[,...]` (only the listed destinations are reachable, everything else dropped), and absent (today's observe-only behavior — non-breaking). The headline artifact is a **proof of absence**: a receipt showing an agent worked in a room and provably zero bytes left the host. Paired with a host-loopback model endpoint, `--egress none` is a capability no sandbox-as-a-service offers.

## Threat model (why the tap is the key, not the source IP)

The guest runs untrusted, root-capable code and **can forge its IPv4 source address** — the witness design already assumes exactly this (`docs/features/host-witness/spec.md:75-77`, `src/artifacts.rs:461-466`, "a compromised guest can neither forge nor suppress" packets *on the tap*). So a per-room rule keyed only on `-s <guest_ip>` is a **spoofing bypass**: a compromised guest sends a packet with some *other* `172.16.0.x` source, misses its own per-room DROP, and falls through to the shared supernet egress ACCEPT — `--egress none` leaks.

The one thing the guest cannot forge is *which interface its packets arrive on*. Every packet this room emits physically transits `tap-fc<k>` (the same unforgeable surface the witness captures on). Therefore per-room egress policy is keyed on the **ingress interface `-i tap-fc<k>`**, not the source IP. The tap name *is* load-bearing here.

## Where it hooks (confirmed against the witness + pool impl)

The pool already gives each room an isolated network slot: slot *k* owns `tap-fc<k>` on the `172.16.0.4k/30` (`src/slot.rs::derive`). The once-per-host substrate (`scripts/setup-tap.sh --host`, modelled in `src/isolation.rs`) installs the `ROOMS_FWD` filter chain, jumped from `FORWARD` position 1, whose relevant tail is:

```
-A ROOMS_FWD -s 172.16.0.0/24 -d 172.16.0.0/24 -j DROP        # guest↔guest isolation
-A ROOMS_FWD -s 172.16.0.0/24 -d 10.0.0.0/8    -j DROP        # RFC1918 blocks
... (192.168/16, 172.16/12)
-A ROOMS_FWD -s 172.16.0.0/24 -o <out_iface>   -j ACCEPT      # ← the supernet egress ACCEPT
-A ROOMS_FWD -i <out_iface> -d 172.16.0.0/24 -m state --state RELATED,ESTABLISHED -j ACCEPT
-A ROOMS_FWD -s 172.16.0.0/24 -m comment --comment "rooms:fwd:v1:..." -j DROP   # default-deny tail
```

**Design: a dedicated per-room chain, jumped by tap.** Rather than splice ordered rules into the shared chain (fragile — `iptables -I` at a fixed position reverses insertion order, and a mis-computed position lands per-room drops above the guest↔guest isolation DROP), the room gets its own chain `ROOMS_EG_<k>`:

1. Create `ROOMS_EG_<k>` and **append** its rules in natural top-to-bottom order (append never reverses, so the ordering-bug class is structurally impossible).
2. Insert exactly **one** position-sensitive rule into `ROOMS_FWD` — the jump, keyed on the unforgeable tap — immediately **above the supernet egress ACCEPT** and **below** the isolation/RFC1918 DROPs:
   ```
   -I ROOMS_FWD <pos> -i tap-fc<k> -j ROOMS_EG_<k>
   ```
   `<pos>` = the 1-indexed line number of the supernet egress ACCEPT (`-s 172.16.0.0/24 -o <out_iface> -j ACCEPT`) in the live `ROOMS_FWD` dump; the jump goes at that position, pushing the ACCEPT down. Placing it below the isolation DROPs means an `allowlist` can never override guest↔guest isolation (those DROPs still fire first); placing it above the supernet ACCEPT means the per-room chain's terminal DROP prevents this room from reaching the permissive ACCEPT.
3. `ROOMS_EG_<k>` ends in a catch-all DROP, so nothing falls off its end back to the supernet ACCEPT. An `ACCEPT` inside it terminates traversal (packet permitted); the terminal `DROP` terminates traversal (packet blocked).

- **Install seam**: `firecracker::create_slot_tap` (`src/firecracker.rs:582`) — right after the tap is up and forwarding is enabled, *before* the VMM can transmit. Same fail-closed-before-guest-transmits posture the witness start already holds: a room asked for `--egress none` that cannot install its chain must not boot.
- **Remove seam**: `firecracker::release_tap` / `delete_tap` (`src/firecracker.rs:1521`/`1540`) — flush + delete `ROOMS_EG_<k>` and remove the one jump, beside the tap, on both the normal teardown path and the `gc` orphan sweep. Removal is idempotent and scoped by the chain's `<k>` name, so a double-teardown or a gc race never touches a live sibling's chain.

## Behavior / fix

1. **Flag surface.** New `--egress <policy>` on the `run`/`exec` path (`src/main.rs` `RunArgs`), parsed into an `egress::Policy` enum:
   - `none` → `Policy::None`
   - `allowlist:api.anthropic.com,10.1.2.0/24` → `Policy::Allowlist(Vec<Dest>)`
   - flag absent → `Policy::Observe` (today's behavior; **non-breaking default**).
   Parse errors (empty allowlist, unresolvable host at launch, malformed CIDR) fail fast, before any slot is claimed — mirroring the `--secret` / `--witness` pre-claim admission in `run_room_inner`.

2. **`none` enforcement.** `ROOMS_EG_<k>` holds just a log + drop, appended in order:
   ```
   -A ROOMS_EG_<k> -o <out_iface> -j LOG --log-prefix "rooms-egress-drop:<k> "
   -A ROOMS_EG_<k> -o <out_iface> -j DROP
   ```
   Everything forwarded from `tap-fc<k>` is dropped. Loopback inside the guest and vsock-to-host (`--secret`, #79) never transit `FORWARD`, so they are unaffected — `none` blocks *external* egress only, and a host-loopback model endpoint remains reachable (the task's stated allowance; wiring the endpoint is a separate follow-up). **DNS caveat**: under `none`, forwarded DNS is dropped too, so the guest cannot resolve external hostnames — expected, and the reason a local resolver / host-loopback endpoint is the intended companion.

3. **`allowlist:X` enforcement.** Resolve each host to IPs **once at launch** and pin them (avoid DNS rebinding); append an ACCEPT per pinned dest, then the log + catch-all drop — natural order, no reversal:
   ```
   -A ROOMS_EG_<k> -d <pinned_ip> -o <out_iface> -j ACCEPT    # per dest
   -A ROOMS_EG_<k> -o <out_iface> -j LOG --log-prefix "rooms-egress-drop:<k> "
   -A ROOMS_EG_<k> -o <out_iface> -j DROP
   ```
   **DNS honesty**: resolution is pinned at launch, so a dynamic-IP endpoint that rotates addresses mid-run will start dropping — documented as a v1 limitation, with CIDR form (`-d 1.2.3.0/24`) as the escape hatch. A hostname the guest resolves must still reach the *same* pinned IP the host allowed; DNS to the configured resolver is only reachable if the resolver's IP is in the allowlist, so `allowlist` with hostnames implies allowing the resolver too — call this out in `--help` and the operator doc.

4. **Anti-spoof + ordering invariant (the load-bearing part).** Two invariants, both the failure class `isolation.rs` exists to catch, so the synthesis + enforcement logic lives in `egress.rs` as **pure functions over rule dumps**, unit-tested against deliberately-broken inputs (the negative assertions must be able to fail):
   - *Anti-spoof*: the jump matches `-i tap-fc<k>`, not `-s <guest_ip>` — a spoofed source cannot dodge it (§ Threat model). A predicate `room_egress_enforced(tap, out_iface, forward_dump, eg_dump) -> bool` verifies both the jump (present, keyed on the tap, above the supernet egress ACCEPT, below the isolation DROPs) and the sub-chain (terminal catch-all DROP scoped by `-o <out_iface>`, no fall-through). Precise on `-i`/`-o` the way `isolation.rs::drop_precedes_egress` requires `-o` (`src/isolation.rs:106`) — a rule constraining only source misclassifies.
   - *Ordering*: append-built sub-chain rules can't reverse; only the single jump is position-sensitive, and the predicate pins it above the supernet ACCEPT. Refutations to test: jump below the supernet ACCEPT (leaks), jump keyed on source IP not tap (spoofable), sub-chain missing its catch-all DROP (falls through to the supernet ACCEPT), jump above the isolation DROPs (allowlist overrides guest↔guest).

5. **Host-recorded blocked attempts → receipt.** A dropped attempt must be host-recorded and land in the room's artifact, tamper-evident:
   - The witness `tcpdump` already sits on `tap-fc<k>` and captures the guest's *attempted* SYN even when `FORWARD` drops the packet downstream — so **attempted ∧ ¬permitted = blocked**, derivable from the existing pcap plus the policy with no new capture path.
   - Cross-check with the `ROOMS_EG_<k>` DROP rule's **packet counter** (`iptables -nvL ROOMS_EG_<k>`) read at teardown: a host-side count of packets this room's drop rule stopped, which the guest cannot alter.
   - `artifacts::Witness` (witness.json, `src/artifacts.rs:274`) gains an egress-policy record: `egress_policy` (`observe|none|allowlist`), the `permitted` destination set, and the `blocked` attempts (dest + packet count). **Bump `WITNESS_SCHEMA_VERSION`** (currently `1`, `src/artifacts.rs:253`) so consumers deserializing `witness.json` see the schema change rather than silently reading new fields as absent. For a `--egress none` run with no attempts, `permitted` is empty and `blocked` is empty — **the proof-of-absence artifact**: policy `none`, permitted `[]`, `capture_complete: true`, zero destinations.

6. **Doctor / degraded mode.** `run_room_inner` already fails fast when `ROOMS_FWD` isn't installed (`doctor::ensure_rooms_fwd_installed`, `src/main.rs:628`). `--egress` depends on that same chain — no new host-substrate requirement; an `--egress` request against a host missing the chain fails with the existing remediation.

## Acceptance

- A room launched `--egress none` cannot reach any external host; a room with `--egress allowlist:X` reaches X and nothing else — verified **from inside the guest** by attempting both (a permitted dest succeeds, a denied dest times out / is refused).
- A guest that **spoofs a different `172.16.0.x` source** under `--egress none` is still blocked (the tap-keyed jump catches it) — an explicit anti-spoof e2e case.
- Every blocked attempt appears host-side in `witness.json` with dest + count, derived outside the guest trust boundary (pcap + `ROOMS_EG_<k>` counter), never from guest self-report.
- Absent flag ⇒ unchanged behavior; existing witness and pool tests stay green.
- A `--egress none` run's receipt shows policy `none`, an empty permitted-egress set, and `capture_complete: true` with zero destinations — the proof-of-absence artifact.
- `make check` green. E2e (`tests/egress_e2e.rs`, host-only, `#[cfg(feature = "e2e")]`) covers `none`, `allowlist`, the spoof case, and blocked-attempt recording on the rooms-host.

## Test plan

Pure/unit (run in CI, any platform):
- `egress::parse` — `none`, `allowlist:host`, `allowlist:host,cidr`, empty allowlist errors, malformed CIDR errors, unknown mode errors.
- `egress` chain synthesis — `none_appends_log_then_drop`, `allowlist_appends_accept_per_dest_then_drop`, rules scope by `-o <out_iface>`; the jump names `-i tap-fc<k>`.
- `egress` enforcement predicate (isolation.rs style) — `enforced_when_tap_jump_precedes_supernet_accept`, and refutations: `a_jump_below_the_supernet_accept_leaks`, `a_source_keyed_jump_is_spoofable`, `a_subchain_without_catch_all_drop_falls_through`, `a_jump_above_the_isolation_drop_overrides_isolation`.
- `artifacts` — `witness_records_none_policy_as_proof_of_absence`, `blocked_set_is_pcap_minus_permitted`, `schema_version_bumped`.

E2e (rooms-host only, gated behind `e2e`):
- `egress_none_blocks_all_external`, `egress_none_blocks_spoofed_source`, `egress_allowlist_permits_only_listed`, `blocked_attempt_lands_in_receipt`, `absent_flag_is_observe_only`.

## Non-goals

- **Not** TLS interception or payload inspection — connection-level allow/deny by destination only.
- **Not** per-process policy inside the guest — the boundary is the room's network slot.
- **Not** local-model wiring — this task only *permits* a host-loopback endpoint under `none`; wiring the endpoint is a separate follow-up.
- **Not** dynamic mid-run allowlist changes or DNS-rebinding-resistant re-resolution — v1 pins at launch and documents the limitation; CIDR form is the escape hatch.
