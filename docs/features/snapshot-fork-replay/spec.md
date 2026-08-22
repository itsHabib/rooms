# snapshot / fork / replay — Technical Design Document

**Status:** Phase 2 implemented; exact-head killer, review, and Gate remain before landing.
**Owner:** @itsHabib
**Date:** 2026-07-26
**Related:** [`docs/vision.md`](../../vision.md) (v0.2 roadmap line) · [`docs/features/vsock-secrets/spec.md`](../vsock-secrets/spec.md) (the per-clone identity channel this extends) · [`docs/features/host-witness/spec.md`](../host-witness/spec.md) + egress work (the per-clone observability this composes with) · memory `rooms-redirect-away-from-custody` (why this is the bet) · Firecracker `docs/snapshotting/{snapshot-support,random-for-clones,network-for-clones,versioning}.md`

> **v2 (2026-07-27)** — revised after design review. codex flagged five load-bearing holes (4×P1, 1×P2); all folded in. The shape changed in two places: **(D2)** neutrality is now enforced *by construction* via a sealed neutral-base boot mode + authoritative monotonic provenance state, not an observed `secrets_delivered` flag; **(D7)** the post-resume nudge now has a real guest receiver (the base carries `/vsock` into the snapshot + a resume-apply agent), and **restore-time hygiene — reseed, clock, identity — moved into phase 1** (single restore reused >once already duplicates RNG/clock, so it can't wait for the fork phase). See the §12 changelog for the finding-by-finding map.

> **v3 (2026-07-27)** — second review pass. codex found four deeper holes (3×P1, 1×P2), all converging on one fact: **the base still ran live crypto daemons (`sshd`, the resume-apply agent) at snapshot time.** This revision introduced quiesce-before-seal and originally used SSH for warm-up; the v5 execution reconciliation below supersedes that transport with a credential-free guest-agent channel so `sshd` never starts before the snapshot. Plus: the base became an explicit consumed template, and the density gate measures **PSS**, not `free`. See §12.

> **v4 (2026-07-27)** — third review pass (Fable). Two net-new P1s + four P2s + a P3, all folded. **(D8)** free-then-reclaim is a race — fixed by transferring ownership to a snapshot token rather than freeing it. **(D2)** the then-current SSH quiesce needed a terminal vsock beacon; v5 removes the SSH session entirely and keeps the beacon as the final proof. Plus: snapshot images carry no baked host keys; the resume agent reconnects rather than holding a vsock connection; witness/egress attach before `Resumed`; `snapshot.mem` is bind-mounted, never copied; and repo transfer uses a host-created credential-free bundle. See the §12 v4 changelog.
>
> **v5 (2026-07-29)** — execution reconciliation after PRs #92–#96. The phase-1 task specs are now authoritative on mechanism: **no pre-snapshot SSH**, credential-free bundle/warm over the guest-agent vsock protocol, forced read-only rootfs + tmpfs overlay, beacon-gated seal, recoverable snapshot intent + reserve-before-reap transaction, and restore via a persistent one-live lease with an explicit hash-verified `--image`. D2, D4, D8, §7, §9, and Q7 below reflect this locked shape.
>
> **v6 (2026-08-21)** — Phase-2 implementation reconciliation. The as-built path adds a bounded
> `rooms clone -n 1..8` batch, concurrent CloneNet allocation and prepared-source restore, one
> namespace/veth/two-hop-NAT identity per clone, namespace-scoped witness and egress custody,
> multi-lessee snapshot tokens, exact-owner cancellation cleanup, and deterministic machine output.
> Rootfs and published snapshot inodes are permanently `FS_IMMUTABLE_FL`-sealed because Firecracker
> can demand-page both files after `Resumed`; path checks alone cannot close that lifetime race.
> `scripts/phase2-killer.sh` is the fail-closed host gate. No performance or `/work-driver`
> integration claim is complete until its named evidence exists.
>
> **Reviewers — focus areas:**
> - **§4 D2 + §7 A** — neutrality *by construction*: the sealed neutral-base boot mode + monotonic `provenance` state. This is the load-bearing security rule; if a base can be tainted after the neutrality check, the design leaks secrets into `snapshot.mem`.
> - **§4 D7 + §7 B** — the post-resume receiver + the ack gate. A neutral base boots without `--secret`; the retained agent reconnects through bounded poll/retry for the reseed/clock/identity nudge *on resume*, or every clone fails closed. Review the retry cadence/deadlines and the no-active-connection snapshot precondition (§10 Q2).
> - **§4 D3/D6 + §9** — review namespace/veth/NAT teardown, veth-keyed custody, multi-lease return,
>   and the immutable shared-inode contract together; breaking any one invalidates the fan-out proof.
> - **§8** — the fork hygiene matrix, especially the **userspace-PRNG** row: the captured minimal resume agent waits without drawing randomness; workload and `sshd` processes start only after hygiene.

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

**FR1.** `rooms base-create` produces a **sealed neutral base**: repo cloned **via the host-side
transport bundle** (never a guest-side authed clone — a credential in the base would break
neutrality by construction) + toolchain warm, **no secret channel armed, no interactive/agent
ingress**, agent process not started; the room's authoritative `provenance` is `neutral`, written
only after a verified quiesce (D2).

**FR2.** `rooms snapshot <base>` pauses the neutral base and writes a Full snapshot (vmstate +
guest-memory file, which includes the `/vsock` device) plus metadata; it **refuses** any room
whose `provenance` is not `neutral`.

**FR3.** `rooms restore <snap> (--keep | --command <cmd>)` boots a fresh microVM from a snapshot — no
kernel boot — reaching SSH-ready, and applies **restore-time hygiene** (reseed RNG, resync clock,
deliver identity + any secrets) over vsock, gated: no ack → no workload.

**FR4.** `rooms clone <snap> -n N` restores N clones concurrently, each in its own network
namespace (distinct host-side identity, isolated from siblings), each with fresh entropy, a
resynced clock, its own secrets/identity, and its own witness/egress plane.

**FR5.** A restore refuses (fails closed) when the snapshot's FC version or rootfs hash does not
match the host.

**FR6.** No secret is ever present in a snapshot file; secrets reach a guest **only** post-resume,
per-clone, over vsock. The captured minimal resume agent waits without drawing randomness; workload
and `sshd` processes start **after** restore hygiene so their userspace PRNGs seed fresh.

**FR7.** Each clone produces its own witness pcap and honors its own egress policy — per-clone
custody survives fork. This holds for a **single** restore too (FR3): witness capture + egress
chain are installed **before the guest resumes** (`Resumed`), so a restored room is never
weaker-custodied than a cold-booted one.

| NFR | Target |
|---|---|
| Restore latency | one eight-clone batch reaches the terminal workload-ready barrier in **<1s total**, measured by a monotonic clock around the real CLI plus its readiness gate |
| Fan-out density | aggregate Firecracker **PSS** for 8 clones is <2× the one-clone baseline; RSS/`free -m` are not accepted evidence because shared pages are counted misleadingly |
| Security | no secret in any snapshot file; no RNG/session-key reuse across clones (kernel **and** userspace); each clone isolated in its own netns |
| Fail-closed | incompatible snapshot → refuse; non-neutral base → refuse to snapshot; a clone whose hygiene nudge isn't acked → no workload |
| Blast radius | snapshot/restore is a sibling path; cold boot + non-forked rooms are byte-for-byte unchanged |

## 3. Architecture overview

```
  rooms base-create  →  SEALED NEUTRAL BASE (once):
     boot (no --secret, /vsock present, minimal resume-apply agent waiting; NO workload agent, NO interactive ingress)
       → clone repo → warm toolchain → idle.  provenance = neutral (authoritative, monotonic)
        │  rooms snapshot <base>   (refuses if provenance != neutral)
        ▼
     PAUSE → PUT /snapshot/create (Full) → { vmstate(+/vsock), mem_file } + metadata{fc_ver, rootfs_hash, slot/ip}
        │
        ▼  rooms clone <snap> -n 8      (bounded concurrent netns fan-out around restore)
   ┌──────────── restore() ×8 (sibling to boot(), File backend, MAP_PRIVATE CoW) ────────────┐
   │  each clone: own netns + veth + host NAT (identical inner IP reused, disambiguated by ns) │
   │              PUT /snapshot/load → Resumed                                                 │
   │              resume-apply agent (already waiting in the snapshot) connects the per-clone   │
   │              vsock listener → applies: reseed RNG · resync clock · secrets · run identity  │
   │              → ACKs.  Host gates: no ack ⇒ no workload_started.                            │
   │              THEN workload + freshly-keyed sshd start (fresh userspace PRNG) · own custody │
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

**Extended (not just reused):** the vsock secrets channel. The neutral base always attaches
`/vsock`; a minimal resume-apply agent is captured between bounded connection attempts and reconnects
after every restore. It applies per-clone reseed/clock/identity/secrets, creates the fresh SSH key,
and ACKs before either the SSH readiness barrier or workload can pass.

**Implemented, four seams:**
1. **sealed neutral-base mode** (`base-create`) — boots with no secret channel and no interactive
   ingress; records authoritative monotonic `provenance` (D2).
2. **`snapshot` module** — pause → `PUT /snapshot/create` → snapshot pair + metadata; refuses a
   non-neutral base.
3. **`restore()` beside `boot()`** — the disjoint FC API flow (`PUT /snapshot/load` before resume),
   staging snapshot+mem into the jail, reclaiming the frozen slot, **applying restore hygiene +
   ack gate** (D7).
4. **netns-per-clone networking** — an indexed exact-owner CloneNet allocator builds a namespace,
   veth pair, connected return route, and first-hop NAT for every clone. Firecracker, witness, SSH,
   and the per-veth egress chain execute in that namespace while the frozen guest IP stays unchanged.

## 4. Key decisions & trade-offs

**D1 — fork is the bet; snapshot is the mechanism; replay is rescoped. (DECIDED.)** Snapshot alone
buys warm-base reuse + pause/resume. Fork buys the differentiated parallelism payoff.
"Deterministic replay" is struck as unachievable (D5). This drives §9: snapshot+restore value first
(phase 1), netns fan-out second (phase 2), receipts as vocabulary (phase 3).

**D2 — neutrality by construction, and the base is *quiesced* before it's sealed. (DECIDED — load-bearing; revised v2, hardened v3–v5.)**
A Full snapshot's memory file is **plaintext guest RAM on disk**; a secret in RAM at snapshot time
is in the file and in every clone. v1 tried to enforce this by *observing* a `secrets_delivered`
lifecycle event — but `RoomMeta` has no such field, lifecycle output is non-authoritative, and a
`--keep` room can be tainted over SSH or vsock *after* the check (codex P1). So neutrality is a
property of **how the base is created**, recorded as durable authoritative state:
- `RoomMeta` gains a monotonic `provenance: provisioning | neutral | tainted` field (authoritative,
  persisted). A `base-create` room starts **`provisioning`** — the pre-seal window where only rooms'
  own credential-free guest-agent provisioning protocol is legitimate. `provisioning → neutral`
  happens only on the quiesced beacon; any *non-warm-up* ingress / secret-arm / workload-start flips
  `tainted` **irreversibly** from either state. No path back.
- `rooms snapshot` refuses unless `provenance == neutral`.
- **v5 — snapshot bases never start SSH or reach the network (locked).** The snapshot-capable image build omits host keys;
  `base-create` fails closed before boot if a supplied image contains any SSH host private key,
  forces the read-only rootfs + tmpfs-overlay path, installs `egress::Policy::None` before the VMM
  can transmit across both IPv4 `FORWARD` and host-local `INPUT`, disables IPv6 before interface
  setup, and suppresses `sshd`. Rooms sends the host-created
  repo bundle plus an optional credential-free warm command over a dedicated typed guest-agent
  vsock protocol with a scrubbed environment. Before warm-up the base agent atomically revokes the
  workload user's sudo grant and proves no alternate sudo policy remains; after warm-up it proves
  that the resume agent, SSH policy, sudo policy, and authorized key retained their captured shapes
  and hashes, and rejects legacy/XDG credential files plus credential-bearing system, global, or
  repository Git config. After stage/clone/warm ACKs, the agent stops non-essential services, closes
  every provisioning connection, validates the retained process set, and emits one terminal
  **`quiesced` beacon** on a separate one-shot endpoint. The host writes
  `provenance = neutral` only after that connection closes. No active vsock connection crosses the
  snapshot, and unlinking payload files is hygiene rather than memory sanitization.
Rejected: encrypt the snapshot (adds key management to dodge an ordering rule construction makes
free); a `--neutral` flag (an assertion, not an enforcement); leaving `sshd` up and firewalling the
guest IP (a net boundary the guest could still be reached behind on the host — stopping the service
is the smaller, surer cut).

**D8 — snapshot atomically inherits a persistent slot reservation before the base is reaped. (DECIDED — revised v4/v5.)**
The frozen guest IP is baked into the snapshot, so the slot must never enter the walk allocator.
Before snapshot creation, `rooms snapshot` exclusively creates a durable per-base indexed transaction intent containing
the complete planned `SnapshotMeta`, artifact paths, and transaction boundaries. After both binary
artifacts are synced, it durably rewrites the still-live base claim to a snapshot-owned reservation
under the slot lock, and only then terminates/reaps the base. `snapshot.json` is published last, after
checked process/jail/room plus egress/TAP cleanup; failures retain the intent and reservation for GC.
The `snapshot-recover` command resumes an indexed crash window without needing the consumed base or
recomputing its metadata.
GC consults the intent index before ordinary dead-room reconciliation: a matching pre-reservation
transaction fences both the base claim and its jail artifacts until the shared recovery state machine
collects the outputs and completes the handoff, or leaves the transaction protected and pending.
If a conclusively dead pre-reservation base has only invalid/partial outputs, the same state machine
terminally aborts: discard partials, prove no metadata/reservation exists, cleanly reap, and free only
the exact ordinary base claim. A reservation is never eligible for this abort.
The reservation is persistent: flat `rooms restore` takes the exclusive lease, while `rooms clone`
takes one bounded `@clone-lease` per exact `(snapshot_id, room_id)`; clean teardown removes only that
lessee and reconstructs the reservation after the last clone, never exposing the slot to the free
pool. A global restore tombstone survives
room deletion until process/jail/room plus egress/TAP teardown and lease return are durably complete;
any incomplete teardown or return retains the tombstone and lease until GC proves clean reap and
retries the exact return. Netns-per-clone (D3) makes the otherwise-identical frozen guest IP
unambiguous for every concurrent lessee.
Rejected: naive free-then-reclaim (the race above); the v3 form assumed atomic transfer was heavier
than a template destroy, but the reservation-token transfer *is* the destroy plus a one-line token
write — cheaper than accepting permanent restore starvation.

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
`boot()` is cleaner than a mode flag through one function. The CLI still requires `--image`: it
validates the rootfs hash and stages the file read-only at the drive path captured in vmstate before
`snapshot/load`; staging the backing file is not a drive-configuration API call.

**D5 — replay → checkpoint receipts. (DECIDED.)** Not bit-deterministic replay. A **checkpoint
receipt** = `{ snapshot_id, fc_version, rootfs_hash, base_repo_sha, pinned_inputs }` — enough to
*re-run from an identical starting world*. It falls out of the snapshot metadata (FR2) + the existing
artifact discipline; ship it as **vocabulary, not machinery**. The FC-version / rootfs-hash fields
also drive the FR5 compat guard, so it is not extra weight.

**D6 — File backend, not UFFD. (DECIDED for v0.2.)** `MAP_PRIVATE` on the memory file gives
automatic CoW page-sharing across clones through the page cache — enough for the fan-out density
target. UFFD (userspace pager, lazy load, cross-VM dedup) is CodeSandbox-scale machinery; revisit
only if the density target isn't met or snapshots grow too large to `File`-map. **The CoW win
requires all N clones to `MAP_PRIVATE` the *same inode* — `snapshot.mem` is therefore
bind-mounted into each clone's jail (the boot-path precedent, `firecracker.rs:29-31`, `951-952`),
never copied (Fable P2).** Copy it per clone and the page cache holds N private copies, the density
gate fails for a reason that looks like a Firecracker problem but is a one-line staging bug. The
small `snapshot.vmstate` may be copied (read once).

Firecracker can continue demand-paging both `snapshot.mem` and the backing rootfs after the load and
resume API calls return. Therefore a successful rootfs build seals the exact image inode with
`FS_IMMUTABLE_FL`; snapshot publication seals `snapshot.vmstate`, `snapshot.mem`, `snapshot.json`,
then the containing directory before recording `Published`; and every production restore refuses an
unsealed source. Seal recovery is idempotent and snapshot teardown never clears these flags. This is
the lifetime guarantee; repeated stat/hash checks remain substitution diagnostics, not write
exclusion.

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
  `{ reseed, clock: <host-now>, secrets…, run_id }`, derives `user.name = rooms <run_id>` and
  `user.email = <run_id>@rooms.invalid` for the rooms user, then ACKs. Git identity is a pure
  function of the already-canonical run id, not a second caller-controlled protocol field.
- **Restore-time hygiene moves to phase 1.** *Every* restore — even a single one — reseeds the RNG
  (kernel via VMGenID/virtio-rng per `random-for-clones.md`, plus the userspace guarantee below),
  steps the clock, and sets identity, behind the ack gate (reuses the vsock-secrets host sequencing:
  no ack ⇒ no `workload_started`). Phase 2 adds only the netns fan-out around this.
- **Userspace PRNG — every *retained* process, not just the agent (codex P1, hardened v3).** Kernel
  reseed does not touch an already-started process's userspace DRBG. v2 delayed the *agent*, but the
  base also ran `sshd` and the resume-apply agent itself — each can hold cloned DRBG state that new
  sessions reuse. v3 closes this at the source via D2 quiescing: **`sshd` and every non-essential
  daemon are stopped before the snapshot**, so the only process captured is the **minimal resume-apply
  agent, which draws no randomness before the nudge** (it blocks on the channel). The agent and a
  **freshly-keyed `sshd`** both start/reseed *after* the kernel reseed on resume. Any daemon that must
  survive the snapshot owns a named reseed/restart step, and the §9 gate validates *those* processes'
  draws, not just `/dev/urandom`.
- **Fresh `sshd` per clone (v3).** Because sealing stops `sshd` (D2), the snapshot has no `sshd`
  running and no host-key/session state to duplicate. On resume the resume-apply agent starts `sshd`
  with exactly one **freshly generated Ed25519 host key**, pins its runtime config to only that key,
  verifies the effective `sshd -T` configuration names exactly that key, restores the exact
  root-owned workload sudo grant only after hygiene succeeds, and applies the clone's identity — so
  the duplicate-host-key problem
  (§8) is solved by the same quiesce-then-restart move, and rooms' workload SSH (`wait_for_ssh`,
  `runner.rs:104`) reconnects to a distinct, freshly-keyed daemon per clone.
- **v4 — the agent's steady state is *between* connect attempts, never a held connection (Fable P2).**
  D7's "blocks on the channel" wording contradicts Q2(a) and, more importantly, Firecracker's
  snapshot support: **active vsock connections are not preserved across snapshot** — on resume the
  device issues a transport reset, severing anything open, and hybrid-vsock does not surface a host
  half-close as EOF (the fetch script documents this, `rooms-secrets-fetch.sh:40`), so an agent
  holding a connection across snapshot can hang forever on a dead stream and fail every clone closed
  at the ack gate. So the agent is a **poll-retry loop** (connect → fail/short-timeout → `nanosleep`
  → retry), each attempt with a **read deadline**; a process asleep in `nanosleep` survives
  pause/resume trivially. **"No active vsock connection at snapshot time" is an explicit snapshot
  precondition** (a FC constraint, not just hygiene) — which the quiesce beacon (D2 v4) must close
  *before* the pause, not hold open.
The implementation uses a 100ms reconnect cadence inside a one-minute host ACK bound. Quiescence
admits only the minimal resume agent; `sshd` starts after reseed with one freshly generated Ed25519
key and an effective configuration that names no alternate key source.

## 5. Data model

- **`RoomMeta.provenance`** (new, D2): `provisioning | neutral | tainted`, monotonic, authoritative,
  persisted. Starts `provisioning` for a `base-create` room (rooms' own warm-up is exempt); the
  quiesced beacon writes `neutral`; any non-warm-up secret-arm / workload-start / interactive session
  flips `tainted` irreversibly from either state. `rooms snapshot` reads *this* (requires `neutral`),
  never a lifecycle event.
- **Snapshot artifact** (per snapshot, `<state-base>/snapshots/<snapshot-id>` at `0700`):
  `snapshot.vmstate` (device/vCPU
  state, **includes the `/vsock` device**), `snapshot.mem` (guest memory — treat as a credential,
  though under D2 it holds no secret), `snapshot.json`: `{ schema_version, snapshot_id, created_at,
  fc_version, rootfs_hash, base_room_id, slot_index, guest_ip, base_repo_sha, provenance:"neutral" }`.
  All three artifact inodes and the directory carry `FS_IMMUTABLE_FL`; the supplied backing rootfs
  must carry the same flag.
- **Checkpoint receipt** (D5): the same metadata surfaced as a first-class artifact line for a run —
  no new store, a projection of `snapshot.json` + the run's pinned inputs.
- **`room.json`** gains additive fields: `provenance` (above), `snapshot_lineage`
  (`{ from_snapshot, base_room }`), and a non-owning restore slot intent. A restored room leaves
  `slot` unset until the exact snapshot lease succeeds, then atomically records leased ownership.
  `room.json` already versions additively (`room.rs:17`).
- **Transaction indexes** (owner-only, outside room/jail/artifact trees): `snapshot-intents` embeds the
  complete planned `SnapshotMeta` before snapshot creation; `restore-intents` retains PID/starttime,
  lineage, exact slot/tap, egress policy, cleanup progress, and expected lease through room deletion
  until resource cleanup and durable lease return. GC scans both indexes so recovery never depends on
  an already-consumed room directory.
- **Slot** (`slot.rs`): a live base claim transfers to a persistent snapshot reservation. Flat
  restore uses an exclusive lease; clone mode stores a canonical bounded set of exact
  `(snapshot_id, lessee_room_id)` leases. Clean teardown removes only its lessee and returns the
  final token to the reservation. Reserved/leased tokens are reconcile-exempt and never enter the
  walk allocator. Under D3 the *host-side* identity moves into a netns; the guest-visible /30 is
  unchanged.
- **CloneNet claim** (`clonenets/<index>`): exact owner id plus allocator-process identity. Namespace,
  veth, route, NAT, and global owner marker names derive from the index; cancellation, kill, and GC
  compare ownership before teardown so a stale cleanup cannot destroy a reassigned network.

## 6. API / config contract

**CLI (new verbs):**
```
rooms base-create --image <rootfs> --repo <r> [--warm <cmd>]  # forced RO+overlay, no SSH/secrets, agent-vsock provisioning
rooms snapshot    <base-id>                                  # transactional Full snapshot; REFUSES provenance != neutral
rooms snapshot-recover [<snapshot-id>]                       # list/resume indexed incomplete snapshot transactions
rooms restore     <snapshot> --image <rootfs> (--keep | --command <cmd>) [--slot <k>] [--secret <ENV>]... [--out <dir>] [--witness]
                                                            # lease frozen slot/IP, File backend, hygiene + explicit lifecycle
rooms clone       <snapshot> -n <1..8> --image <rootfs>       # keep the ready batch, or add --command for concurrent work
                  [--command <cmd> --out <dir> --witness]
                  [--secret <ENV>]... [--egress <policy>] [--max-wall <duration>] [--json]
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
secrets:{…}, run_id }`; it reseeds the kernel RNG, steps the clock, stages secrets + identity,
replaces the rooms user's global Git config and overrides only the warmed repository's two local
author keys with the derived `rooms.invalid` identity, then ACKs. Host blocks `workload_started`
until the ack (reuses vsock-secrets §5.4 sequencing).
The retained resume agent is the nudge receiver; only workload and `sshd` processes start after this.

## 7. Key flows

**A — create + quiesce + seal a neutral base (D2, v5).** The snapshot-capable build omits SSH host
keys, and `rooms base-create --repo r` rejects a supplied image containing any before boot. It then
forces a read-only backing rootfs with tmpfs overlay, fail-closed `Policy::None` across forwarded
and host-local IPv4 traffic plus base-only `ipv6.disable=1` before guest
execution, and no `sshd`; `/vsock` is present for the
minimal provisioning/resume agent. Rooms transfers a host-created credential-free repo bundle and
optional credential-free warm command over the typed guest-agent protocol. Its staging directory
remains root-owned and non-writable by the workload user, but is mode `0711` so that user can
traverse to the explicitly named bundle and warm script. After stage/clone/warm
ACKs, the agent closes the provisioning channel, stops non-essential services, validates the
retained process set, and emits a **`quiesced` beacon** over a separate one-shot connection. **Only
after that connection closes** is `provenance = neutral` persisted. No interactive ingress or active
vsock connection exists at snapshot time.

**B — snapshot, consume the base, restore one clone WITH hygiene (phase 1).** `rooms snapshot <base>`:
refuse if `provenance != neutral`; assert **no active vsock connection** (D7 v4 precondition);
canonicalize the output and reject every managed room/jail cleanup tree plus both snapshot/restore
transaction-index trees; exclusively create the per-base recovery
intent; stage owner-only jail targets writable by the Firecracker uid/gid; `PATCH /vm {Paused}`;
`PUT /snapshot/create {Full}`; collect both jail-created outputs into their private host artifact
paths, preserving a Firecracker-uid-readable/no-group-or-other-access memory inode, sync and validate
them, then durably record the create-and-collect-success boundary;
**durably reserve the slot while the base claim is live** (sync the token before
rename and the slot directory after); cleanly reap process/jail/room plus egress/TAP state; publish
`snapshot.json`; seal the exact artifact files and directory immutable; then record `Published`. A
`rooms restore <snap> --image <rootfs> (--keep | --command <cmd>) --slot k` writes a global
`PreSpawn` restore intent first, holds the child behind a parent-death barrier until PID/starttime are
durable in both indexed and room-local state, then leases the persistent reservation; compat guard
(FR5); a command output tree must be canonically disjoint from the supplied rootfs image, the entire
rooms state base, every jail-instance root, and every known custom completed or pending snapshot tree; stage the
hash-verified, immutable rootfs read-only; prepare the source once per request (metadata, FC
compatibility, and rootfs hash), then revalidate immutable exact inode identity at every custody
boundary; **bind-mount**
`snapshot.mem` + stage `vmstate`
into the jail (never copy the mem file, D6 v4); fresh FC process; **install the witness pcap + egress
chain BEFORE resume** (FR7 — same fail-closed posture as boot, `firecracker.rs:478-498`);
`PUT /snapshot/load` (File) → `Resumed`; the
waiting resume-apply agent applies the hygiene nudge — reseed kernel RNG, step clock, start a
**freshly-keyed `sshd`**, deliver identity — and ACKs (**no ack ⇒ no workload**); re-probe SSH
(`wait_for_ssh`, `runner.rs:104`, against the fresh `sshd`); admitted `--secret` values travel only
inside that acknowledged nudge, never SSH env forwarding. `--command` runs through the existing
runner and then tears down, while `--keep` explicitly hands off the live persisted room id. Witnessed
commands require `--out`; capture/output conflict with `--keep`. A bare restore is refused before
effects. Even a *single* restore reseeds — a warm base reused twice must not repeat RNG/clock/host-key.

**C — fork N clones (phase 2, the payoff).** `rooms clone <snap> -n 8` overlaps request-wide source
preparation with concurrent exact-owner CloneNet allocation, then restores the full batch
concurrently. Every clone gets a **netns** + veth + host NAT (identical inner IP, isolated), its own
hygiene nudge (`run_id` + optional per-invocation secrets), and namespace-scoped witness/egress
custody. Kept mode returns only after every clone passes the SSH barrier; command mode runs one
command concurrently, collects per-room artifacts, and tears every member down. Cancellation and
partial failure retain durable intents until exact cleanup can finish. Clones map the same immutable
memory inode privately, so only dirtied pages diverge.

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
| **Userspace PRNG (every retained process)** | kernel reseed does **not** touch an already-started process's DRBG | **D2 v5:** snapshot mode never starts `sshd`; only the minimal resume-apply agent is retained and it draws no randomness before the nudge. Fresh `sshd` starts after kernel reseed. §9 validates retained-process draws, not only `/dev/urandom` |
| **SSH host key / sessions** | a running `sshd` in the snapshot → every clone shares host key + session RNG | **D2 v5:** snapshot mode carries no baked host key and never starts `sshd`; the resume-apply agent generates and starts a freshly-keyed daemon per restore |
| Wall clock / kvmclock | resumes stale → token-expiry + TLS-validity wrong | post-resume clock step in the same hygiene nudge, on every restore |
| Secrets in RAM | baked into `snapshot.mem`, shared by all clones | **D2 neutrality by construction**: no secret ever in the base; secrets only post-fork over vsock |
| Base still owns its slot | snapshot pauses while the live base claim remains | **D8 v5:** reserve-before-reap atomically transfers the claim to a persistent snapshot reservation; restore leases and returns it |
| Snapshot files on disk | plaintext memory readable or mutable after admission | `0700` snapshot-owned directory, private artifact modes, permanent `FS_IMMUTABLE_FL` on artifacts + directory, and exact-inode restore checks; still treat the memory image as a credential |
| FC version pinning | snapshot unloadable after FC upgrade | record `fc_version`; compat guard refuses (FR5); non-goal to migrate |
| Dead TCP / vsock across restore | pre-snapshot connections gone | host re-probes SSH (fresh `sshd`); the resume-apply agent (re)connects the per-clone vsock listener on resume (D7) |
| Agent run identity | same git author / run id across forks | retained resume agent receives a canonical per-clone `run_id`, derives a minimal credential-free global Git identity from it, overrides the warmed repository's local author keys, and starts the workload agent only after the ACK |

The invariant reviewers should try to break: **no clone reaches `workload_started` sharing another
clone's network identity, SSH host key, kernel or userspace RNG stream (in *any* retained process),
stale clock, or a secret from the base snapshot.**

## 9. Rollout / implementation plan

| Phase | Goal | High-level tasks | Depends on | Gate |
|---|---|---|---|---|
| **1. snapshot-restore — landed** | a sealed neutral base snapshots and restores to a single working room **with full hygiene** | `base-create`, neutral provenance, recoverable Full snapshot, persistent reservation, `restore` sibling, immutable backing artifacts, pre-resume custody, hygiene ACK, and exact teardown | Firecracker snapshot GA | current branch re-exercises the neutral base → snapshot → one-clone host path |
| **2. fork-clones — implemented, not yet landed** | N clones from one snapshot, each isolated — the differentiated payoff | indexed CloneNet allocator + namespace/veth/two-hop NAT; multi-lessee reservation; prepared-source concurrent restore; namespace-aware runner/witness/egress; deterministic `rooms clone -n 1..8`; crash/cancellation cleanup | phase 1 | **VALIDATION GATE** (killer demo) below, then exact-head review and Gate |
| **3. checkpoint-receipts + hardening** (stub) | replay-rescope vocabulary + fleet ergonomics | checkpoint receipt as a first-class artifact (D5); snapshot GC/retention; FC-upgrade library-invalidation ergonomics; UFFD only if density missed | phase 2 + gate | each item needs a demonstrated need first |

Phase 2 is intentionally one integrated reviewed slice because the safety argument crosses allocator,
lease, Firecracker namespace, egress, witness, guest hygiene, and teardown boundaries. Phase 3 remains
unsized; immutable artifact deletion/retention and direct consumer ergonomics need demonstrated demand.

**Intermediate gate (after phase 1):** on the rooms-host, `base-create` a neutral base, `snapshot`
it, `restore --command` it **twice**, then exercise one `restore --keep` handoff and confirm: (a) all
restores reach workload-ready; (b) the compat guard
refuses a forced fc_version/rootfs mismatch; (c) the two restores show **distinct kernel RNG draws, a
correct (resynced) clock, and distinct `sshd` host keys** — hygiene fires on every restore, not just
fork; (d) `snapshot` refuses a non-neutral room; (e) the snapshot memory file, grepped for **any
warm-up-time secret and the clone's *runtime-generated* host key**, contains **neither** — proving
quiesce + neutrality cleared them. A snapshot-capable image must **omit build-time host-key
generation entirely**; deleting `/etc/ssh/ssh_host_*` during quiesce is not an allowed fallback
because loaded or cached key bytes can remain in `snapshot.mem`. The resume-apply agent generates a
fresh key **into the overlay** before starting `sshd` per clone. Grep the runtime key and verify the
base image contained no baked private key.

**VALIDATION GATE (after phase 2) — the killer demo:** boot one base, clone a repo + warm the
`claude` toolchain, `rooms snapshot`; then `rooms clone <snap> -n 8` and:
- (a) 8 clones reach workload-ready in **under a second total**;
- (b) **aggregate PSS** (`/proc/*/smaps_rollup`, or a controlled cgroup memory delta with a
  before/after baseline) shows the fleet **≪ 8 × 256 MiB** of private memory — proving CoW page-cache
  sharing, not eight private copies (v3: `free -m`/RSS can't establish this — a shared page counts once
  in every process's RSS, codex P2);
- (c) 8 real `/work-driver` tasks run in parallel on a host that could not hold 8 cold rooms;
- (d) **8 distinct witness pcaps** — per-clone custody survives fork;
- (e) **each clone is a distinct netns/veth/NAT identity with verified cross-clone isolation** (clone
  A cannot reach clone B), a **freshly-keyed `sshd`** (distinct host keys), and an
  **application-level** RNG-draw probe across the *retained* processes (not just `/dev/urandom`) shows
  distinct output per clone — the hygiene matrix holds.
The Rooms-owned killer may prove eight concurrent repository workloads and eight custody artifacts,
but it must label that honestly: it does **not** satisfy (c) unless Ship's actual `/work-driver`
dispatch path selected Rooms and produced eight independently tracked tasks. That consumer proof may
require a separate Ship change and cannot be inferred from eight broadcast shell commands.
Phase 3 is not committed until this passes.

## 10. Remaining questions

1. **Consumer selection.** What is the smallest Ship `/work-driver` adapter that selects
   `rooms clone` for an eligible file-disjoint batch while retaining per-task commands, outputs, and
   lifecycle records? The Rooms CLI is implemented; consumer adoption is a separate claim.
2. **How warm should a neutral base be?** The safe boundary is locked—repo bundle and toolchain warm,
   before workload/`sshd`—but the best cache set is workload-dependent. Measure additional warm steps
   against snapshot size, neutral-process admission, and the literal fleet latency gate.
3. **Artifact retention and upgrades.** Published rootfs/snapshot inodes are intentionally immutable.
   A future delete/upgrade verb must first prove no room or intent references the exact artifact,
   clear flags only on that identity, and delete durably. Until then, replacement is the explicit
   rootfs-builder path and snapshots are operator-retained evidence.

## 11. Validation plan

`scripts/phase2-killer.sh` owns the exact reproducible host gate and writes `summary.json` plus hashes,
topology, PSS, two-hop return-path, guest-hygiene, witness, teardown, and leak-audit evidence beneath a
unique preserved proof root. It builds the current source and a fresh immutable rootfs, warms a neutral
base, publishes an immutable snapshot, measures one clone, launches and probes the kept eight-clone
fleet, tears it down exactly, then runs eight witnessed command-mode repository workloads. The gate
fails on `>=1s`, aggregate fleet PSS `>=2×` the one-clone baseline, duplicated post-reseed draws or
fresh-key evidence,
missing namespace/NAT return traffic, sibling reachability, mutable/substituted artifacts, incomplete
or aliased pcaps, reservation drift, or any proof-owned residue. A passing Rooms subgate summary has
`status: "partial"` and still records that
`/work-driver` consumer integration is outside its evidence; that separately tracked acceptance item
must remain open until an actual consumer run exists.

## 12. Changelog

**v2 (2026-07-27) — design-review pass (codex, 4×P1 + 1×P2):**
- **P1 "persist neutrality as authoritative state"** → **D2 rewritten**: neutrality by construction
  (sealed `base-create` mode + monotonic authoritative `provenance`), not an observed
  `secrets_delivered` event. §5, §7 A.
- **P1 "add a post-resume receiver for the vsock nudge"** → **D7 (new)**: the neutral base carries
  `/vsock` into the snapshot and the boot-time one-shot becomes a long-lived resume-apply agent
  captured between bounded connection attempts; §3 "Extended", §6, §10 Q2 (retry tuning).
- **P1 "move restore hygiene into phase 1"** → **D7 + §9**: reseed/clock/identity + ack gate now in
  phase 1 (single restore reused >once already duplicates RNG/clock); phase 2 is netns fan-out only.
  New phase-1 intermediate gate checks RNG/clock on a double-restore.
- **P1 "reset cloned userspace PRNG"** → **§8 userspace-PRNG row + D7**: structural fix — snapshot
  before workload/`sshd` processes start; §9 gate probes the **application** PRNG, not just `/dev/urandom`.
- **P2 "align identity validation with the netns model"** → **§9 gate rewritten**: verify distinct
  netns/veth/NAT identity + cross-clone isolation (inner MAC/IP are intentionally identical under
  D3), replacing the impossible "no two clones share a MAC/IP" check.

**v3 (2026-07-27) — second review pass (codex, 3×P1 + 1×P2).** All four converged on live crypto
daemons in the base at snapshot time; fixed by quiescing the base before sealing.
- **P1 "free the base slot before restore"** → **D8 (new)** + §7 B introduced the consumed-template
  requirement. v4/v5 supersede its original free/reclaim mechanism with reserve-before-reap and a
  persistent one-live lease.
- **P1 "seal the guest's direct SSH path"** → **D2 hardened** + §7 A: sealing now **stops `sshd` +
  non-essential daemons** (the image bakes a reachable `sshd` + the operator key,
  `build-rootfs-alpine.sh:271`) *before* marking neutral — refusing rooms `exec` alone left a live SSH
  path into the "neutral" base.
- **P1 "reseed every snapshotted userspace process"** → **§8 + D7 hardened**: quiesce removes `sshd`'s
  and other daemons' captured DRBG; the only retained process is the minimal resume-apply agent (draws
  no randomness pre-nudge); a fresh-keyed `sshd` starts post-reseed; the gate validates retained
  processes, not just `/dev/urandom`.
- **P2 "measure sharing with PSS instead of `free`"** → **§9/§11 gate**: aggregate PSS
  (`smaps_rollup`) or a cgroup memory delta with a baseline, since RSS counts a shared page in every
  process.
- New open questions §10 Q6 (quiesce/reseed protocol) + Q7 (warm-up transport vs the `sshd` it stops).

**v4 (2026-07-27) — third review pass (Fable, 2×P1 + 4×P2 + 1×P3).** Anchors + FC API claims
verified accurate; the holes were in ordering, verifiability, and staging.
- **P1 "free-then-reclaim slot race"** → **D8 rewritten** + §7B/§9(1b): a released slot is walk-claimed
  (`slot.rs:99-104`, test `:562-570`) by a concurrent room before restore reclaims it → permanent
  `TargetTaken` starvation on the busy host this targets. Fix: transfer the slot to a never-reclaim
  reservation token (`reconcile`-exempt), don't free it. Crash-window between create + teardown named.
- **P1 "quiesce leaves the invoking sshd child alive + unverifiable"** → **D2 hardened (v4)** + §7A
  added the terminal vsock beacon. v5 removes pre-snapshot `sshd` entirely and retains the beacon as
  the process-set proof before `provenance=neutral`.
- **P2 "baked host keys pollute the mem-grep gate"** → **§9(e)**: `ssh-keygen -A` at build
  (`build-rootfs-alpine.sh:281`) leaves keys in page cache; drop build-time keygen for
  snapshot images, key per clone into the overlay, grep the runtime key not the baked one.
- **P2 "D7 vs Q2(a) contradiction on a real FC vsock limit"** → **D7 hardened (v4)** + §10 Q2: active
  vsock connections don't survive snapshot (transport reset on resume); agent is poll-retry with a
  read deadline, never a held connection; "no active vsock conn at snapshot" is an explicit precondition.
- **P2 "phase-1 restore never places witness/egress"** → **FR7 + §7B + §9(1c)**: install witness pcap +
  egress chain **before `Resumed`** (`firecracker.rs:478-498` posture) so a restored room isn't
  weaker-custodied than a cold one.
- **P2 "stage into jail must be bind-mount, not copy"** → **D6 (v4)**: all clones `MAP_PRIVATE` the same
  `snapshot.mem` inode (bind-mount, `firecracker.rs:29-31/951-952`); a per-clone copy silently kills
  the CoW density thesis.
- **P3 "private-repo warm-up can taint the base"** → **FR1 + §7A**: repo transfer uses the host-side
  bundle (no guest credential); a guest-side authed clone in `base-create` is forbidden.

**v4.1 (2026-07-27) — spec-PR review (codex, 2×P1 + 1×P2 on the phase-1 task specs #91).**
- **P1 "preserve the reservation across repeated restores"** → **D8**: the snapshot's slot reservation
  is *persistent*; a restore *leases* it and teardown *returns* it (not free-to-pool), or the
  gate-required double-restore re-opens the steal race.
- **P1 "exempt the provisioning SSH session from taint"** → **D2/§5**: `provenance` gains a
  **`provisioning`** pre-seal state. v5 removes the SSH session entirely; only the credential-free
  guest-agent provisioning protocol is permitted before `provisioning → neutral` on the beacon.
- **P2 "deleting a loaded host key isn't sanitization"** → **D2/§7A**: if `sshd` ran pre-snapshot it
  already loaded the baked private key into pages that reach `snapshot.mem`; the snapshot-capable base
  must never load a baked host key (no baked keys + non-interactive warm-up preferred), not merely
  unlink the file during quiesce.

**v6 (2026-08-21) — Phase-2 as-built reconciliation.**
- Added the bounded concurrent clone batch, canonical multi-lessee slot tokens, exact-owner CloneNet
  allocation/reconciliation, namespace-aware Firecracker/runner/witness/egress, and deterministic
  partial-failure records.
- Added cancellation-safe restore/teardown custody and a real one-clone privileged integration test.
- Hardened neutral warm and resume hygiene: byte-exact BusyBox framing, no warm-time sudo, pre/post
  credential and policy seals, one pinned fresh Ed25519 host key, exact post-hygiene sudo restore.
- Made backing safety a kernel invariant: rootfs and published snapshot inodes are immutable for the
  complete demand-paging lifetime; crash recovery completes partial sealing idempotently.
- Materialized the exact eight-clone host proof as `scripts/phase2-killer.sh`, including literal
  latency, PSS, shared inode, bidirectional two-hop NAT, isolation, key/RNG/clock identity, eight
  witnesses, reservation return, and terminal leak checks. `/work-driver` adoption remains a
  separately named consumer gate.
