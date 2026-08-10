# Phase 2 (fork-clones) — Q3 network spike + decomposition

**Status:** decided — the spike that §10 Q3 of [`spec.md`](spec.md) mandates *before* the slot rework.
**Date:** 2026-08-09
**Resolves:** spec §10 Q3 ("netns vs `network_overrides`"); sizes the phase-2 task breakdown.
**Grounding:** Firecracker `docs/snapshotting/network-for-clones.md` (upstream) + a full file:line
inventory of the current single-tap-per-room wiring on `origin/main @ 9902c6b`.

## The question (spec §10 Q3)

Every clone resumes with the **same guest IP/gateway frozen into the snapshot** — baked into the
kernel cmdline `ip=` at base boot (`build_boot_args`, `firecracker.rs:1287`) and captured in
`snapshot.mem`/`vmstate`. Restoring N of them into the host's single default namespace collides:
N taps cannot all route to the same guest `/30`. Does `/snapshot/load`'s `network_overrides` field
avoid a full network-namespace-per-clone rework, or not?

## Answer — netns is required; `network_overrides` does **not** avoid it

`network_overrides` remaps **only the host TAP device name** (`iface_id → host_dev_name`) at load.
It does not change the guest's IP, gateway, or MAC — those stay frozen. So even with distinct host
tap names, all N clones still present the **same guest IP** on the host return path: the host cannot
disambiguate which tap owns `172.16.0.4k+2`. Upstream's `network-for-clones.md` says the same and
still prescribes namespaces; it offers `network_overrides` only for the jailer-less case and notes it
"does not eliminate the need for namespace isolation."

**Decision: network-namespace-per-clone** (spec D3 confirmed). Each clone gets its own netns in which
the base's frozen guest `/30` is reused verbatim and is unambiguous. Because the tap lives in a
private namespace, we can recreate it under the **same name the snapshot baked** (`tap-fc<k>`), so
**`network_overrides` is not needed at all** — the snapshot's `host_dev_name` resolves to the
netns-local tap. (We add it later only if we ever rename netns taps.)

### Why this is actually the elegant path, not just the forced one

The per-clone distinctness moves off the *guest* side (frozen, shared) and onto a new *host* side
(the veth). All N clones legitimately share one frozen guest `/30`; the netns + veth pair is what
makes them distinct and isolated on the host. That means the clone allocator does **not** consume a
new guest `/30` per clone — it reuses the base's — and instead allocates a netns + a host-side veth
`/30` per clone from a separate host supernet.

## Target topology (per clone `i`, restored from snapshot of base slot `k`)

```
        netns  rooms-clone-<room_id>                         default namespace (host)
  ┌─────────────────────────────────────────┐
  │ tap-fc<k>  (SAME name the snapshot baked) │
  │   gateway .4k+1/30   ── guest .4k+2 (FC)  │
  │ jailer launched with --netns <ns>          │
  │ veth-g  <host-side /30 .b+2>               │────┐  veth pair
  │   default route → veth-h                    │    │
  │   MASQUERADE guest→veth-g (netns-internal)  │    │
  └─────────────────────────────────────────┘    │
                                                    ▼
                                        veth-h <host-side /30 .b+1>
                                        host route + MASQUERADE veth→upstream
                                        egress policy + witness enforced here / in-ns
```

- **Guest side (frozen, shared across clones):** `tap-fc<k>`, gateway `.4k+1`, guest `.4k+2` — reused
  in every clone's netns. No change to the snapshot, no `ip=` rewrite, no `network_overrides`.
- **Host side (new, distinct per clone):** the netns + a veth `/30` drawn from a **second** supernet
  (disjoint from the guest `172.16.0.0/24`). Two-hop SNAT: netns masquerades guest→veth, host
  masquerades veth→upstream.
- **jailer `--netns`** is the clean integration seam: Firecracker's jailer enters a named netns
  before exec, so the FC process + its tap live in the namespace with zero guest-visible change.

## Blast-radius decision (spec NFR "non-forked rooms byte-for-byte unchanged")

netns is a **clone-path-only** mechanism. Cold boot and single `rooms restore` (phase 1) stay on the
existing flat default-namespace tap model untouched — one live lease on the frozen slot, exactly as
shipped. `rooms clone -n N` is the only caller that enters the netns fan-out. This preserves the
blast-radius NFR and keeps the rework additive rather than a rewrite of the boot path.

## Lease-model consequence

Phase 1 allows exactly **one** live restore because the frozen guest IP is identical in the default
namespace (`slot::lease`, one live `@lease` per reservation, `slot.rs:403`). Under netns the guest
`/30` is no longer contended — N clones hold it simultaneously in N namespaces — so the clone path
takes **N leases against the snapshot reservation**, disambiguated by the per-clone netns+veth
allocation, not by the guest slot. The reservation/tombstone teardown discipline
(`hold_lease_for_teardown`, `finish_teardown`, `restore_exec.rs:369`) generalizes to N; the free-lock
must now serialize netns+veth reclaim, not just tap-delete.

## Rework surface (from the inventory — every name-derivation/attach point netns touches)

| Concern | Today (flat ns) | Phase-2 change |
|---|---|---|
| Slot→identity | `derive()` `slot.rs:183` — one `/30` = tap+gw+guest | add a **netns+veth axis**; guest `/30` reused from base |
| Tap create | `create_slot_tap` `firecracker.rs:667` (default ns) | create `tap-fc<k>` **inside the netns**; add veth pair + netns lifecycle |
| Guest IP | frozen `ip=` `firecracker.rs:1287` | unchanged (reused per netns) |
| Egress | `ROOMS_EG_<k>` keyed `-i tap-fc<k>` `egress.rs:445`, attach `firecracker.rs:528` | re-scope enforcement to the clone (in-netns tap or host veth) |
| Witness | `tcpdump -i tap-fc<k>` `witness.rs:139`, attach `firecracker.rs:517` | `ip netns exec <ns> tcpdump -i tap-fc<k>` — per-clone pcap |
| Restore custody | install witness+egress before Resumed `restore_exec.rs:220` | same ordering, but inside the netns |
| `/snapshot/load` | no `network_overrides` `restore_exec.rs:238` | **unchanged** — tap name reused; no override needed |
| Host NAT substrate | `setup-tap.sh` ROOMS_FWD/NAT, `isolation.rs` consts (one flat ns) | add veth-supernet MASQUERADE; two-hop SNAT |
| jailer spawn | `RestoreLaunch`/`spawn_restore` `firecracker.rs:1683+` | pass `--netns <ns>` |
| GC/reconcile | slot free / tap delete | also reclaim leaked netns + veth |

## Decomposition (materialized as dossier tasks under phase `fork-clones`)

1. **2a — netns+veth allocator + host NAT substrate.** The long-pole core: a second allocator axis
   (`CloneNet` = netns name + veth `/30` from a disjoint host supernet), netns/veth lifecycle
   (`ip netns add/del`, veth create/move/addr/route), two-hop MASQUERADE, `isolation.rs`/`setup-tap.sh`
   substrate rework, GC reclaim of leaked netns/veth. No FC integration yet — pure host-networking
   allocator with unit + host tests. **L; may split allocator vs NAT substrate if it overruns the band.**
2. **2b — restore into a netns (custody-in-namespace).** jailer `--netns`; create the frozen
   `tap-fc<k>` inside the netns; move witness (`ip netns exec … tcpdump`) and egress enforcement into
   the clone; keep custody-install-before-`Resumed`. One clone restored into one netns end-to-end. **M/L.**
3. **2c — `rooms clone <snap> -n N` + per-clone hygiene fan-out.** N-lease model against the snapshot
   reservation; concurrent restore with the pool cap; per-clone vsock resume nudge (reseed / clock /
   per-clone secret + `run_id` + `git_identity`, `RESUME_PORT`); fail-closed on any clone's missing ack. **M/L.**
4. **2d — VALIDATION GATE (host, rooms-host).** The killer demo, spec §9: `clone -n 8` < 1s; aggregate
   **PSS** proves CoW (≪ 8×256 MiB); 8 parallel real tasks; 8 distinct witness pcaps; distinct RNG +
   no host-path collision + verified cross-clone isolation (A can't reach B) + distinct `sshd` keys.
   Human-gated; not a code PR. **Blocks phase 3.**

Ordering: 2a → 2b → 2c → 2d, strictly sequential (each depends on the prior). 2a is the reviewable
first increment.
