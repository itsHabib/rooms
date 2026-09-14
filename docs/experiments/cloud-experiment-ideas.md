# Cloud experiment ideas

**Status: original experiment proposals.** This preserves the questions that motivated the cloud lab. Measurements, remaining work and three subsequent additions are tracked in [PR #125](https://github.com/itsHabib/rooms/pull/125). The starting-point observations below predate that lab and are not current capacity claims. Costs are hypotheses until measured; cloud runs use the operator's existing spend authorization and explicit cleanup.

## Starting point before the cloud lab

- **Cold rooms with Nix.** #121 attaches a sealed, digest-checked Nix closure (squashfs) read-only at `/nix`, with private writable scratch. A real Fleet-authored Workbench patch passed its 16 Python tests in a cold room in 18.3 s on local Lima. The headless run in workbench#344 repeated it at 14.7–17.3 s.
- **Snapshots and clones.** #123 prepares once and restores many. On nested aarch64 Lima: the cold run took 18.2 s and a restore took 15.8 s. Readiness fell from 11.4 s to 1.9 s, but the tests slowed from 3.0 s to 10 s because of nested copy-on-write page faults. Running 2 or 4 clones at once gave no throughput gain. Writing the snapshot took 15.4 s to disk or 5.8 s to tmpfs, for a 1 GiB memory file. Memory per room (PSS) was not measured.
- **Earlier cloud run.** On a GCP N2-standard-8 (8 vCPU / 32 GiB, nested KVM), 1, 2 and 4 polyglot rooms took about 21.5, 22.1 and 27.1 s each, and all passed. Four rooms together used about 2.5 GiB (summed PSS, excluding host cache). The whole run cost well under a dollar.
- **Not yet measured at that starting point:** bare-metal KVM, density beyond 4 rooms, per-room memory at scale, and lazy memory restore.

Nested virtualization on a Mac hides the numbers that matter. Every idea below needs a real KVM host.

## The headline numbers

### 1. Density ramp
- **Question:** how many restored rooms fit on one host before latency or memory falls apart?
- **Run:** from one prepared snapshot, restore 8 → 16 → 32 → 64 → 128 rooms, each applying and testing the same Workbench patch. Do it first on nested-virt GCP (spot), then once on a bare-metal host to price the cost of nesting.
- **Measure:** per-room PSS and host RSS, time-to-ready and time-to-result (p50/p99), throughput (rooms completed per minute), and the knee where adding rooms stops helping.
- **Unlocks:** the defining number, *rooms per dollar*, which every product claim hangs off.

### 2. The shared Nix store as shared memory
- **Question:** can every room map the same physical pages for `/nix` instead of each caching its own copy?
- **Run:** at 16/32/64 rooms, compare today's read-only virtio-blk squashfs against a shared-memory attachment such as virtio-pmem with DAX. Cloud Hypervisor supports that; check whether Firecracker's device model does before building on it. As a fallback, measure how much the host page cache already shares the one read-only image.
- **Measure:** total host memory at N rooms under each attachment, and whether startup time changes.
- **Unlocks:** potentially a large drop in memory per room at density, which makes #1's number much better.

### 3. Cold vs restored, on real KVM
- **Question:** does restore actually beat cold once nested page faults are gone, and does lazy loading make "ready" nearly instant?
- **Run:** repeat #123's cold-vs-restore comparison on real KVM. Add a lazy restore where memory pages stream in on demand (userfaultfd page-fault handling, which Firecracker supports for snapshot restore), from local disk, tmpfs, and hugepage-backed tmpfs.
- **Measure:** readiness, test time, and total useful-work latency for each backing.
- **Unlocks:** confirms or kills the sub-second-ready claim with real evidence.

## Exotic

### 4. Fork the world for agents
- **Question:** can Rooms turn agent work into cheap parallel search?
- **Run:** snapshot a room at an agent's decision point, then restore N copies from that one moment. Give each copy a different next action (a different patch, fix or approach), run the tests in all of them, and keep the winner. Try it on a real Workbench task with 4, 8 and 16 branches.
- **Measure:** the time and cost to explore N branches versus running them one after another, and how often the extra branches find a better answer.
- **Unlocks:** speculative execution for agents, with rooms as the undo button. It's native to Rooms' snapshot design and hard to do cheaply anywhere else. It is the demo that makes Rooms plus agents click instantly.

### 5. Rooms that travel
- **Question:** can a room pause on one host and resume on another?
- **Run:** snapshot on host A, ship the memory file and disk diff through object storage, and restore on host B. Compare against restoring from local disk.
- **Measure:** end-to-end resume latency, bytes shipped, and whether the evidence comes out identical.
- **Unlocks:** portable work across a fleet of boxes, and survival when a spot host is preempted.

### 6. Hostile tenants
- **Question:** do the isolation and resource limits hold at density?
- **Run:** at 64 rooms, mix normal workloads with fork bombs, disk fills, egress attempts, `/nix` tampering, and memory hogs.
- **Measure:** that every limit and refusal holds and nothing escapes the room, plus the noisy-neighbor effect on everyone else's p99.
- **Unlocks:** a prerequisite before Rooms runs anything untrusted.

### 7. A GPU room
- **Question:** can one room host a local model for the whole fleet?
- **Run:** pass a GPU through to one room (VFIO) running a local model server, then point other rooms' agents at it for cheap inference.
- **Caveat:** Firecracker doesn't do device passthrough, so this means a different VMM (e.g. Cloud Hypervisor) for GPU rooms. That's a real design fork, and worth knowing about before choosing.
- **Unlocks:** ties Rooms to local-first model offload; isolated, shared inference.

## Toward a peer-to-peer fleet

### 8. Agents inside rooms
- **Question:** can agents run *in* rooms and coordinate without a supervisor on the host?
- **Run:** first, run the headless verifier from workbench#344 inside a room with a scoped credential and a Fleet mail and receipt endpoint it can reach from inside. Then run two agent rooms that talk over Fleet mail, with the host watcher only as a waker.
- **Measure:** that the task completes with receipts at the exact head, that the credential can't reach beyond its scope, and wall time versus agents on the host.
- **Unlocks:** the first real step toward a fully headless, peer-to-peer fleet.

### 9. CI in rooms
- **Question:** is Rooms a better CI runner?
- **Run:** shard Workbench's full test suite across 32 rooms on one host.
- **Measure:** wall time and cost per CI run versus GitHub Actions, and whether results are reproducible across shards.
- **Unlocks:** a concrete, useful product claim with a price tag.

### 10. Chaos on spot
- **Question:** does everything recover when the host dies mid-run?
- **Run:** run rooms on a spot or preemptible VM and kill the host mid-run (or let preemption do it).
- **Measure:** that receipts stay truthful (no false passes), cleanup completes on the next host, and Fleet checkpoint recovery resumes the work. Also the cost savings versus on-demand.
- **Unlocks:** confidence to run the fleet on the cheapest capacity.

## Recommended first three

1. **#1 + #2 together:** density plus the shared Nix store. This is the defining number. It's half a day on a spot host, probably a few dollars.
2. **#4, fork the world:** the demo that shows why Rooms matters for agents.
3. **#8, agents inside rooms:** the step toward the headless peer-to-peer fleet.

## Ground rules for any run

- An explicit spend OK and a hard cap per run. Delete every paid resource at the end and record the actual bill.
- Reuse the existing preflight (`rooms doctor`), leak and hash audit, and cleanup checks.
- Record results under `docs/experiments/<name>-<date>.md` with raw evidence alongside, like the snapshot matrix proof. Keep "measured" separate from "estimated".
- No new scheduler, registry, or cloud service unless a measured failure demands one.
