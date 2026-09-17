# Experiment 2b: do clones of one snapshot share their tools' memory?

**Yes, if the base read those tools before it was snapshotted.** Six clones of a
base warmed with the Python and glibc packages held 225 MiB between them after
reading those packages again. Six clones of an unwarmed base held 1,484 MiB.
Host KSM got the unwarmed clones down to 322 MiB, at a cost in host CPU and
workload time that warming does not pay. Neither needs a change to Rooms.

## Setup

Local Lima VZ host: aarch64, 6 vCPU, 4 GiB RAM, nested KVM, Linux 6.8.0-139,
Firecracker 1.15.0. Guests: 1 vCPU, 512 MiB, File-backed restore. Sealed Python
toolstore, 179 MiB as squashfs and 538 MiB uncompressed, attached read-only over
`virtio-blk`. Rooms binary SHA-256 `ad708957…1c350`, the build measured in the
[readiness results](../readiness-results/README.md); toolstore
`85074bed…760d3`.

[`cow_sharing.py`](../../../scripts/cloud-experiments/recipes/cow_sharing.py)
forks N clones with `rooms clone`, has each run a workload and then hold, and
samples every Firecracker process's `/proc/<pid>/smaps_rollup` once a second.
Figures are the median of the last five samples in which all N VMMs were alive,
summed over VMMs. One trial per cell.

Workloads: `idle` sleeps. `scan` reads every file in the store. `subset` reads
the Python and glibc packages, about 185 MiB.

KSM is enabled with `prctl(PR_SET_MEMORY_MERGE)` in the process that launches
`rooms clone`; the jailer and Firecracker inherit it across fork and exec
(Linux 6.4 and later). `ksmd` ran at 4,000 pages per 20 ms.

## Results

Summed VMM PSS in MiB, with per-clone in brackets.

| Base | Workload | KSM | 1 clone | 2 | 4 | 6 | Wall at 6 |
|---|---|---|---:|---:|---:|---:|---:|
| cold | idle | off | 37 | 48 (24) | 70 (17) | 92 (15) | 74 s |
| cold | idle | on | 37 | 45 (23) | 61 (15) | 73 (12) | 176 s |
| cold | subset | off | 269 | 512 (256) | 998 (250) | 1,484 (247) | 81 s |
| cold | subset | on | 217 | 239 (120) | 281 (70) | 322 (54) | 102 s |
| **subset-warmed** | subset | off | 64 | 96 (48) | 160 (40) | **225 (37)** | 85 s |
| cold | scan | off | 468 | 913 (456) | 1,802 (450) | 2,697 (449) | 165 s |
| cold | scan | on | — | 536 (268) | 557 (139) | 646 (108) | 413 s |
| scan-warmed | scan | off | 469 | 913 (457) | 1,802 (450) | 2,694 (449) | 190 s |

Wall time includes the hold: 60 s for idle and scan on the cold base, 45 s
elsewhere. `ksmd` CPU at six clones: 60 s idle, 33 s subset, 151 s scan.

The cold scan rows are from the second run. In the first run four scan
conditions hit a 150 s wall limit (exit 124) before the hold ended; their memory
figures matched the second run to within 60 MiB. The single-clone KSM scan was
measured only in the first run: 414 MiB, exit 0.

## What this shows

- **Idle clones are nearly free.** Restored guest memory is a private mapping of
  one snapshot file, so untouched pages are shared page cache. Each extra idle
  clone cost about 11 MiB.
- **Reading the store after restore is what duplicates memory.** The guest fills
  its page cache with store contents, which the host sees as private dirty pages:
  about 250 MiB per clone for the subset and all of guest RAM for the full scan.
  This is experiment 2's linear growth, reproduced on a laptop.
- **Warming the base moves that cost before the snapshot.** The guest cache
  survives the snapshot (a clone of the scan-warmed base resumed with 440 MiB
  already cached), reading cached pages does not dirty them, and so they stay
  shared. Six warmed clones used 85% less than six cold ones, 30% less than KSM,
  with no `ksmd` time and no slowdown.
- **Warming only helps a working set that fits in guest RAM.** The full store is
  538 MiB and the guest has 484 MiB, so a full scan evicts and rewrites the
  cache however warm the base was. The scan-warmed row matches the cold row for
  that reason. Warm what the workload touches, not everything.
- **KSM works with no Rooms change, and costs what it is known to cost.** It
  recovered 78% of the subset duplication and 76% of the scan duplication. The
  six-clone scan ran 2.5 times longer with it, on a host where `ksmd` competed
  with six vCPUs for six cores. It also merges across tenants, which is a
  documented timing side channel. Use it only for same-tenant rooms on a host
  that is short of memory and has CPU to spare.

## A real task, not a file read

`pytask` starts the interpreter, imports fourteen standard-library packages,
byte-compiles a copy of one and runs `test.test_json`, then holds for 30 s. The
warmed base ran that same task once as its `--warm` command.

| Base | 1 clone | 2 | 4 | 6 | Private dirty at 6 |
|---|---:|---:|---:|---:|---:|
| cold | 88 | 150 (75) | 274 (68) | 397 (66) | 359 |
| warmed by running the task | 93 | 125 (62) | 188 (47) | **251 (42)** | 176 |

Warming still helps, by 37% at six clones, but less than for the file read (85%).
Warming shares what the task *reads*: interpreter, libraries, bytecode. What a
task *allocates* is private to each clone whatever the base held, and a real
task allocates. The warm command that works is the workload's own start-up, run
once; listing store paths by hand is not needed.

Two attempts before this one failed and are not in the table: the task first
tried to byte-compile inside the read-only store, then reused a temp directory
the warm user owned. Both exited non-zero within seconds. Each attempt also left
a snapshot behind, which is how the slot limit below was found.

## What this says about Nix

The duplication measured in experiment 2 does not come from Nix, and it is not
fixed by changing the delivery device. It comes from every guest populating its
own cache after restore. A pinned toolstore is known before the snapshot is
taken, which is exactly the case warming handles. `rooms base-create --warm`
already exists; the missing piece is choosing a warm command that touches the
tools a workload will use. That makes the other delivery mechanisms in
experiment 2a matter mostly for cold `rooms run`, where there is no snapshot to
inherit from.

## Limits

- One trial per cell on one laptop-class host, six clones at most. These are
  observations, not distributions, and say nothing about 64 rooms.
- A warmed base's idle clones cost more: 21 MiB each at six for the scan-warmed
  base against 15 MiB cold, because each VMM maps more resident snapshot pages.
  Idle clones of the subset-warmed base were not measured.
- Warmed pages are shared only until a guest writes to them or evicts them under
  memory pressure. A workload that fills guest RAM loses the benefit.
- KSM timing depends on the scan rate chosen here. A slower rate would cost less
  CPU and merge later; that trade was not explored.
- Every snapshot holds one of the host's eight room slots for good, and there is
  no command to retire one. Four snapshots from this work plus four older ones
  filled the pool (`pool full: all 8 slots claimed`); see follow-ups.
- File-backed restore only. UFFD backing was not measured.
- PSS attributes shared pages evenly between the VMMs that map them. Host
  `MemAvailable` deltas in the summaries track the summed PSS to within 8% for
  conditions above 500 MiB and are noise below that.

## Repeat it

```sh
R=target/release/rooms
sudo -E $R base-create --image "$IMAGE" --toolstore "$TOOLSTORE" --memory 512 --json \
  --warm 'find /nix/store/*-python3-* /nix/store/*-glibc-* -xdev -type f -exec cat {} + >/dev/null 2>&1 || true' \
  > base-receipt.json
sudo -E $R snapshot "$ROOM_ID" --out "$WARM_SNAPSHOT" --json > snapshot-receipt.json
sudo -E python3 scripts/cloud-experiments/recipes/cow_sharing.py --rooms $R \
  --snapshot "$WARM_SNAPSHOT" --image "$IMAGE" --toolstore "$TOOLSTORE" \
  --counts 1,2,4,6 --workloads subset --ksm 0 --slack 300 --out "$RESULTS"
```

Run `sudo bash scripts/setup-tap.sh --host` first if the host has rebooted. The
output directory must be new. The script leaves KSM off and unmerged when it
exits.

## Receipts

Per-run summaries: [run1](run1.summary.json) (cold base, all conditions, 150 s
limit) · [run2](run2.summary.json) (cold scan, 460 s limit) ·
[warm1](warm1.summary.json) (scan-warmed base) ·
[subset-cold](subset-cold.summary.json) · [subset-warm](subset-warm.summary.json) ·
[pytask-cold](pytask-cold.summary.json) · [pytask-warm](pytask-warm.summary.json)

[Raw evidence](evidence.tar.xz), SHA-256
`49ffc29faf18d7c8b35abf4c1dd5ca33581ab70fb056af9a9f26326262b6fe7a`: every
invocation, CLI output, host log, per-second memory samples and the two warmed
bases' receipts. It excludes VM memory, disk images and credentials. No cloud
resources or model calls were used; the final host inventory had no Rooms or
VMMs and KSM off.
