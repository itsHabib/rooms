# snapshot / fork / replay — Technical Design Document

**Status:** draft / proposal — NOT a build commitment. The artifact we decide from.
**Owner:** @itsHabib
**Date:** 2026-07-26
**Related:** [`docs/vision.md`](../../vision.md) (v0.2 roadmap line) · [`docs/features/vsock-secrets/spec.md`](../vsock-secrets/spec.md) (the per-clone identity channel this reuses) · [`docs/features/host-witness/spec.md`](../host-witness/spec.md) + egress work (the per-clone observability this composes with) · memory `rooms-redirect-away-from-custody` (why this is the bet) · Firecracker `docs/snapshotting/{snapshot-support,random-for-clones,network-for-clones,versioning}.md`

> **Reviewers — focus areas:**
> - **§4 D1** — fork is the bet, snapshot is the mechanism, deterministic replay is a trap (rescoped to checkpoint receipts, §4 D5). Push on whether that split is right.
> - **§4 D2 / §7 C** — the **neutral-snapshot ordering invariant**: snapshot a guest that holds NO secrets, fork, then deliver per-clone secrets over vsock. This is the load-bearing security rule; if it's wrong the whole design leaks.
> - **§4 D3 / §9** — netns-per-clone is the long pole and it drags egress-chain install + witness tap-naming with it. The phasing deliberately lands single-restore value (phase 1) *before* the netns rework (phase 2) so snapshot value survives if netns slips.
> - **§8** — the fork hygiene matrix (RNG/clock/identity duplication) — every row needs a mitigation that actually fires.

## 1. Problem & hypothesis

`/work-driver` fans out N coding-agent tasks, each into its own room. Today every room pays
the **full cold cost**: boot the microVM (~150–300ms), transport + clone the repo, warm the
agent toolchain (install/JIT the `claude` or cursor runtime, prime caches) — seconds to
minutes, paid N times, and the host can only hold so many rooms built from scratch at once.

The cold *boot* is not the expensive part; the **repo transfer + toolchain warm-up** is. And
that warm state is identical across a fan-out batch until the task-specific work begins.

**The bet:** Firecracker's snapshot/restore lets us pay the warm-up **once** and **fork** it.
Boot one room, clone the repo, warm the runtime, snapshot the paused guest, then restore that
snapshot into **N clones** that share the warm memory copy-on-write and each start
seconds-to-minutes of setup ahead. Sub-second per clone, memory shared across the fleet — a
host that could never boot 8 cold rooms can hold 8 forks.

This is the differentiated "cool Firecracker" direction (memory `rooms-redirect-away-from-custody`):
the microVM-native magic, and it **repositions the tap-native containment work we already
built** — each clone still gets its own witness pcap, its own egress policy, its own one-shot
vsock secret delivery. Fork is what makes that per-clone plane earn its keep.

**Honest about the moat (non-goal to oversell):** cloud sandboxes are *not* structurally
unable to do this — E2B has pause/resume, CodeSandbox live-clones running microVMs in ~2s via
userfaultfd. The differentiation is **local-first, on your own metal, composed with per-clone
custody** — not the capability in the abstract.

**Non-goals (v0.2):**
- **Bit-deterministic replay** — with LLM sampling, network, and wall-clock in the loop it is
  unachievable. Rescoped to **checkpoint receipts** (§4 D5): re-run from an identical *starting*
  world, not an identical *run*. "Replay" leaves the roadmap vocabulary.
- **UFFD / custom pager** — the File backend's `MAP_PRIVATE` CoW is enough for v0.2; userspace
  paging is real machinery (CodeSandbox-scale) we haven't earned.
- **Diff-snapshot chains** — still Firecracker developer-preview; Full snapshots only.
- **Snapshot migration across FC versions or CPU models / cross-host portability** — snapshots
  are version- and CPU-pinned; a FC upgrade invalidates the library, and that's accepted, not
  fixed.
- **Snapshotting a guest that has already received secrets** — forbidden by the §4 D2 ordering
  invariant, not a feature to be added.

## 2. Functional & non-functional requirements

**FR1.** `rooms snapshot <id>` pauses a running room and writes a Full snapshot (vmstate +
guest-memory file) plus metadata (FC version, rootfs hash, guest IP/slot lineage) into the
room state dir.

**FR2.** `rooms restore <snap>` boots a fresh microVM from a snapshot — no kernel boot — reusing
the same slot/IP the snapshot froze, and reaches SSH-ready.

**FR3.** `rooms clone <snap> -n N` restores N independent clones concurrently, each with a
distinct network identity, distinct entropy, a resynced clock, and its own witness/egress/vsock
plane.

**FR4.** A restore refuses (fails closed) when the snapshot's FC version or rootfs hash does not
match the host — no silent restore of an incompatible image.

**FR5.** The neutral-snapshot invariant is enforced: a snapshot taken after a room received
secrets is refused (or the secrets are provably absent from guest memory). Per-clone secrets are
delivered *after* fork over the existing one-shot vsock.

**FR6.** Each clone produces its own witness pcap and honors its own egress policy — per-clone
custody survives fork.

| NFR | Target |
|---|---|
| Restore latency | restore-to-SSH-ready **< ~1s per clone** (Firecracker load itself is single-digit–tens of ms; the tail is our stage + probe) |
| Fan-out density | 8 clones of a 256 MiB warm room fit on a host that cannot hold 8 cold rooms; `free -m` shows CoW sharing (aggregate RSS ≪ 8×256 MiB) |
| Security | no secret in any snapshot file; no RNG/session-key reuse across clones; each clone a distinct network identity |
| Fail-closed | incompatible snapshot → refuse; post-secret snapshot → refuse; a clone that can't get fresh identity → does not start |
| Blast radius | snapshot/restore is a sibling path; cold boot + non-forked rooms are byte-for-byte unchanged |

## 3. Architecture overview

```
  BASE room (cold, once):
     boot() → clone repo → warm toolchain → agent idle, NO secrets in RAM
        │  rooms snapshot <id>
        ▼
     PAUSE → PUT /snapshot/create (Full) → { vmstate, mem_file } + metadata{fc_ver, rootfs_hash, slot/ip}
        │
        ▼  rooms clone <snap> -n 8
   ┌───────────────── restore() ×8 (sibling to boot(), File backend, MAP_PRIVATE CoW) ─────────────────┐
   │  each clone:  own netns + veth + host NAT (identical inner IP)   ← the frozen slot IP is reused    │
   │               PUT /snapshot/load → Resumed                                                          │
   │               per-clone vsock nudge:  reseed RNG · resync clock · deliver secrets · run identity    │
   │               own witness pcap · own egress chain · re-probe SSH                                    │
   └────────────────────────────────────────────────────────────────────────────────────────────────────┘
        shared warm memory (clean pages), per-clone dirtied pages copy-on-write
```

**Reused (unchanged):** overlay-init RO-rootfs boot (one shared RO base; per-guest writes live
in the overlay *in guest RAM*, so the memory snapshot carries dirty disk state and clones CoW it
— **no per-clone rootfs copy**, `firecracker.rs:1188`); the one-shot vsock secrets channel as the
per-clone identity nudge (`bind_secrets_listener`, `firecracker.rs:557`); the already-attached
virtio entropy device (`firecracker.rs:1292`); per-tap witness/egress attach (`firecracker.rs:485`);
`room::probe` pid+starttime liveness (`room.rs:257`); the slot layer's **reserve-by-index hook,
already written for exactly this** (`slot.rs:80` — its doc comment: *"the reserve-by-index hook
for snapshot restore, which must reclaim the IP frozen into its snapshot"*).

**New, three seams:**
1. **`snapshot` module** — pause → `PUT /snapshot/create` → write snapshot pair + metadata. Policy
   (what to record, the neutral-guest precondition) over the `transport` mechanism.
2. **`restore()` beside `boot()`** — the disjoint FC API flow (`PUT /snapshot/load` before resume,
   no boot-source/drive PUTs), staging the snapshot+mem files into the jail like drives, reclaiming
   the frozen slot via `claim(target: Some(k))`.
3. **netns-per-clone networking** — the long pole: move the slot layer from bare taps to
   netns+veth+NAT so N clones keep identical inner IPs without collision; drags egress-chain
   install + witness tap-naming along.

## 4. Key decisions & trade-offs

**D1 — fork is the bet; snapshot is the mechanism; replay is rescoped. (DECIDED.)** Snapshot alone
buys warm-image reuse + pause/resume (real, low-risk value). Fork buys the differentiated
parallelism payoff. "Deterministic replay" is struck as unachievable (§4 D5). This ordering drives
§9: ship snapshot value first, fork second, receipts as vocabulary.

**D2 — the neutral-snapshot ordering invariant. (DECIDED — load-bearing.)** A Full snapshot's
memory file is **plaintext guest RAM on disk**. If a room received a secret before snapshot, that
secret is in the file and in every clone. So the invariant: **snapshot only a neutral guest (no
secrets delivered), fork, then deliver per-clone secrets over the one-shot vsock post-resume.**
The base room's agent is warmed but *idle* and unauthenticated at snapshot time. `rooms snapshot`
refuses if the room's vsock secrets delivery already fired (the room lifecycle records this).
Rejected: encrypt the snapshot (adds a key-management problem to dodge an ordering rule that's free).

**D3 — netns-per-clone, not in-guest re-IP. (DECIDED.)** Each slot's /30 gives a distinct guest IP
frozen into the kernel cmdline `ip=` (`firecracker.rs:1205`) — a clone believes it is the base's IP.
The upstream `network-for-clones.md` pattern is a **network namespace per clone** (identical inner
`vmtap`/IP, veth to host, iptables NAT), so the guest never changes its IP; the namespace
disambiguates. Rejected: a MMDS/agent-driven in-guest re-IP (needs a guest agent + races the
network coming up). Cost: the slot layer becomes a netns allocator and this **touches egress-chain
install and witness tap naming** — the design's biggest rework and the §9 long pole.

**D4 — `restore()` is a sibling to `boot()`, not a flag. (DECIDED.)** `configure_vm`
(`firecracker.rs:1232`) is a linear PUT sequence ending in `InstanceStart`. Restore is a different
flow: `PUT /snapshot/load` (with the snapshot+mem paths and `mem_backend: File`) *before* any other
config, then `Resumed` — no boot-source, no drive PUTs. Forcing both down one function with a mode
flag muddies both. A `restore()` sharing jail/guard/staging plumbing with `boot()` is cleaner.

**D5 — replay → checkpoint receipts. (DECIDED.)** Not bit-deterministic replay. A **checkpoint
receipt** = `{ snapshot_id, fc_version, rootfs_hash, base_repo_sha, pinned_inputs }` — enough to
*re-run from an identical starting world*. It mostly falls out of the snapshot metadata (FR1) +
the existing artifact discipline; ship it as **vocabulary, not machinery**. The FC-version /
rootfs-hash fields also drive the FR4 compat guard, so this is not extra weight.

**D6 — File backend, not UFFD. (DECIDED for v0.2.)** `MAP_PRIVATE` on the memory file gives
automatic CoW page-sharing across clones through the page cache — enough for the fan-out density
target. UFFD (userspace pager, lazy load, cross-VM dedup) is CodeSandbox-scale machinery; revisit
only if the density target isn't met or snapshots grow too large to `File`-map.

## 5. Data model

- **Snapshot artifact** (per snapshot, under the room state dir, `0700`): `snapshot.vmstate`
  (device/vCPU state), `snapshot.mem` (guest memory — **treat as a credential**, though under D2 it
  holds no secret), and `snapshot.json` metadata: `{ schema_version, snapshot_id, created_at,
  fc_version, rootfs_hash, base_room_id, slot_index, guest_ip, base_repo_sha, secrets_delivered:
  false }`. `secrets_delivered:true` at snapshot time is a refuse (D2).
- **Checkpoint receipt** (D5): the same metadata, surfaced as a first-class artifact line for a run
  — no new store, a projection of `snapshot.json` + the run's pinned inputs.
- **`room.json`** gains an additive v-bump field: `snapshot_lineage` (`{ from_snapshot, base_room }`)
  so a restored room records its origin. `room.json` already versions additively (`room.rs:17`).
- **Slot** (`slot.rs`): no shape change — the `claim(target: Some(k))` reserve-by-index path already
  exists; restore uses it to reclaim the frozen IP. `TargetTaken` (never silent fallback) is the
  collision error.

## 6. API / config contract

**CLI (new verbs):**
```
rooms snapshot <room-id> [--out <dir>]     # pause + Full snapshot + metadata; refuses a non-neutral guest
rooms restore  <snapshot> [--slot <k>]     # one clone, reclaim frozen slot/IP, File backend, re-probe SSH
rooms clone    <snapshot> -n <N>           # N clones: netns-per-clone + per-clone vsock nudge
```

**`snapshot` module (policy over transport):**
```rust
pub fn create(room: &RoomMeta, out: &Path, now: SystemTime) -> Result<SnapshotMeta, SnapshotError>;
// refuses if room.secrets_delivered (D2) or room not Paused-able; records fc_version + rootfs_hash.
```

**`restore()` (sibling to `boot()` in `firecracker`):**
```rust
pub async fn restore(req: RestoreRequest<'_>) -> Result<Guard, FirecrackerError>;
// stages snapshot.vmstate + snapshot.mem into the jail; PUT /snapshot/load {mem_backend:{backend_type:"File"}}
// BEFORE any other config; then PATCH /vm {state:"Resumed"}. No boot-source, no drive PUTs.
```

**FR4 compat guard:** `restore` reads `snapshot.json`, compares `fc_version` (from `firecracker
--version`) and `rootfs_hash` (of the mounted RO base) to the host; mismatch →
`FirecrackerError::SnapshotIncompatible { field }` (fail closed), never a best-effort load.

**Per-clone vsock nudge (post-resume):** reuse the one-shot secrets blob (arbitrary `NAME=value`,
vsock-secrets §5.2) to carry the clone's `{ secrets…, run_id, git_identity, reseed: true, clock:
<host-now> }`; the guest hook reseeds the RNG, steps the clock, and the runner reads secrets +
identity — then the socket is unlinked (first-read-then-delete, unchanged).

## 7. Key flows

**A — snapshot (base room).** Room booted, repo cloned, toolchain warm, agent idle, **no secrets
delivered**. `rooms snapshot`: verify neutral (refuse if `secrets_delivered`), `PATCH /vm
{Paused}`, `PUT /snapshot/create {Full, snapshot_path, mem_file_path}`, write `snapshot.json`
(fc_version, rootfs_hash, slot/ip). Room may then resume or tear down.

**B — restore one clone (increment 2, zero network hard problems).** `rooms restore <snap>
--slot <k>`: `claim(target: Some(k))` reclaims the frozen IP's slot (`TargetTaken` if busy);
compat guard (D-FR4); stage snapshot+mem into the jail; fresh FC process; `PUT /snapshot/load`
(File backend) → `Resumed`; re-probe SSH (`wait_for_ssh`, `runner.rs:104` — pre-snapshot TCP is
dead). Same slot ⇒ no IP collision ⇒ **proves the whole restore path before any netns work.**

**C — fork N clones (increment 3, the payoff).** `rooms clone <snap> -n 8`: for each clone,
allocate a **netns** + veth + host NAT (identical inner IP, no collision), restore as in B into
that netns, then the **per-clone vsock nudge** (reseed RNG per `random-for-clones.md`, resync clock,
deliver per-clone secrets + run identity). Each clone attaches its own witness pcap + egress chain.
Clones share the warm memory CoW; only dirtied pages diverge.

**D — the security ordering (why B/C are safe).** The base snapshot is neutral (D2), so nothing in
`snapshot.mem` is a secret. Secrets enter *only* after fork, *only* over each clone's own vsock,
*only* into that clone's RAM (never a snapshot, never the disk). A snapshot taken post-secret is
refused at flow A.

**E — incompatible restore (fail closed).** Host FC upgraded since the snapshot, or the RO rootfs
changed → compat guard mismatches → `SnapshotIncompatible` → nothing loaded, remedy names re-snapshot.

**F — clone can't get fresh identity.** netns/veth/NAT setup fails, or the vsock reseed nudge isn't
acked → that clone does not reach `workload_started` (reuses the vsock-secrets fail-closed gate) →
no clone runs with duplicated identity.

## 8. Fork hygiene / failure model

The load-bearing matrix — every clone must diverge from its siblings on each axis or it's unsafe:

| Duplicated state | Why it bites | Mitigation (this design) |
|---|---|---|
| MAC / IP / hostname | two clones same address → SSH/network collision | netns-per-clone + host NAT (D3); frozen IP reclaimed per clone via slot target |
| Kernel + userspace RNG | identical CSPRNG stream → repeated TLS nonces/session keys/UUIDs (nonce-reuse breaks AES-GCM) | virtio entropy device (already attached, `firecracker.rs:1292`) + **post-resume in-guest reseed** over vsock (`random-for-clones.md`: VMGenID auto-reseed on kernel ≥5.18; userspace PRNGs reseed explicitly); **snapshot before any TLS/session** (D2 neutral guest already ensures this) |
| Wall clock / kvmclock | resumes stale → token-expiry + TLS-validity math wrong | post-resume clock step in the same vsock nudge |
| Secrets in RAM | baked into `snapshot.mem`, shared by all clones | **D2 ordering invariant**: neutral snapshot; secrets only post-fork over vsock |
| Snapshot files on disk | plaintext memory readable | `0700` room state dir; treat as credential; runbook note |
| FC version pinning | snapshot unloadable after FC upgrade | record `fc_version`; compat guard refuses (FR4); non-goal to migrate |
| Dead TCP / vsock across restore | pre-snapshot connections gone | host re-probes SSH; rebind vsock listener pre-load |
| Agent run identity | same git author / run id across forks | vsock nudge delivers per-clone `run_id` + `git_identity`; agent idle pre-snapshot |

The invariant reviewers should try to break: **no clone reaches `workload_started` sharing another
clone's network identity, RNG stream, or a secret from the base snapshot.**

## 9. Rollout / implementation plan

| Phase | Goal | High-level tasks | Depends on | Gate |
|---|---|---|---|---|
| **1. snapshot-restore** (Fable increments 1+2) | snapshot a room and restore it to a working single room — warm-image reuse, zero network hard problems | (1a) `snapshot` module + `rooms snapshot`: pause → Full `/snapshot/create` → `snapshot.json` (fc_version, rootfs_hash, slot/ip), refuse a non-neutral guest [opus]; (1b) `restore()` sibling + `rooms restore`: `/snapshot/load` File backend, reclaim frozen slot via `claim(target)`, FR4 compat guard, re-probe SSH [opus] | — | **intermediate gate:** a snapshot restores to an SSH-ready room, liveness intact, compat guard refuses a mismatched host |
| **2. fork-clones** (Fable increment 3) | N clones from one snapshot, each a distinct identity — the differentiated payoff | netns-per-clone allocator (rework slot layer + `create_slot_tap`, `firecracker.rs:609`) + veth/NAT (`network-for-clones.md`); per-clone vsock post-resume nudge (reseed RNG, resync clock, deliver secrets + run identity); per-clone witness/egress attach; `rooms clone -n N` | phase 1 | **VALIDATION GATE** (killer demo) below |
| **3. checkpoint-receipts + hardening** (stub) | replay-rescope vocabulary + fleet ergonomics | checkpoint receipt as a first-class artifact (projection of `snapshot.json` + pinned inputs, D5); snapshot GC / retention; FC-upgrade library-invalidation ergonomics; UFFD only if density target missed | phase 2 + gate | each item needs a demonstrated need first |

Rough scope: phase 1 is two PR-sized tasks (new FC API surface, but bounded — no networking rework).
Phase 2 is the long pole (netns touches slot/egress/witness) — a stub here, tasks materialized when
phase 1 lands. Phase 3 is deliberately unsized.

**VALIDATION GATE (after phase 2) — the killer demo:** on the rooms-host, boot one room, clone a
repo + warm the `claude` toolchain, `rooms snapshot`; then `rooms clone <snap> -n 8` and:
- (a) 8 clones reach SSH-ready in **under a second total**;
- (b) `free -m` shows aggregate RSS **≪ 8 × 256 MiB** (CoW memory sharing proven);
- (c) 8 real `/work-driver` tasks run in parallel on a host that could not hold 8 cold rooms;
- (d) **8 distinct witness pcaps** — per-clone custody survives fork;
- (e) no two clones share a MAC/IP, and an RNG-draw probe (e.g. `head -c16 /dev/urandom | xxd` per
  clone) shows **distinct** output — the hygiene matrix holds.
Phase 3 is not committed until this passes.

## 10. Open questions

1. **Neutral-guest detection.** D2 refuses a snapshot if `secrets_delivered`. Is the room lifecycle
   flag sufficient, or do we also want a memory-scan / a hard rule that the base room is booted with
   `--no-secrets`? Lean: lifecycle flag + a `--neutral` assertion on `rooms snapshot`.
2. **netns vs `network_overrides`.** `/snapshot/load` accepts a `network_overrides` param to remap
   guest NICs to differently-named host taps — could that avoid full netns-per-clone for the
   same-IP problem, or does the frozen guest IP still collide at the host routing layer? Resolve in
   the phase-2 spike before committing the slot rework.
3. **Does the frozen slot/IP model even survive fork?** Increment 2 reuses the *same* slot (one
   restore). Increment 3 needs N *different* host-side identities for the *same* guest IP — netns is
   the answer, but confirm the slot layer's `/30` accounting doesn't need to change shape (vs just
   move into a namespace).
4. **Snapshot size vs `File`-map cost.** A warm agent room's memory file could be a few hundred MiB;
   at N=8 the page-cache sharing is the win, but confirm the mem-file write on `snapshot create`
   isn't itself a latency cliff. Measure in phase 1.
5. **How warm is the base?** Snapshot after repo-clone + toolchain-warm but before the task prompt.
   Where exactly the "neutral warm" line sits (which caches primed, agent process started-but-idle)
   is a phase-1 tuning question that sets the whole payoff size.

## 11. Validation plan

The §9 gate is the plan. Its signal is binary and baseline-free: run the killer demo and check the
five conditions (8 clones < 1s, `free -m` proves CoW sharing, 8 parallel real tasks, 8 distinct
witness pcaps, distinct RNG draws + no MAC/IP collision). If (b) fails (RSS ≈ 8×256 MiB) the CoW
sharing isn't happening and the density thesis is unproven; if (e) fails (shared RNG or identity)
the hygiene matrix is broken and the fork is unsafe regardless of speed. Phase 1 has its own cheaper
intermediate gate — a single restore reaching SSH-ready with the compat guard firing — so snapshot
value is proven before the netns long pole starts.
