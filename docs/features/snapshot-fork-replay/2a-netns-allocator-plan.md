> **Status:** draft — task-2a implementation plan drafted by a drive driver session (Opus) on 2026-08-09, pending human review. The netns/veth networking specifics (supernet choice, two-hop MASQUERADE, rp_filter) especially need a checking pass before implementation.

Read the spike, the spec, and the current wiring. Here's the plan for **2a only**.

---

# Task 2a — `CloneNet` allocator + host NAT substrate

## 1. Shape of the increment

2a is **pure host networking**: a second allocator axis and the substrate it needs, with *no* Firecracker integration (`--netns`, in-ns tap, witness/egress re-scoping all belong to 2b). The deliverable is: given an index, we can materialize an isolated namespace with working two-hop egress, reclaim it, and detect leaks — provable on the rooms-host without booting a VM.

### Naming / addressing decisions (with one deviation from the spike)

| Thing | Value | Why |
|---|---|---|
| Clone supernet | `172.31.0.0/22` (override `ROOMS_CLONE_SUPERNET`) | Disjoint from the guest `172.16.0.0/24` (`isolation.rs:23`) but *inside* `172.16.0.0/12`, so the existing guest-side DROP `-s 172.16.0.0/24 -d 172.16.0.0/12` (`setup-tap.sh:76`) already blocks a guest from addressing a clone's veth directly — free defense, no new guest-side rule. 1024 addrs = 255 usable `/30`s. |
| Clone `/30` | index `i`, base `4i` → veth-h `.4i+1` (host), veth-g `.4i+2` (in-ns) | Byte-for-byte the same carve `slot::derive` uses (`slot.rs:183`), so one mental model. |
| Index range | `1..=MAX_CLONE (=255)`; 0 reserved | Mirrors the slot-0 reservation discipline (`slot.rs:5`, `ensure_pool_index` `slot.rs:119`). |
| netns name | `rooms-c<i>` | **Deviation:** the spike sketched `rooms-clone-<room_id>`. Index-derived names make GC parseable from `ip netns list` alone with no state read, and mirror the slot allocator exactly. The owning `room_id` lives *inside* the allocator file, same as a slot token. |
| veth names | `vh<i>` (host side) / `vg<i>` (in-ns) | `IFNAMSIZ` is 16 — `veth-clone-<room_id>` doesn't fit; `vh255` does. The in-ns name is namespace-local so it never collides. |

The guest `/30`, `tap-fc<k>`, and the frozen `ip=` are untouched — 2a never derives a guest address.

## 2. Layer mapping

Strictly additive, no new downward import:

```
config (clonenets_dir)  ─┐
room (CloneNet data)     ├─ clonenet.rs   POLICY: derive, carve, validate, claim/free/reconcile
isolation.rs (predicates)┘        │             + argv PLAN builders (pure Vec<String>)
                                  ▼
                        netns.rs   MECHANISM: runs `ip` / `ip netns exec iptables`
                                  ▼
                        registry.rs (gc reclaim)  ──▶ main.rs (hidden CLI + gc/doctor wiring)
```

- **`src/clonenet.rs` (new, config/room tier — no I/O except its own allocation files).** Policy only.
  - `pub struct CloneNet { index: u8, netns: String, veth_host: String, veth_ns: String, host_addr: Ipv4Addr, ns_addr: Ipv4Addr, prefix: u8 }` — plain data, `Serialize`/`Deserialize`, sibling of `room::Slot` (`room.rs:43`). It goes in `room.rs` if it needs to land in `room.json`; for 2a it does **not** (no room owns one yet), so it lives in `clonenet.rs` and moves to `room.rs` in 2b when `RoomMeta` gains the field. Call that out in the PR so 2b doesn't re-litigate it.
  - `fn derive(index) -> CloneNet`, `const fn ensure_clone_index(index)` — direct analogues of `slot.rs:183` / `slot.rs:119`.
  - **Allocation** over `<state>/clonenets/<i>`: `claim` (`O_CREAT|O_EXCL` + `<room_id>\n<pid> <starttime>\n` token), `free` (compare-and-delete under a `clonenets.lock` free-lock), `reconcile` (judge by claimer liveness). This is a deliberate ~120-line mirror of `slot.rs`'s *simple* subset — it does **not** duplicate the reservation/lease/tombstone machinery (`@reservation`/`@lease`, `slot.rs:256-264`), which stays slot-only. I considered extracting a shared `indexfile` core out of `slot.rs` instead; I'd rather not refactor a 1893-line module carrying live lease semantics inside a network PR. If a reviewer pushes back, that extraction is a clean follow-up, not a blocker.
  - **Plan builders**: `pub fn create_plan(&self, out_iface: &str) -> Vec<Vec<String>>` and `destroy_plan(&self)` returning argv vectors (`["netns","add","rooms-c1"]`, …). Pure ⇒ snapshot-testable in CI with zero root. This is the policy/mechanism seam that makes the whole thing unit-testable off-host.
- **`src/netns.rs` (new, firecracker/rootfs tier).** Dumb executor: runs the plans, maps non-zero exits into `FirecrackerError`/a new `NetnsError`. Reuses the `run_ip` shape at `firecracker.rs:687` — factor `run_ip` out of `firecracker.rs` into `netns.rs` and have `create_slot_tap` (`firecracker.rs:667`) call it, so there's one `ip` runner, not two. `#[cfg(not(unix))]` stubs mirror the existing pattern. Imports `clonenet`, never the reverse.
- **`src/isolation.rs`.** Add a `clone_supernet!()` macro + `CLONE_SUPERNET`, `ROOMS_CLONE_FWD` chain const, `CLONE_ISOLATION_DROP`, `CLONE_FORWARD_JUMP`, and clone-scoped analogues of `forward_jump_is_first` / `no_accept_before_drop` / `drop_precedes_egress` / `rooms_fwd_isolates`. Generalize the existing bodies over a supernet parameter (private `fn isolates(dump, supernet)`), keeping the current `pub fn`s as thin wrappers so no caller changes. Plus one new test asserting the two supernets are **disjoint** — the invariant the whole design rests on.
- **`src/config.rs`.** `pub fn clonenets_dir(&self)` beside `slots_dir` (`config.rs:96`); `CLONENETS_DIR` const in `clonenet.rs` beside `SLOTS_DIR` (`slot.rs:30`).
- **`src/registry.rs`.** `reconcile_leaked_netns(config)` called from `gc` right after `reconcile_leaked_slots` (`registry.rs:249`), under the same guards (skip on `--dry_run`, skip on `--only`, skip when snapshot intents pending). Two-sided reclaim: (a) `clonenet::reconcile` removes allocator files whose claimer is dead; (b) enumerate `ip netns list`, keep only names parsing as `rooms-c<i>` (canonical spelling only — same `slot_index_of` strictness at `slot.rs:524`), and `ip netns del` any with no live allocator file. Deleting the ns destroys the veth pair and the in-ns iptables automatically; the only host-side residue is the `vh<i>` peer, which the kernel reaps with its partner — assert that in the host test rather than trusting it.
- **`src/main.rs`.** One hidden dev subcommand `rooms clonenet {claim,free,ls}` (`#[command(hide = true)]`) — it exists so the host test script can drive the allocator without a VM, and so 2b has a debugging handle. No user-facing surface.
- **`scripts/setup-tap.sh`.** New `ROOMS_CLONE_FWD` chain, same shape as `ROOMS_FWD`: clone↔clone DROP, RFC1918 DROPs, egress ACCEPT out `$OUT_IFACE`, `RELATED,ESTABLISHED` return, marker-comment DROP tail; jumped from `FORWARD` position 1 (installed *after* the `ROOMS_FWD` jump so `ROOMS_FWD` stays first — the existing `forward_jump_is_first` predicate must keep passing, and the clone predicate checks position 2). Plus `-t nat -A POSTROUTING -s $CLONE_SUPERNET -o $OUT_IFACE -j MASQUERADE` (host hop; the guest→veth hop is masqueraded *inside* the ns by `netns.rs`). Bump the marker to `rooms:fwd:v2:...` so `doctor` flags hosts on the old layout. Symmetric idempotent teardown; record/restore `net.ipv4.conf.<vh>.forwarding` and (if we must relax it) `rp_filter` through the same `/run/rooms` state-file discipline used at `setup-tap.sh:103`.
- **`src/doctor.rs`.** One added check that the clone chain + NAT rule are present, reusing the new predicates (`doctor.rs:701` pattern). Cheap; drop it to 2b if the band gets tight.

## 3. PR sizing — yes, split

Weighted estimate for one PR: ~815 production LOC + ~540 test/script (0.5× / 0×) ≈ **1085 weighted** — past the 1000 stretch band. Split into two sequential PRs:

**2a-i — `feat/clonenet-allocator`** (~470 weighted): `clonenet.rs` (derive + carve + claim/free/reconcile + plan builders), `config.rs` dir, `isolation.rs` predicates + disjointness test, unit tests. Entirely CI-testable, zero root, no host dependency. This is the reviewable core — the addressing and race discipline reviewers actually need to think about.

**2a-ii — `feat/clonenet-netns-substrate`** (~560 weighted): `netns.rs` + the `run_ip` consolidation, `setup-tap.sh` clone chain + two-hop MASQUERADE, `registry.rs` gc reclaim, hidden CLI, `doctor` check, `scripts/test-clone-netns.sh`. Mostly mechanism and shell — reviews fast despite the size, and its correctness is demonstrated by the host script, not by reading.

If the operator wants one PR, `feat/phase2-clonenet` is the name — but it lands ~1085 weighted and I'd expect a sizing finding.

## 4. Test strategy

**Unit (CI, `make check`, no root, no Linux):**
- `derive` determinism + exact `/30` arithmetic at boundaries (`i=1`, `i=MAX_CLONE`); index 0 and `MAX_CLONE+1` rejected.
- Clone supernet ∩ guest supernet = ∅ — asserted as a real test, not a comment.
- Allocator: `O_EXCL` claim wins exactly once under concurrent claimers (mirror the existing `slot.rs` racing tests, `tempdir`-backed); `free` is compare-and-delete (`AlreadyFree` / `AlreadyReassigned` equivalents); `reconcile` reclaims a dead claimer's file and leaves a live one; stray filenames (`01`, `+1`, `0`, `256`, `.1.tmp`) never parse as an index.
- Plan builders: exact argv snapshots for create/destroy, including the in-ns MASQUERADE and the default route — this is where a silent typo in a shelled-out command would otherwise only surface on the host.
- `isolation.rs`: the full negative battery for the clone chain, one test per way it breaks — missing DROP, dest-ACCEPT above the DROP, source-only ACCEPT above the DROP, broad matchless ACCEPT, DROP after the egress ACCEPT, jump missing, and a new one: **the clone jump displacing the `ROOMS_FWD` jump from position 1**. The existing module's stated ethos ("a test that cannot fail is worthless", `isolation.rs:11`) applies verbatim.

**Host (rooms-host / Lima, root, not CI — `scripts/test-clone-netns.sh`, following `test-tap-rules.sh`):**
1. `setup-tap.sh --host` → both chains present, correctly ordered, `ROOMS_FWD` jump still first, both NAT rules present; re-run is idempotent.
2. Claim 4 clonenets → 4 namespaces exist, veths up, addresses exact, `ip netns exec rooms-c<i> ip route` shows the default via `vh<i>`.
3. **Egress works through two hops:** `ip netns exec rooms-c1 ping -c1 8.8.8.8` (or a TCP connect if ICMP is filtered on the Lima uplink).
4. **Cross-clone isolation:** `rooms-c1` cannot reach `rooms-c2`'s `/30` — the load-bearing negative, and a direct down-payment on the 2d gate's "A can't reach B".
5. Free → namespace gone, `vh<i>` gone from the host, allocator file gone.
6. **Leak reclaim:** hand-create `rooms-c7` with no allocator file, run `rooms gc` → reclaimed. And the inverse: a namespace with a *live* allocator file is left alone.
7. `--host --teardown` removes both chains and restores recorded sysctls; running it twice is clean.

Known host risks to shake out in step 2–3 on the aarch64 Lima host: `ip netns` needs `/var/run/netns` writable under the sudo quirks already recorded for that VM; in-namespace iptables is a separate rule set (every in-ns rule must go through `ip netns exec`, never the host binary); and the two-hop return path may need `rp_filter` relaxed on `vh<i>` — recorded and restored via the existing `/run/rooms` state-file pattern, never flipped globally.

## 5. Out of scope for 2a (stated so review doesn't drift)

jailer `--netns` (`firecracker.rs:1683`), creating `tap-fc<k>` inside the namespace, `ip netns exec … tcpdump` witness (`witness.rs:139`), egress chain re-scoping (`egress.rs:445`), the N-lease model against a snapshot reservation, and `rooms clone`. 2a ships an allocator and a substrate that nothing in the boot or restore path calls yet — cold boot and phase-1 `rooms restore` stay byte-for-byte unchanged, which is exactly the blast-radius NFR the spike protects.