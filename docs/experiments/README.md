# Rooms experiments: what we learned and what is next

Start here. Each experiment has its own write-up with the numbers, the machine,
the limits and raw receipts; this page is the index and the lessons that outlived
the numbers. Measurements are small and local unless a row says otherwise. None
of them is a capacity claim.

## Where things are

| What | Where |
|---|---|
| The original ten cloud experiments, status and remaining work | [ten-experiments.md](ten-experiments.md), results in [cloud-results/](cloud-results/README.md) |
| Experiments 11–13 (storage, eBPF tracing, deployment UI), planned | [storage-observability-deployments.md](storage-observability-deployments.md) |
| Experiments 2a–2d and 14–22, questions and prototypes | [next-experiments.md](next-experiments.md) |
| How to rerun a clone workload and what to retain | [cloud-lab.md](cloud-lab.md) |
| Substrate bugs found while experimenting | [../follow-ups.md](../follow-ups.md) |

## Results so far

| # | Question | Answer | Write-up |
|---|---|---|---|
| 1, 3 | When is a restored room usable? | Resume acknowledgement comes well before usable SSH: 0.7 s and 1.2 s for one clone, 2.6 s and 4.6 s at four. Measure both. | [readiness-results](readiness-results/README.md) |
| 2b | Do clones share their tools' memory? | Yes, if the base read the tools before the snapshot. Six clones: 1,484 MiB cold, 322 MiB with host KSM, 225 MiB warmed. A real Python task gains less (397 to 251 MiB) because what a task allocates is always private. | [cow-sharing-results](cow-sharing-results/README.md) |
| 2d, 17 | What does it cost to ship a snapshot? | A cold snapshot is 89% zeros: 58 MB to send of 537 MB. Related snapshots share only 20–24 MB at 64 KiB chunks. A warmed base is up to six times larger to ship. | [chunkstore-results](chunkstore-results/real-snapshots/README.md) |
| 15 | Can peers in separate Rooms coordinate? | Yes, through Redis on the host: 240 of 240 tasks accepted exactly once at 2, 4 and 6 clones, including with a clone killed and the store restarted mid-run. About 4 ms per claim. | [lease-plane-in-rooms-results](lease-plane-in-rooms-results/README.md) |
| 15, 21 | File store or networked store? | On one machine a directory is fine. Across a VM boundary it fell to 41 tasks/s while Redis held about 530. A model check found that a fencing token alone does not prevent double acceptance. | [lease-plane-results](lease-plane-results/README.md) |

## Lessons

**About Rooms**

- **Nix is not the memory problem.** Experiment 2's linear memory growth comes
  from every guest filling its own page cache after restore, whatever delivers
  the files. Warm the base with the workload's own start-up, run once;
  `base-create --warm` already does it.
- **Warm what fits.** A 538 MiB store cannot be warmed into a 512 MiB guest; a
  full scan thrashes the cache and the warmed base looks useless.
- **Warming and travelling pull in opposite directions.** Warm bases save host
  memory and cost wire bytes. Ship the cold base and warm on arrival.
- **KSM works with no Rooms change, and costs what it is known to cost.** It
  recovered 76–78% of duplicated memory, made a six-clone scan 2.5 times slower,
  and merges across tenants. Same-tenant hosts short of memory only.
- **Skipping zero pages is most of the transfer win.** Chunk-level dedup between
  snapshots found little, and content-defined chunking changed it by under 3%.
- **Clones are indistinguishable from outside.** They share a frozen hostname and
  address. Identity has to be minted after restore.
- **Clones share no filesystem.** Anything that coordinates them is a network
  service, or nothing.

**About measuring**

- **A workload that does not fit invalidates the comparison.** The first
  warmed-base test showed no benefit because the workload was wrong, not the idea.
- **Failed attempts teach the substrate.** Three failed warm attempts exposed that
  snapshots hold room slots for good. Keep the failures in the write-up.
- **Wall limits that are too tight look like regressions.** Four timeouts in the
  first 2b run were the harness, and the rerun separated that from KSM's real
  slowdown.
- **Run the real server.** A fake Redis passed every test and lost 40% of its
  throughput at 64 peers; real Redis did not. Running as root in a container
  exposed an outage injector that did nothing.
- **Trigger faults on progress, not on a timer.** A fixed eight-second fault
  landed after the work had finished at four and six clones.
- **A passing summary with no work done is a failure.** Exit codes and pass flags
  need "and the work happened" in them.

## Substrate findings

Recorded in [follow-ups](../follow-ups.md): a guest can reach services on the
host's own LAN address; `rooms kill` of one clone leaves `rooms clone --command`
waiting for the full wall limit; a snapshot holds one of eight room slots for
good and nothing retires it.

## What is next

1. **Local, free:** 2a narrowed to cold `rooms run` (toolstore against tools baked
   into the image); 22 prewarmed clone pool, using the 2b memory figures; 21 the
   seeded fault schedules run inside Rooms; 14 Fleet seats as Rooms.
2. **Needs agent time, no cloud spend:** 2c which toolchain format an agent edits
   correctly; 16 speculative swarm; 4 and 8 from the original ten.
3. **Needs cloud hosts:** timed cross-host transfer for 2d and 17; 18 work
   stealing and failover; 2a and 2b at 16, 32 and 64 rooms. Rehearse locally
   first so host-hours go to measuring.
4. **Blocked on hardware:** bare-metal legs of 1 and 3; GPU Room (7) needs a host
   that exposes an IOMMU.

## Sharing

The repository is public, so a link to this page is the simplest thing to send.
Every write-up states its machine, workload and limits; quote a number only with
those beside it.
