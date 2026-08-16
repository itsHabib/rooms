# Kickoff — Task 2a: netns+veth allocator + host NAT substrate

**Audience:** the implementing agent. Rip this and build; it is self-contained.
**Dossier task:** `rooms` / phase `fork-clones` / `p2-netns-veth-allocator` (`tsk_01KZME5Y0YWWAZQ06CQDTD7WKT`).
**Worktree:** `.claude/worktrees/feat-p2-netns-veth-allocator`, branch `feat/p2-netns-veth-allocator` off `origin/main @9902c6b`.
**Design record (read first, in order):**
1. [`phase2-netns-spike.md`](phase2-netns-spike.md) — the Q3 decision + topology + **§Review revisions R1–R7** (the adversarial review that tightened this task; R2/R3/R6/R7 are yours).
2. [`spec.md`](spec.md) §4 D3 (netns-per-clone) + D8 (slot reservation), §8 (fork hygiene matrix).

## What you are building (and NOT building)

A **host-side network allocator** — a second allocation axis so N clones restored from one snapshot each get a distinct, isolated host identity (a network namespace + a veth pair) while sharing the base's frozen guest `/30` inside their namespaces. This is **pure host-networking**: `ip netns` / `ip link` / `iptables`, plus the on-disk claim discipline. It does **NOT** touch Firecracker, the restore path, snapshots, or the `rooms clone` CLI — those are tasks 2b/2c. You can build and test 2a end-to-end with zero FC involvement.

**In scope**
- A new allocator module (`src/clonenet.rs`) that hands out `CloneNet` allocations and reclaims leaked ones, **reusing `slot.rs`'s crash-discipline primitives** (R7).
- netns + veth lifecycle mechanism: create/destroy a namespace, a veth pair, addresses, routes, two-hop MASQUERADE.
- A **separate** iptables chain `ROOMS_VETH_FWD` for the veth supernet, with `isolation.rs`-style pure predicates that unit-test every way its isolation can break (R2, R3).
- The `setup-tap.sh --host` substrate additions for the veth supernet (host MASQUERADE + the base chain + the second FORWARD jump).

**Out of scope (do not touch)**
- jailer `--netns`, `restore()`, `/snapshot/load`, witness/egress *installation* at restore time → **2b**.
- `rooms clone -n N`, the multi-lessee lease relaxation → **2c**.
- The flat cold-boot / single-restore path. **R3: your diff to `ROOMS_FWD` and the flat `isolation.rs` fixtures must be literally zero.**

## Why netns (the one-paragraph version)

Every clone resumes with the **same** guest IP/gateway frozen into the snapshot (kernel `ip=`, `firecracker.rs:1287`). `network_overrides` only remaps the host tap *name*, not the guest IP, so N clones collide on the host return path. The fix: each clone runs in its own **network namespace**, where the base's frozen guest `/30` (e.g. `172.16.0.4k/30`) is reused verbatim and is unambiguous. Per-clone distinctness moves to a **new host-side axis** — a veth pair from a **second supernet, `172.17.0.0/24`** — bridging each netns to the host. Two-hop SNAT: MASQUERADE guest→veth *inside* the netns, then MASQUERADE veth→upstream on the host. (This task builds that host-side axis; 2b puts the guest into it.)

## The code you are mirroring — anchors

### Mirror this (the pattern to copy): `src/slot.rs`
The slot allocator is the exact shape your `clonenet` allocator should take. **Reuse its primitives; do not fork a second, subtly-different reconcile path (R7).**
- `derive(index)` `slot.rs:183` — pure index→identity math (`base = 4*index`; tap/gateway/guest from `172.16.0.0/24`). Your `CloneNet::derive` is the analog over `172.17.0.0/24`.
- `claim()` `slot.rs:80` + `try_claim()` `:133` — `O_CREAT|O_EXCL` claim files under `<state>/slots/<k>`; the filesystem is the race arbiter. Your files live under `<state>/clonenets/<k>`.
- `Claimer` `:50` (pid+starttime liveness token written into the claim file), `lock_frees()` `:171` (the free-lock held only across verify+unlink), `rewrite_slot_atomic()` `:301` (temp-then-rename), `sync_dir()` `:319`.
- `free()` / `reconcile()` (`:211`, ~`:595`) — compare-and-delete + leaked-claim reclamation by probing the dead claimer. Your `reclaim` reclaims a leaked netns+veth the same way.
- `SLOTS_DIR` `:30`, `MAX_SLOT=63` `:39` (the `/24` carve = 64 `/30`s minus slot 0). Same carve for `172.17.0.0/24`.
- **Factor, don't copy:** if the O_EXCL-claim / free-lock / atomic-rewrite / liveness-token logic is generic over "an indexed resource under a state dir," lift it into a shared helper both `slot` and `clonenet` call. A little duplication is OK per house style, but a *second reconcile path* is exactly what R7 says to avoid.

### Mirror this (the predicate style), do NOT modify it: `src/isolation.rs`
Pure string analysis of `iptables -S` dumps proving the flat `ROOMS_FWD` isolates guest↔guest. **You add a parallel set for `ROOMS_VETH_FWD`; you change nothing here that the flat path asserts.**
- `supernet!()` macro `:16` + `SUPERNET` `:23`, `ISOLATION_DROP` `:26`, `FORWARD_JUMP` `:35`.
- `forward_jump_is_first()` `:47` — checks only the *first* `-A FORWARD` line, so adding a **second** jump `-A FORWARD -j ROOMS_VETH_FWD` after the existing one keeps this true. Verify that.
- `has_isolation_drop` / `no_accept_before_drop` / `drop_precedes_egress` / `rooms_fwd_isolates` `:66-126` — copy this shape for `ROOMS_VETH_FWD`, including the **negative-assertion tests** (`:159-240`) that prove each way isolation can break is caught. A test that cannot fail is worthless — replicate that discipline.

### Reference (what the flat path does at the host, so you match the seam): `src/firecracker.rs`
- `create_slot_tap()` `:667` — `ip tuntap add … user firecracker` / `ip addr add <gw>/<prefix>` / `ip link set up` / `set_tap_forwarding()` `:708` (`sysctl net.ipv4.conf.<tap>.forwarding=1`). Your netns+veth setup is the analog; **R6: enumerate the per-`veth-h` + in-netns forwarding sysctls and the connected `/30` route** — the flat path's single `forwarding=1` is not enough for a two-hop path.
- `run_ip()` helper `:690` — the thin `ip` command wrapper. Reuse/extend for `ip netns exec …`.

### The host substrate script: `scripts/setup-tap.sh`
- Builds `ROOMS_FWD` (`:22`), guest↔guest + RFC1918 DROPs (`:72-76`), supernet egress ACCEPT (`:78`), NAT `POSTROUTING … MASQUERADE` (`:97`), all for `172.16.0.0/24`.
- **You add** a disjoint block for `172.17.0.0/24`: the `ROOMS_VETH_FWD` chain, its FORWARD jump (second, after `ROOMS_FWD`), the cross-clone DROP, the egress ACCEPT + return, and the host-side `POSTROUTING -s 172.17.0.0/24 -o <upstream> MASQUERADE`. **Leave the `172.16` block byte-for-byte unchanged.**

## Concrete design

### `CloneNet` (data model)
```
struct CloneNet {
    index: u8,           // 1..=MAX_SLOT, its own O_EXCL walk under <state>/clonenets/<k>
    netns: String,       // e.g. "rooms-c<index>"  (namespace name)
    veth_host: String,   // host-side veth end, stays in default ns (IFNAMSIZ ≤ 15 chars!)
    veth_guest: String,  // guest-side veth end, moved into the netns
    host_ip: Ipv4Addr,   // 172.17.0.(4*index + 1)  — veth-host address (the netns's gateway)
    netns_ip: Ipv4Addr,  // 172.17.0.(4*index + 2)  — veth-guest address inside the netns
    prefix: u8,          // 30
}
```
- **Independent axis:** a `CloneNet` index is *not* the snapshot slot index. A clone = (a leased snapshot slot → the frozen guest `/30`, handled in 2c) **+** (a `CloneNet` → this host-side veth `/30`). 2a allocates only the latter.
- **IFNAMSIZ gotcha:** Linux interface names are ≤ 15 bytes. `veth-h<index>` (≤ 8) and `veth-g<index>` are safe; keep any scheme short. netns names have no such limit.

### netns + veth lifecycle (mechanism)
On allocate (after the O_EXCL claim succeeds):
1. `ip netns add <netns>`
2. `ip link add <veth_host> type veth peer name <veth_guest>`
3. `ip link set <veth_guest> netns <netns>`
4. host: `ip addr add <host_ip>/30 dev <veth_host>` ; `ip link set <veth_host> up`
5. in-netns: `ip -n <netns> addr add <netns_ip>/30 dev <veth_guest>` ; `ip -n <netns> link set <veth_guest> up` ; `ip -n <netns> link set lo up` ; default route `ip -n <netns> route add default via <host_ip>`
6. forwarding (R6): `sysctl net.ipv4.conf.<veth_host>.forwarding=1`; in-netns `sysctl net.ipv4.ip_forward=1` (or per-iface); confirm the host has the connected `/30` route to reach `netns_ip`.
7. in-netns MASQUERADE (hop 1): `ip netns exec <netns> iptables -t nat -A POSTROUTING -o <veth_guest> -j MASQUERADE` (rewrites the guest `172.16` source to `netns_ip` before it crosses the veth).
   - hop 2 (host `172.17`→upstream) is **substrate**, installed once by `setup-tap.sh`, not per-clone.

On free/reclaim (teardown or reconcile of a leaked claim): delete the netns (`ip netns del` — takes the veth pair + in-netns rules with it), remove the claim file under the free-lock. Idempotent; a half-torn state must converge.

### `ROOMS_VETH_FWD` rules (R2 — the full set, all unit-tested)
Host default-ns chain, jumped from `FORWARD` **second** (after `ROOMS_FWD`):
1. `-A ROOMS_VETH_FWD -s 172.17.0.0/24 -d 172.17.0.0/24 -j DROP` — **cross-clone A↛B** (guests are in separate netns; inter-clone traffic can only transit the host via the veths — this is what isolates them).
2. `-A ROOMS_VETH_FWD -s 172.17.0.0/24 -d 10.0.0.0/8 -j DROP` (+ other RFC1918) — mirror the flat RFC1918 drops.
3. `-A ROOMS_VETH_FWD -s 172.17.0.0/24 -o <upstream> -j ACCEPT` — egress.
4. `-A ROOMS_VETH_FWD -d 172.17.0.0/24 -i <upstream> -m state --state RELATED,ESTABLISHED -j ACCEPT` — return path.

Plus the **guest→host INPUT-drop shape** (R2 ii) — the veth analog of today's per-room `INPUT -i tap-fc<k> -j DROP` (`egress.rs:301`): `-A INPUT -i <veth_host> -j DROP` for the `none` posture. **2a defines + unit-tests the predicate that verifies this rule's presence/ordering; the live per-clone install happens in 2b.** (State that boundary in the module doc so 2b knows to call it.)

### Predicates to write (mirror `isolation.rs`)
- `rooms_veth_fwd_isolates(dump)` — cross-clone DROP present, unpreempted by a `172.17` ACCEPT above it, ahead of the egress ACCEPT.
- `forward_jumps_ordered(forward_dump)` — `ROOMS_FWD` still first, `ROOMS_VETH_FWD` present.
- `veth_input_drop_present(input_dump, veth)` — the guest→host drop for a given veth.
- Negative-assertion tests for each (a missing drop, an ACCEPT above the drop, a broad match-less ACCEPT, a drop after egress) — the `isolation.rs:159-240` pattern.

## Acceptance (from the task spec)

- **Unit:** distinct netns+veth per index, no host-side collision, reservation-exempt from the walk; `ROOMS_VETH_FWD` + INPUT-drop predicates with full negative-assertion coverage; `CloneNet::derive` math.
- **Flat-path diff == 0:** `ROOMS_FWD` and the flat `isolation.rs` fixtures unchanged (grep the diff).
- **Host test (rooms-host, gated behind the e2e feature like existing host tests):** allocate 3 `CloneNet`s; each reaches upstream through its own two-hop NAT; **cross-clone A↛B verified** (ping from netns A to netns B's guest IP fails); guest→host INPUT drop verified for `none`; the base guest `/30` reused in all three without collision; teardown leaves `ip netns list` clean and no leaked veths.
- `make check` green (fmt + clippy `-D warnings` + tests). Watch the `clippy.toml` caps: cognitive 20, ≤100 lines/fn, ≤6 args. No `unwrap`/`panic!`/`indexing_slicing` in non-test code (lint forbids). No `#[allow]` without a one-line justification.

## Weak spots / bail-points (where this gets sharp)

1. **Two-hop return path (R6/M3).** Easy to get the forward path working and miss the reverse route — the host needs the connected `/30` route to `netns_ip`, and forwarding must be on for *both* the veth-host and inside the netns. The host test must assert **bidirectional** reachability, not just clone→upstream. If a clone can reach upstream but replies never return, this is the cause.
2. **The second FORWARD jump ordering.** `ROOMS_FWD` must stay first (the flat isolation invariant). Adding `ROOMS_VETH_FWD` second is safe *because* `172.17` traffic doesn't match `172.16`-scoped `ROOMS_FWD` rules and falls through — verify that falling-through is actually what happens (no default DROP in `ROOMS_FWD` that eats it).
3. **`ip netns del` teardown completeness.** Deleting the netns should reap the veth pair and in-netns rules, but a veth whose host end lingers (partial create) needs explicit cleanup in reconcile. Test the crash-mid-create path.
4. **Root/privilege.** `ip netns` / `iptables` need `CAP_NET_ADMIN`; the flat path already runs `rooms` under `sudo -E` on the host. Match that; don't invent a new privilege model.
5. **Don't leak into the flat path.** If you find yourself editing `slot.rs`'s `derive` or `isolation.rs`'s flat constants, stop — that's the blast-radius NFR breaking. The veth axis is additive and disjoint.

## Definition of done

`make check` green; flat-path diff zero; the host test passes on rooms-host (Lima VM `rooms-host` — see memory `rooms-host-mac-lima.md` for build/run: build with `cargo build --release --target-dir ~/rooms-target-restore`, run under `sudo -E env HOME=$HOME`); PR opened with the reviewer panel (Copilot + `@codex/@claude/@cursor review`); dossier task `p2-netns-veth-allocator` updated with the PR. If the diff overruns the stretch band (~1000 weighted LOC), split allocator vs NAT-substrate into two PRs (the task spec allows this).

---

## Ready-to-paste handoff prompt

> Implement task 2a of the rooms snapshot-fork phase 2: the **netns+veth allocator + host NAT substrate**. Work in the worktree `/Users/mh/dev/rooms/.claude/worktrees/feat-p2-netns-veth-allocator` (branch `feat/p2-netns-veth-allocator`).
>
> **Start by reading** `docs/features/snapshot-fork-replay/2a-netns-allocator-kickoff.md` — it is your complete brief (design, code anchors, the exact iptables rule set, acceptance, and the sharp edges). Then skim its two referenced docs (`phase2-netns-spike.md` §Review revisions, `spec.md` §4 D3/D8, §8) and the dossier task `p2-netns-veth-allocator`.
>
> Build a `src/clonenet.rs` allocator that **reuses `slot.rs`'s O_EXCL-claim / free-lock / atomic-rewrite / reconcile primitives** (factor shared logic; do not fork a second reconcile path). Add the `ROOMS_VETH_FWD` chain + an `isolation.rs`-style predicate module with full negative-assertion tests, and the `172.17.0.0/24` substrate additions to `scripts/setup-tap.sh`. This is **host-networking only** — no Firecracker, restore, snapshot, or CLI changes (those are 2b/2c).
>
> **Hard constraints:** (1) your diff to the flat `ROOMS_FWD` and `isolation.rs` flat fixtures must be **zero** (blast-radius NFR); (2) the veth substrate is a **separate** chain, jumped second after `ROOMS_FWD`; (3) enforce the **full** veth rule set — cross-clone DROP, egress ACCEPT, return path, AND the guest→host INPUT-drop predicate; (4) enumerate the forwarding sysctls + reverse `/30` route so the return path works, and assert **bidirectional** reachability in the host test. `make check` must be green (clippy `-D warnings`, complexity caps, no `unwrap`/`panic!` in non-test code). Mind IFNAMSIZ (≤15-char interface names). Open a PR with the standard reviewer panel when green.
