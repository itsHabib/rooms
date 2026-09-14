# Experiments 11–13: storage, tracing and deployment visibility

Added at Michael's request on September 14, 2026. These extend the original ten
experiments; they are planned, not implemented or measured. Each should produce a
working prototype, a reproducible comparison, receipts and a short lesson that
can inform a useful Rooms feature. The longer-term direction is a hosted Rooms
product with an understandable operator experience.

The previous experiments and their remaining work stay in scope. Track progress
in [the experiment list](ten-experiments.md). Reuse the Rust cloud lab and existing
Rooms/Fleet records before adding another framework or state store. These entries
do not start paid resources, resume paused agents or enlarge the existing budget.

## 11. NVMe, logical volumes and cache behavior

**Question:** can the operator's earlier local physical-volume/logical-volume
work improve Rooms preparation, clone storage, restore throughput or cost?
"NVM" is interpreted here as NVMe storage; locate the earlier implementation
and confirm its actual devices, layout and cache behavior before choosing a design.
Its location and results have not yet been recovered.

**Prototype:** a reproducible storage fixture for the same frozen Rooms workload,
plus a small Rust harness extension to record storage measurements. Start with
ordinary files on a disposable device and compare an equivalent logical-volume
layout on that same device. Add thin snapshots and an NVMe-backed cache variant
only where the recovered implementation and hardware make those comparisons
meaningful. Keep the filesystem, workload and integrity checks comparable; label
cold-cache and warm-cache trials separately.

Measure base preparation and snapshot creation, restore-to-ready and result times,
throughput, logical/allocated bytes, host and guest memory, backing-device I/O,
cache hit/miss observations when available, and total setup/reuse cost. Distinguish
faster hardware from a better layout. This is not assumed to solve the duplicated
guest-memory caches observed in experiment 2.

Exercise clone write isolation, full data/metadata pools, interruption during
snapshot creation, restart/recovery and cleanup. Use explicitly disposable test
storage; preserve the operator's existing volume layout and data. No formatting
or PV/VG/LV changes are part of merely tracking this experiment.

**Deliver:** an executable fixture and comparison report, a layout diagram and
recovery guide, then a focused Rooms storage POC if the measurements justify it.
A negative result should explain which existing path to retain.

## 12. eBPF tracing that answers real Rooms questions

**Question:** what useful visibility is missing from existing lifecycle events,
receipts, memory samples and the host network witness, and can eBPF fill it at
acceptable overhead?

Continue [the existing agent-tracing proposal](../features/ebpf-agent-tracing/idea.md)
and preserve the [host-witness trust model](../features/host-witness/spec.md).
That proposal records an August kernel probe; recheck the exact kernel used for
this experiment rather than treating the older capability result as current.

**Prototype:** choose a small set of concrete questions, such as "which command
ran?", "which file operation failed?" and "what delayed this restore?" Reuse
existing host observations where sufficient. If guest process/file events are
needed, prototype an in-guest tracer and stream its events to a host-owned output
artifact. Label host observations and guest-reported observations separately.
Prefer a Rust implementation where it fits the existing harness; choose the
specific BPF library after checking kernel support and a minimal working probe.

Attach room/attempt identity and timestamps; represent dropped events, tracer
failure and collection gaps explicitly. Avoid collecting file contents or secret
argument values by default. Do not equate an empty trace with no activity or a
guest trace with tamper-proof evidence.

Replay known exec/file/network actions, kill the tracer, induce event pressure,
exercise forbidden operations and compare tracing-on/off overhead under the
existing CI and density workloads. Check whether the trace explains a real failure
more clearly than the current logs.

**Deliver:** a runnable tracing POC, raw events, a readable per-Room trace/report,
loss/overhead measurements and an integration guide. Promote the useful signals
into Rooms diagnostics; do not introduce tracing as an automatic new merge gate.

## 13. Telemetry and UI for Rooms deployments

**Question:** can a person or supervising agent quickly understand which hosts and
Rooms exist, what work they are doing, where a failure happened and what remains
to clean up?

**Prototype:** a working deployment overview and Room detail page backed by real
Rooms state and retained lab receipts. Start from lifecycle, registry and witness
records plus Fleet work/handoff data where available. Derive the view from these
sources rather than inventing a second authority for task completion.

Show deployment/host identity and health, active Rooms, snapshot reservations,
capacity, task attempts, preparation/ready/run/cleanup stages, resource use, logs,
result artifacts and measured or explicitly estimated costs. Link each displayed
result to its receipt and input revision. Show unknown and stale observations
clearly; a disconnected host must not appear healthy because its last sample was.
Only display readiness or other stages that are actually instrumented.

Build the first usable view from the saved experiment data, then connect it to a
live disposable deployment. Exercise host loss, missing receipts, duplicate and
out-of-order events, stale snapshots, failed workloads and cleanup leftovers.
Use browser end-to-end tests for these operator flows. Any later controls for
starting/stopping deployments should call existing operations and preserve their
authority, rather than putting a separate scheduler into the page.

**Deliver:** a runnable UI POC with a reproducible demo, screenshots, data-source
mapping, telemetry adapter and failure-flow tests. The useful outcome is that an
operator can find a stuck attempt, explain its state and locate the next action
without reconstructing this chat.

## How these connect

Storage measurements can inform deployment placement and capacity. Tracing can
explain why a Room is slow or failed. The UI can expose both through the same
attempt and receipt identities. They are independently useful experiments: the
UI can start with existing data while storage and tracing prototypes develop.
Keep README links, runnable examples and failure lessons alongside the code as
each experiment progresses toward a reviewable POC PR.
