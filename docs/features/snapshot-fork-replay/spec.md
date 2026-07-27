# snapshot / fork / replay — Technical Design Document

**Status:** draft / proposal — NOT a build commitment. The artifact we decide from.
**Owner:** @itsHabib
**Date:** 2026-07-26
**Related:** [`docs/vision.md`](../../vision.md) (v0.2 roadmap line) · [`docs/features/vsock-secrets/spec.md`](../vsock-secrets/spec.md) (the per-clone identity channel this extends) · [`docs/features/host-witness/spec.md`](../host-witness/spec.md) + egress work (the per-clone observability this composes with) · memory `rooms-redirect-away-from-custody` (why this is the bet) · Firecracker `docs/snapshotting/{snapshot-support,random-for-clones,network-for-clones,versioning}.md`

> **v2 (2026-07-27)** — revised after design review. codex flagged five load-bearing holes (4×P1, 1×P2); all folded in. The shape changed in two places: **(D2)** neutrality is now enforced *by construction* via a sealed neutral-base boot mode + authoritative monotonic provenance state, not an observed `secrets_delivered` flag; **(D7)** the post-resume nudge now has a real guest receiver (the base carries `/vsock` into the snapshot + a resume-apply agent), and **restore-time hygiene — reseed, clock, identity — moved into phase 1** (single restore reused >once already duplicates RNG/clock, so it can't wait for the fork phase). See the §12 changelog for the finding-by-finding map.

> **Reviewers — focus areas:**
> - **§4 D2 + §7 A** — neutrality *by construction*: the sealed neutral-base boot mode + monotonic `provenance` state. This is the load-bearing security rule; if a base can be tainted after the neutrality check, the design leaks secrets into `snapshot.mem`.
> - **§4 D7 + §7 B** — the post-resume receiver + the ack gate. A neutral base boots without `--secret`; the guest still needs a live consumer for the reseed/clock/identity nudge *on resume*, or every clone fails closed. The resume-trigger mechanism is the one genuinely open sub-decision (§10 Q2).
> - **§4 D3 + §9** — netns-per-clone is the long pole (drags egress-chain install + witness tap-naming). Phase 1 now ships snapshot + restore **with hygiene**; phase 2 adds only netns fan-out.
> - **§8** — the fork hygiene matrix, especially the **userspace-PRNG** row: kernel entropy does not reseed an already-started process, so the neutral base is snapshotted **before the agent process starts**.

## 1. Problem & hypothesis

`/work-driver` fans out N coding-agent tasks, each into its own room. Today every room pays
the **full cold cost**: boot the microVM (~150–300ms), transport + clone the repo, warm the
agent toolchain (install/JIT the `claude` or cursor runtime, prime caches) — seconds to
minutes, paid N times, and the host can only hold so many rooms built from scratch at once.

The cold *boot* is not the expensive part; the **repo transfer + toolchain warm-up** is. And
that warm state is identical across a fan-out batch until the task-specific work begins.

**The bet:** Firecracker's snapshot/restore lets us pay the warm-up **once** and **fork** it.
Create one **sealed neutral base** — repo cloned, toolchain warm, no secrets, agent process not
yet started — snapshot the paused guest, then restore that snapshot into **N clones** that share
the warm memory copy-on-write and each start seconds-to-minutes of setup ahead. Sub-second per
clone, memory shared across the fleet — a host that could never boot 8 cold rooms can hold 8
forks.

This is the differentiated "cool Firecracker" direction (memory `rooms-redirect-away-from-custody`):
the microVM-native magic, and it **repositions the tap-native containment work we already
built** — each clone still gets its own witness pcap, its own egress policy, and its own
per-clone secret + identity delivery over vsock. Fork is what makes that per-clone plane earn
its keep.

**Honest about the moat (non-goal to oversell):** cloud sandboxes are *not* structurally
unable to do this — E2B has pause/resume, CodeSandbox live-clones running microVMs in ~2s via
userfaultfd. The differentiation is **local-first, on your own metal, composed with per-clone
custody** — not the capability in the abstract.

**Non-goals (v0.2):**
- **Bit-deterministic replay** — with LLM sampling, network, and wall-clock in the loop it is
  unachievable. Rescoped to **checkpoint receipts** (§4 D5): re-run from an identical *starting*
  world, not an identical *run*. "Replay" leaves the roadmap vocabulary.
- **UFFD / custom pager** — the File backend's `MAP_PRIVATE` CoW is enough for v0.2.
- **Diff-snapshot chains** — still Firecracker developer-preview; Full snapshots only.
- **Snapshot migration across FC versions or CPU models / cross-host portability** — snapshots
  are version- and CPU-pinned; a FC upgrade invalidates the library, accepted not fixed.
- **Snapshotting a non-neutral guest** — forbidden by construction (§4 D2), not a future feature.
- **Snapshotting an already-started agent process** — the base is sealed *before* the agent
  runs, so no live userspace-PRNG / session state is captured (§4 D7, §8).

## 2. Functional & non-functional requirements

**FR1.** `rooms base-create` produces a **sealed neutral base**: repo cloned + toolchain warm,
**no secret channel armed, no interactive/agent ingress**, agent process not started; the room's
authoritative `provenance` is `neutral`.

**FR2.** `rooms snapshot <base>` pauses the neutral base and writes a Full snapshot (vmstate +
guest-memory file, which includes the `/vsock` device) plus metadata; it **refuses** any room
whose `provenance` is not `neutral`.

**FR3.** `rooms restore <snap>` boots a fresh microVM from a snapshot — no kernel boot — reaching
SSH-ready, and applies **restore-time hygiene** (reseed RNG, resync clock, deliver identity + any
secrets) over vsock, gated: no ack → no workload.

**FR4.** `rooms clone <snap> -n N` restores N clones concurrently, each in its own network
namespace (distinct host-side identity, isolated from siblings), each with fresh entropy, a
resynced clock, its own secrets/identity, and its own witness/egress plane.

**FR5.** A restore refuses (fails closed) when the snapshot's FC version or rootfs hash does not
match the host.

**FR6.** No secret is ever present in a snapshot file; secrets reach a guest **only** post-resume,
per-clone, over vsock. The agent process starts **after** restore hygiene, so its userspace PRNG
seeds fresh.

**FR7.** Each clone produces its own witness pcap and honors its own egress policy — per-clone
custody survives fork.

| NFR | Target |
|---|---|
| Restore latency | restore-to-workload-ready **< ~1s per clone** (FC load is single-digit–tens of ms; the tail is our stage + hygiene-ack + probe) |
| Fan-out density | 8 clones of a 256 MiB warm base fit on a host that cannot hold 8 cold rooms; `free -m` shows CoW sharing (aggregate RSS ≪ 8×256 MiB) |
| Security | no secret in any snapshot file; no RNG/session-key reuse across clones (kernel **and** userspace); each clone isolated in its own netns |
| Fail-closed | incompatible snapshot → refuse; non-neutral base → refuse to snapshot; a clone whose hygiene nudge isn't acked → no workload |
| Blast radius | snapshot/restore is a sibling path; cold boot + non-forked rooms are byte-for-byte unchanged |

## 3. Architecture overview

```
  rooms base-create  →  SEALED NEUTRAL BASE (once):
     boot (no --secret, /vsock present, resume-apply agent waiting; NO agent process, NO interactive ingress)
       → clone repo → warm toolchain → idle.  provenance = neutral (authoritative, monotonic)
        │  rooms snapshot <base>   (refuses if provenance != neutral)
        ▼
     PAUSE → PUT /snapshot/create (Full) → { vmstate(+/vsock), mem_file } + metadata{fc_ver, rootfs_hash, slot/ip}
        │
        ▼  rooms clone <snap> -n 8      (phase 2 adds the netns fan-out around phase-1 restore)
   ┌──────────── restore() ×8 (sibling to boot(), File backend, MAP_PRIVATE CoW) ────────────┐
   │  each clone: own netns + veth + host NAT (identical inner IP reused, disambiguated by ns) │
   │              PUT /snapshot/load → Resumed                                                 │
   │              resume-apply agent (already waiting in the snapshot) connects the per-clone   │
   │              vsock listener → applies: reseed RNG · resync clock · secrets · run identity  │
   │              → ACKs.  Host gates: no ack ⇒ no workload_started.                            │
   │              THEN the agent process starts (fresh userspace PRNG) · own witness · own egress│
   └───────────────────────────────────────────────────────────────────────────────────────────┘
        shared warm memory (clean pages), per-clone dirtied pages copy-on-write
```

**Reused (unchanged):** overlay-init RO-rootfs boot (one shared RO base; per-guest writes in the
overlay live *in guest RAM*, so the memory snapshot carries dirty disk state and clones CoW it —
**no per-clone rootfs copy**, `firecracker.rs:1188`); the already-attached virtio entropy device
(`firecracker.rs:1292`); per-tap witness/egress attach (`firecracker.rs:485`); `room::probe`
pid+starttime liveness (`room.rs:257`); the slot layer's **reserve-by-index hook, already written
for exactly this** (`slot.rs:80` — *"the reserve-by-index hook for snapshot restore, which must
reclaim the IP frozen into its snapshot"*).

**Extended (not just reused):** the vsock secrets channel. Today it is a **boot-time one-shot**
guest→host fetch that exits before a snapshot could be taken (`scripts/lib/rooms-secrets-fetch.sh`),
and a neutral base booted without `--secret` gets no `/vsock` device at all (`firecracker.rs:539`,
`1275-1290`). Both must change (D7): the neutral base **always** attaches `/vsock`, and the
boot-time one-shot becomes a **long-lived resume-apply agent** captured *in* the snapshot, waiting,
so every clone has a live consumer on resume.

**New, four seams:**
1. **sealed neutral-base mode** (`base-create`) — boots with no secret channel and no interactive
   ingress; records authoritative monotonic `provenance` (D2).
2. **`snapshot` module** — pause → `PUT /snapshot/create` → snapshot pair + metadata; refuses a
   non-neutral base.
3. **`restore()` beside `boot()`** — the disjoint FC API flow (`PUT /snapshot/load` before resume),
   staging snapshot+mem into the jail, reclaiming the frozen slot, **applying restore hygiene +
   ack gate** (D7).
4. **netns-per-clone networking** (phase 2, the long pole) — move the slot layer from bare taps to
   netns+veth+NAT so N clones keep identical inner IPs without collision; drags egress-chain
   install + witness tap-naming along.

## 4. Key decisions & trade-offs

**D1 — fork is the bet; snapshot is the mechanism; replay is rescoped. (DECIDED.)** Snapshot alone
buys warm-base reuse + pause/resume. Fork buys the differentiated parallelism payoff.
"Deterministic replay" is struck as unachievable (D5). This drives §9: snapshot+restore value first
(phase 1), netns fan-out second (phase 2), receipts as vocabulary (phase 3).

**D2 — neutrality by construction, not by observation. (DECIDED — load-bearing; revised in v2.)**
A Full snapshot's memory file is **plaintext guest RAM on disk**; a secret in RAM at snapshot time
is in the file and in every clone. v1 tried to enforce this by *observing* a `secrets_delivered`
lifecycle event — but `RoomMeta` has no such field, lifecycle output is non-authoritative, and a
`--keep` room can be tainted over SSH or vsock *after* the check (codex P1). So v2 makes neutrality
a property of **how the base is created**, recorded as durable authoritative state:
- `rooms base-create` boots a **sealed** base: the vsock **secrets** payload is never armed, no
  agent workload runs, and interactive/`exec` ingress is refused. The only mutations are the
  warm-up steps rooms itself drives (repo clone, toolchain warm).
- `RoomMeta` gains a monotonic `provenance: neutral | tainted` field (authoritative, persisted).
  It starts `neutral` only for a `base-create` room and flips to `tainted` **irreversibly** the
  instant anything that could introduce unique/secret state occurs (secret armed, agent/workload
  started, interactive session opened). There is no path back to `neutral`.
- `rooms snapshot` refuses unless `provenance == neutral`. Neutrality is thus *unforgeable by
  a later action*, not a best-effort assertion.
Rejected: encrypt the snapshot (adds key management to dodge an ordering rule that construction
makes free); trust a `--neutral` CLI flag (an assertion, not an enforcement).

**D3 — netns-per-clone, not in-guest re-IP. (DECIDED.)** Each slot's /30 gives a distinct guest IP
frozen into the kernel cmdline `ip=` (`firecracker.rs:1205`); a clone believes it is the base's IP.
The upstream `network-for-clones.md` pattern is a **network namespace per clone** (identical inner
`vmtap`/IP + MAC, veth to host, iptables NAT), so the guest never changes its address; the namespace
disambiguates and isolates. Rejected: MMDS/agent-driven in-guest re-IP (needs a guest agent + races
the network). Cost: the slot layer becomes a netns allocator and this **touches egress-chain install
and witness tap naming** — the design's biggest rework and the §9 long pole.

**D4 — `restore()` is a sibling to `boot()`, not a flag. (DECIDED.)** `configure_vm`
(`firecracker.rs:1232`) is a linear PUT sequence ending in `InstanceStart`. Restore is a different
flow: `PUT /snapshot/load` (snapshot+mem paths, `mem_backend: File`) *before* any other config, then
`Resumed` — no boot-source, no drive PUTs. A `restore()` sharing jail/guard/staging plumbing with
`boot()` is cleaner than a mode flag through one function.

**D5 — replay → checkpoint receipts. (DECIDED.)** Not bit-deterministic replay. A **checkpoint
receipt** = `{ snapshot_id, fc_version, rootfs_hash, base_repo_sha, pinned_inputs }` — enough to
*re-run from an identical starting world*. It falls out of the snapshot metadata (FR2) + the existing
artifact discipline; ship it as **vocabulary, not machinery**. The FC-version / rootfs-hash fields
also drive the FR5 compat guard, so it is not extra weight.

**D6 — File backend, not UFFD. (DECIDED for v0.2.)** `MAP_PRIVATE` on the memory file gives
automatic CoW page-sharing across clones through the page cache — enough for the fan-out density
target. UFFD (userspace pager, lazy load, cross-VM dedup) is CodeSandbox-scale machinery; revisit
only if the density target isn't met or snapshots grow too large to `File`-map.

**D7 — a post-resume receiver + restore-time hygiene, in phase 1. (DECIDED — new in v2.)** v1 assumed
the vsock secrets channel would carry the per-clone nudge, but that channel is a **boot-time one-shot
that has already exited before the snapshot**, and a neutral base has no `/vsock` device at all
(codex P1). And v1 deferred all hygiene (reseed/clock/identity) to phase 2 — wrong, because
**restoring one snapshot more than once (phase-1 warm-base reuse) already resumes the same clock and
RNG state** (codex P1). So v2:
- The neutral base **always attaches `/vsock`** (even with no secret), so the device is in the
  snapshot and every clone has it.
- The boot-time one-shot fetch becomes a **long-lived resume-apply agent** that is running and
  *waiting on the channel* at snapshot time, so it is captured in the snapshot and is live on every
  resume. On resume it connects the per-clone host listener and applies the nudge:
  `{ reseed, clock: <host-now>, secrets…, run_id, git_identity }`, then ACKs.
- **Restore-time hygiene moves to phase 1.** *Every* restore — even a single one — reseeds the RNG
  (kernel via VMGenID/virtio-rng per `random-for-clones.md`, plus the userspace guarantee below),
  steps the clock, and sets identity, behind the ack gate (reuses the vsock-secrets host sequencing:
  no ack ⇒ no `workload_started`). Phase 2 adds only the netns fan-out around this.
- **Userspace PRNG** (codex P1): kernel reseed does not touch an already-started process's PRNG. The
  guarantee is structural — the neutral base is snapshotted **before the agent process starts**
  (D2 sealed base), so no long-lived userspace-PRNG state exists to duplicate; the agent starts
  *after* restore hygiene and seeds fresh from the reseeded kernel. If a runtime must be pre-started
  in a future variant, it owns a named reseed step and the §9 gate validates *its* PRNG, not just
  `/dev/urandom`.
The one genuinely open sub-decision is **how the resume-apply agent knows to re-connect on resume**
(poll-retry loop captured mid-wait, vs a resume signal, vs host-driven SSH trigger) — §10 Q2.

## 5. Data model

- **`RoomMeta.provenance`** (new, D2): `neutral | tainted`, monotonic, authoritative, persisted.
  `neutral` only for a `base-create` room; flips to `tainted` irreversibly on any secret-arm /
  workload-start / interactive session. `rooms snapshot` reads *this*, never a lifecycle event.
- **Snapshot artifact** (per snapshot, room state dir `0700`): `snapshot.vmstate` (device/vCPU
  state, **includes the `/vsock` device**), `snapshot.mem` (guest memory — treat as a credential,
  though under D2 it holds no secret), `snapshot.json`: `{ schema_version, snapshot_id, created_at,
  fc_version, rootfs_hash, base_room_id, slot_index, guest_ip, base_repo_sha, provenance:"neutral" }`.
- **Checkpoint receipt** (D5): the same metadata surfaced as a first-class artifact line for a run —
  no new store, a projection of `snapshot.json` + the run's pinned inputs.
- **`room.json`** gains additive fields: `provenance` (above) and `snapshot_lineage`
  (`{ from_snapshot, base_room }`) on a restored room. `room.json` already versions additively
  (`room.rs:17`).
- **Slot** (`slot.rs`): no shape change — `claim(target: Some(k))` reclaims the frozen IP;
  `TargetTaken` (never silent fallback) is the collision error. Under D3 the *host-side* identity
  moves into a netns; the guest-visible /30 is unchanged.

## 6. API / config contract

**CLI (new verbs):**
```
rooms base-create --repo <r> [--warm <cmd>]   # sealed neutral base: no secrets, no agent, /vsock present; provenance=neutral
rooms snapshot    <base-id>                    # pause + Full snapshot + metadata; REFUSES provenance != neutral
rooms restore     <snapshot> [--slot <k>]      # one clone, reclaim frozen slot/IP, File backend, restore hygiene + ack, re-probe SSH
rooms clone       <snapshot> -n <N>            # N clones: netns-per-clone around restore() (phase 2)
```

**`snapshot` module (policy over transport):**
```rust
pub fn create(base: &RoomMeta, out: &Path, now: SystemTime) -> Result<SnapshotMeta, SnapshotError>;
// refuses if base.provenance != Neutral (D2); records fc_version + rootfs_hash; snapshot includes /vsock.
```

**`restore()` (sibling to `boot()`):**
```rust
pub async fn restore(req: RestoreRequest<'_>) -> Result<Guard, FirecrackerError>;
// stage snapshot.vmstate + snapshot.mem into the jail; PUT /snapshot/load {mem_backend:{backend_type:"File"}}
// BEFORE any other config; PATCH /vm {Resumed}; then run the restore-hygiene nudge and AWAIT its ack
// (no ack ⇒ error, no workload). D7.
```

**FR5 compat guard:** `restore` reads `snapshot.json`, compares `fc_version` (`firecracker
--version`) and `rootfs_hash` (of the mounted RO base) to the host; mismatch →
`FirecrackerError::SnapshotIncompatible { field }` (fail closed), never a best-effort load.

**Restore-hygiene nudge (post-resume, per clone):** the resume-apply agent (captured in the snapshot,
waiting) connects the per-clone vsock listener and receives `{ reseed:true, clock:<host-now>,
secrets:{…}, run_id, git_identity }`; it reseeds the kernel RNG, steps the clock, stages secrets +
identity, ACKs. Host blocks `workload_started` until the ack (reuses vsock-secrets §5.4 sequencing).
The agent process starts only after this.

## 7. Key flows

**A — create a sealed neutral base (D2).** `rooms base-create --repo r`: `boot()` a room with the
secrets payload **unarmed** but `/vsock` **present** (for the resume-apply agent) and interactive/
`exec` ingress **refused**; rooms drives repo-clone + toolchain-warm; the agent process is NOT
started. `provenance = neutral` (authoritative). Any attempt to arm a secret, start the agent, or
open an interactive session flips `provenance = tainted` irreversibly.

**B — snapshot + restore one clone WITH hygiene (phase 1).** `rooms snapshot <base>`: refuse if
`provenance != neutral`; `PATCH /vm {Paused}`; `PUT /snapshot/create {Full}`; write `snapshot.json`.
`rooms restore <snap> --slot k`: `claim(target: Some(k))` reclaims the frozen IP's slot
(`TargetTaken` if busy); compat guard (FR5); stage snapshot+mem; fresh FC process; `PUT
/snapshot/load` (File) → `Resumed`; the waiting resume-apply agent applies the hygiene nudge (reseed,
clock, identity) and ACKs — **no ack ⇒ no workload**; re-probe SSH (`wait_for_ssh`, `runner.rs:104`,
pre-snapshot TCP is dead); the agent process starts fresh. Even a *single* restore reseeds — a
warm-base reused twice must not repeat RNG/clock.

**C — fork N clones (phase 2, the payoff).** `rooms clone <snap> -n 8`: for each clone allocate a
**netns** + veth + host NAT (identical inner IP, isolated), restore as in B into that netns with its
own hygiene nudge (per-clone secrets + `run_id`), attach its own witness pcap + egress chain. Clones
share warm memory CoW; only dirtied pages diverge.

**D — the security ordering (why B/C are safe).** The base is neutral **by construction** (D2), so
nothing in `snapshot.mem` is a secret and no live userspace-PRNG/session state is captured. Secrets
+ identity enter *only* post-resume, *only* per clone over its own vsock, *only* into that clone's
RAM. A non-neutral base is refused at flow B.

**E — incompatible restore (fail closed).** Host FC upgraded, or RO rootfs changed → compat guard
mismatch → `SnapshotIncompatible` → nothing loaded, remedy names re-snapshot.

**F — clone can't get fresh identity (fail closed).** netns/veth/NAT setup fails, or the hygiene
nudge isn't acked → that clone never reaches `workload_started` (reuses the vsock-secrets gate) → no
clone runs with duplicated identity, stale clock, or unreseeded RNG.

## 8. Fork hygiene / failure model

Every clone must diverge from its siblings on each axis or it's unsafe:

| Duplicated state | Why it bites | Mitigation (v2) |
|---|---|---|
| MAC / IP / hostname | two clones same address → collision | **netns-per-clone + host NAT (D3)** — inner MAC/IP intentionally identical, disambiguated + isolated by namespace; frozen IP reclaimed per clone via slot target |
| Kernel RNG | identical CSPRNG stream → repeated TLS nonces/keys/UUIDs | virtio entropy device (`firecracker.rs:1292`) + **post-resume kernel reseed** in the hygiene nudge (`random-for-clones.md`: VMGenID auto-reseed on kernel ≥5.18) — applied on **every** restore (D7) |
| **Userspace PRNG** | kernel reseed does **not** touch an already-started process's PRNG (OpenSSL pools, session keys) | **structural: snapshot the base *before* the agent process starts (D2 sealed base)** — no userspace-PRNG state to duplicate; agent seeds fresh post-hygiene. Future pre-started runtimes own a named reseed + app-PRNG validation |
| Wall clock / kvmclock | resumes stale → token-expiry + TLS-validity wrong | post-resume clock step in the same hygiene nudge, on every restore |
| Secrets in RAM | baked into `snapshot.mem`, shared by all clones | **D2 neutrality by construction**: no secret ever in the base; secrets only post-fork over vsock |
| Snapshot files on disk | plaintext memory readable | `0700` room state dir; treat as credential; runbook note |
| FC version pinning | snapshot unloadable after FC upgrade | record `fc_version`; compat guard refuses (FR5); non-goal to migrate |
| Dead TCP / vsock across restore | pre-snapshot connections gone | host re-probes SSH; the resume-apply agent (re)connects the per-clone vsock listener on resume (D7) |
| Agent run identity | same git author / run id across forks | hygiene nudge delivers per-clone `run_id` + `git_identity`; agent starts idle-then-fresh post-resume |

The invariant reviewers should try to break: **no clone reaches `workload_started` sharing another
clone's network identity, kernel or userspace RNG stream, stale clock, or a secret from the base
snapshot.**

## 9. Rollout / implementation plan

| Phase | Goal | High-level tasks | Depends on | Gate |
|---|---|---|---|---|
| **1. snapshot-restore** | a sealed neutral base snapshots and restores to a single working room **with full hygiene** — warm-base reuse, safe under repeat restore | (1a) sealed neutral-base mode + `provenance` state + `rooms base-create` (no secret channel, no agent start, `/vsock` present, refuse interactive ingress) [opus]; (1b) `snapshot` module + `rooms snapshot`: pause → Full create → `snapshot.json`, refuse non-neutral [opus]; (1c) `restore()` sibling + `rooms restore`: `/snapshot/load` File backend, reclaim frozen slot, FR5 compat guard, **resume-apply agent + restore-hygiene nudge (reseed/clock/identity) + ack gate**, re-probe SSH [opus] | Firecracker snapshot GA | **intermediate gate** below |
| **2. fork-clones** | N clones from one snapshot, each isolated — the differentiated payoff | netns-per-clone allocator (rework slot layer + `create_slot_tap`, `firecracker.rs:609`) + veth/NAT (`network-for-clones.md`); per-clone hygiene nudge + witness/egress attach; `rooms clone -n N` | phase 1 | **VALIDATION GATE** (killer demo) below |
| **3. checkpoint-receipts + hardening** (stub) | replay-rescope vocabulary + fleet ergonomics | checkpoint receipt as a first-class artifact (D5); snapshot GC/retention; FC-upgrade library-invalidation ergonomics; UFFD only if density missed | phase 2 + gate | each item needs a demonstrated need first |

Rough scope: phase 1 is three PR-sized tasks (the neutral-base mode + the resume-apply receiver are
new surface, but bounded — no networking rework). Phase 2 is the long pole (netns touches
slot/egress/witness) — a stub here, tasks materialized when phase 1 lands. Phase 3 is deliberately
unsized.

**Intermediate gate (after phase 1):** on the rooms-host, `base-create` a neutral base, `snapshot`
it, `restore` it **twice** and confirm: (a) both restores reach workload-ready; (b) the compat guard
refuses a forced fc_version/rootfs mismatch; (c) **the two restores show distinct kernel RNG draws
and a correct (resynced) clock** — proving hygiene fires on every restore, not just fork; (d)
`snapshot` refuses a non-neutral room.

**VALIDATION GATE (after phase 2) — the killer demo:** boot one base, clone a repo + warm the
`claude` toolchain, `rooms snapshot`; then `rooms clone <snap> -n 8` and:
- (a) 8 clones reach workload-ready in **under a second total**;
- (b) `free -m` shows aggregate RSS **≪ 8 × 256 MiB** (CoW sharing);
- (c) 8 real `/work-driver` tasks run in parallel on a host that could not hold 8 cold rooms;
- (d) **8 distinct witness pcaps** — per-clone custody survives fork;
- (e) **each clone is a distinct netns/veth/NAT identity with verified cross-clone isolation** (clone
  A cannot reach clone B), and an **application-level** RNG-draw probe (not just `/dev/urandom`) shows
  distinct output per clone — the hygiene matrix holds.
Phase 3 is not committed until this passes.

## 10. Open questions

1. **Sealing the base (D2).** Is `base-create` best implemented as a distinct boot mode (no secret
   device armed + `exec`/interactive refused), or as the *default* boot with a `--seal` that trips
   `provenance=tainted` on first secret/workload/exec? Lean: distinct mode, so "neutral" is the
   explicit, auditable path.
2. **Resume-trigger for the apply agent (D7) — the one real unknown.** How does the captured
   resume-apply agent re-connect on resume? (a) a poll-retry loop captured mid-wait (simple, robust,
   the host serves nothing during the neutral phase so it just retries); (b) a resume signal
   (VMGenID/virtio) it waits on; (c) host-driven — after `Resumed`, host `ssh`-triggers the apply.
   Resolve with a phase-1 spike; (a) is the current favorite.
3. **netns vs `network_overrides`.** `/snapshot/load` accepts `network_overrides` to remap guest NICs
   to differently-named host taps — could that avoid full netns-per-clone, or does the frozen guest
   IP still collide at host routing? Resolve in the phase-2 spike before the slot rework.
4. **Snapshot size vs `File`-map cost.** A warm base's memory file could be a few hundred MiB; at N=8
   the page-cache sharing is the win, but confirm the mem-file write on `snapshot create` isn't a
   latency cliff. Measure in phase 1.
5. **How warm is the neutral base?** Snapshot after repo-clone + toolchain-warm but **before** the
   agent process starts (D2/D7). Which caches can be primed without starting a long-lived
   PRNG-holding process is the phase-1 tuning question that sets the payoff size.

## 11. Validation plan

The §9 gates are the plan; both signals are binary and baseline-free. **Phase 1 (cheap):** a neutral
base restores twice to workload-ready, the compat guard fires on a forced mismatch, `snapshot`
refuses a non-neutral room, and the two restores show distinct kernel-RNG draws + a correct clock —
proving hygiene fires on every restore. **Phase 2 (the payoff):** the killer demo — 8 clones < 1s,
`free -m` proves CoW sharing, 8 parallel real tasks, 8 distinct witness pcaps, verified netns
isolation + distinct **application-level** RNG draws. If (b) fails (RSS ≈ 8×256 MiB) the density
thesis is unproven; if the isolation/RNG check fails the fork is unsafe regardless of speed.

## 12. Changelog

**v2 (2026-07-27) — design-review pass (codex, 4×P1 + 1×P2):**
- **P1 "persist neutrality as authoritative state"** → **D2 rewritten**: neutrality by construction
  (sealed `base-create` mode + monotonic authoritative `provenance`), not an observed
  `secrets_delivered` event. §5, §7 A.
- **P1 "add a post-resume receiver for the vsock nudge"** → **D7 (new)**: the neutral base carries
  `/vsock` into the snapshot and the boot-time one-shot becomes a long-lived resume-apply agent
  captured mid-wait; §3 "Extended", §6, §10 Q2 (the resume-trigger spike).
- **P1 "move restore hygiene into phase 1"** → **D7 + §9**: reseed/clock/identity + ack gate now in
  phase 1 (single restore reused >once already duplicates RNG/clock); phase 2 is netns fan-out only.
  New phase-1 intermediate gate checks RNG/clock on a double-restore.
- **P1 "reset cloned userspace PRNG"** → **§8 userspace-PRNG row + D7**: structural fix — snapshot
  *before* the agent process starts; §9 gate probes the **application** PRNG, not just `/dev/urandom`.
- **P2 "align identity validation with the netns model"** → **§9 gate rewritten**: verify distinct
  netns/veth/NAT identity + cross-clone isolation (inner MAC/IP are intentionally identical under
  D3), replacing the impossible "no two clones share a MAC/IP" check.
