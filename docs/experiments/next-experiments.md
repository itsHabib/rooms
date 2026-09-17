# Experiments 2a–2d and 14–22: tool delivery, swarms and shared state

Added at Michael's request on 2026-09-16. These extend the
[experiment list](ten-experiments.md) and [experiments 11–13](storage-observability-deployments.md);
all are planned, not implemented or measured. The earlier experiments and their
remaining work stay in scope. Each entry should produce a working prototype, a
reproducible comparison, receipts and a short lesson.

Rooms stays substrate. Fleet, key-value stores and swarm policy live in harnesses
and recipes under `scripts/` and `examples/`, not in `src/`. The one substrate
change several entries may justify is a guest-reachable vsock service port; decide
that from evidence. Nothing here starts paid resources or enlarges the existing
budget; entries marked *local* run on the Lima host.

## Is Nix the right idea? (2a–2d)

Nix does two jobs in Rooms: resolving a pinned toolset and delivering it into
the guest. Experiment 2 measured the delivery side: the 182.5 MiB store was
resident once on the host, but every guest kept its own page cache, and peak
summed VMM PSS grew linearly to 41.32 GiB at 64 rooms. That duplication follows
from attaching the store over `virtio-blk`, not from Nix. These entries separate
the two jobs.

### 2a. Delivery bake-off (*local*)

**Question:** for one frozen toolset, which delivery mechanism gives the lowest
memory per room and time to first command?

**Prototype:** the experiment 2 workload and 16/32/64 ramp against each of:

- Nix toolstore on `virtio-blk` (baseline)
- the same store on virtio-pmem with DAX, the result experiment 2 did not reach
- virtiofs with DAX under Cloud Hypervisor, since Firecracker has no virtiofs;
  label the VMM difference in every comparison
- an OCI image flattened to erofs or squashfs
- tools baked into the base snapshot, with no store device

Measure PSS per room, host page cache, time from dispatch to first command,
build time and artifact bytes. Reuse the per-clone `dispatch_to_resume_ack_seconds`
and `dispatch_to_ssh_ready_seconds` readiness fields from the clone JSON record.

### 2b. Snapshot as the package manager (*local*)

**Question:** if a base is provisioned once and cloned, does copy-on-write memory
already remove the duplication, making the delivery mechanism secondary?

**Prototype:** clone 16/32/64 from one warmed base and compare shared and private
pages with and without host KSM, and with File versus UFFD backing. Report the
KSM scan cost and its side-channel caveat beside any saving.

### 2c. Authoring ergonomics (*local*)

**Question:** which toolchain definition can an agent change correctly?

**Prototype:** one fixed task, "add this dependency and prove it works in a
room", attempted under a Nix flake, a Dockerfile converted to a rootfs,
mise or devbox, and apt followed by a snapshot. Count attempts, wall time and
the final reproducibility of each. Scoped model budget, stated before the run.

### 2d. Lazy and chunked images (*cloud*)

**Question:** can a host start a room after fetching only the chunks it touches?

**Prototype:** compare the experiment 5 bundle with Nydus or eStargz for the
rootfs and a content-addressed chunk store (casync or desync) for snapshot
memory. Measure bytes transferred, time to ready on a cold host and deduplication
across two related snapshots.

## Swarms and shared state (14–19)

Fleet today keeps mail, leases, receipts and slots on one machine's filesystem.
Rooms clones a machine in about a second. These entries combine the two.

### 14. Fleet seats as Rooms (*local*)

**Question:** can a Fleet pool hand out microVM clones where it now hands out
worktree directories?

**Prototype:** a pool adapter outside both repositories' cores: a seat is a clone
of a warmed base, a dead peer is a killed VM, and a handoff is a snapshot plus
the usual checkpoint. Extends experiment 8. Measure seat creation and recycle
time against worktree slots, and show that one peer cannot read another's files.

### 15. Shared state plane (*local*)

**Question:** does a networked store beat file-based coordination once peers are
isolated machines?

**Prototype:** Valkey or NATS JetStream on the host, reached from guests over
vsock, carrying mail, leases with a TTL and receipt streams. Compare with
file-based Fleet at 4, 16 and 64 peers: claim latency, double-claim rate under a
partition made with experiment 6's egress controls, and recovery after the store
is killed. Model-check the lease protocol and replay the model's traces against
the running system.

### 16. Speculative swarm (*local, model budget*)

**Question:** does re-forking from the current best branch beat independent
branches at the same cost?

**Prototype:** extends experiment 4. Snapshot at a decision point, clone N ways,
let peers publish partial scores to the store from 15; a coordinator kills
losers early and re-forks the leader. Compare quality per dollar with serial
execution and with N independent branches. Scoped model budget.

### 17. Peer-to-peer snapshot distribution (*cloud*)

**Question:** can hosts exchange snapshot chunks directly, with no central bucket?

**Prototype:** builds on 2d. Hosts advertise the chunks they hold and fetch from
each other. Measure time to ready on a third host as the number of seeding hosts
grows, and verify every chunk hash on receipt.

### 18. Work stealing and autonomous failover (*cloud*)

**Question:** can surviving hosts finish a preempted host's rooms with no
coordinator involvement?

**Prototype:** room specs on a shared stream, periodic checkpoints, and idle hosts
that claim unacknowledged work. Kill a Spot host as in experiment 10. This is the
autonomous failover experiment 10 explicitly did not claim; no false completion
and no duplicated side effects are the pass conditions.

### 19. Shared build cache (*local*)

**Question:** how much of experiment 9's CI time is repeated compilation?

**Prototype:** sccache or a remote-cache server on the host, reached by every
room. Rerun experiment 9's shards cold and warm. Report hit rate, wall time and
the trust boundary: a cache shared between tenants is a poisoning path, so state
who may write.

## Further out (20–22)

### 20. Time-travel debugging

**Question:** can a failed agent run be rewound to the step before it went wrong?

**Prototype:** periodic diff snapshots during a run, an index from transcript
step to snapshot, and a command that restores the room at a chosen step for a
human or a second agent to inspect. Measure snapshot overhead against interval.

### 21. Deterministic fault injection for peers

**Question:** do Fleet's coordination rules hold under scheduled failures?

**Prototype:** run a small swarm from 14 and 15 under a seeded schedule of pauses,
kills, clock steps and partitions, and check the invariants from the lease model
after every schedule. A failing seed must replay to the same failure.

### 22. Prewarmed clone pool

**Question:** what does it cost to make a room appear ready in under 100 ms?

**Prototype:** keep K resumed, idle clones and hand one out on request while a
replacement warms. Measure request-to-ready and the idle memory cost of K, using
the density figures from experiment 1 and the sharing result from 2b.

## Suggested order

1. 2a and 2b: local, free, and they answer the Nix question with data.
2. 14 and 15 together: they turn experiment 8 into a real swarm and everything
   from 16 onward depends on them.
3. 16 as the first demonstration, then 21 to show the swarm is trustworthy.
4. 2d, 17 and 18 when cloud hosts are next justified; they share one budget request.
