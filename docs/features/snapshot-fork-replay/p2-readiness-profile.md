# Phase-2 gate: where the 43 seconds go

Profile of the `fleet_not_under_one_second` performance failure on task 2d
(`p2-validation-gate`). The gate requires eight clones to reach authenticated
readiness in **< 1 s**; the last two host runs measured **43.0 s** and **48.6 s**.

This doc decomposes that number. It changes nothing in `src/`.

> **Update 2026-09-02** — the settling experiment was run; see
> "Measured on 2026-09-02" below. The working hypothesis in the earlier
> sections (exec path / small-file writes) is superseded: the cost is the
> nested-virtualization stage-2 page fault, and a THP-backed `snapshot.mem`
> cuts fleet readiness ~3× with no code change.

## Where the data came from

No new instrumentation was needed. `scripts/phase2-killer.sh:99` already pins

```
ROOMS_PROOF_RUST_LOG="info,rooms::vsock=debug"
```

and `src/vsock.rs` already stamps every resume-protocol stage transition with a
per-room `elapsed_ms` (`record_resume_step` / `expect_resume_step`, both taking
`connected_at`). Every proof run has therefore been writing a full per-clone,
per-stage timeline into `$PROOF_ROOT/logs/fleet.stderr` all along.

Runs read (on the rooms-host, under `~/.r2/`):

| Run | Head | `fleet-ready.ns` |
| --- | --- | --- |
| `zA3P` | `52a361c` | 48 607 564 485 |
| `FMWT` | `3cdde4e` | 43 003 705 389 |

## The decomposition

**Δ is the median across the eight clones**, so these columns describe the
median clone's timeline — they are not additive with the fleet boundaries in the
next table. See "Reading the tables together" below.

| Stage | FMWT Δ | zA3P Δ | What it does |
| --- | --- | --- | --- |
| `reseeded` | 0.69 s | 0.20 s | `RNDRESEEDCRNG` ioctl |
| `identity` | 2.82 s | 2.72 s | write identity + secrets + git config |
| **`hostkeys`** | **14.27 s** | **12.79 s** | `ssh-keygen -t ed25519` + chmods + config parse |
| `privilege` | 4.03 s | 6.12 s | restore sudo (file writes) |
| `clock` | 0.06 s | 0.07 s | 5 vsock round-trips + `clock_settime` |
| `sshd` | 1.70 s | 1.90 s | launch sshd |
| **sum — median clone done** | **23.56 s** | **23.81 s** | |

### Fleet accounting, on a single clock

The per-stage `elapsed_ms` above is measured from each room's **vsock accept**
(`src/vsock.rs:567`), while the gate's own interval starts before the CLI is even
invoked (`scripts/phase2-killer.sh:3582`). Subtracting one from the other would
mix two time origins. The log carries wall-clock timestamps on every line and
spans the whole measured window, so the fleet accounting below is instead built
on **one clock**, `t=0` at CLI start, with every boundary taken as the *last* of
the eight clones to cross it.

| Boundary (all 8 across by) | FMWT | zA3P |
| --- | --- | --- |
| microVMs resumed from snapshot | 0.47 s | 0.53 s |
| vsock agents connected | 2.62 s | 3.88 s |
| guest-hygiene ACKs | 27.39 s | 30.65 s |
| SSH authenticated | 42.92 s | 48.53 s |
| CLI returned (`fleet-ready.ns`) | 43.00 s | 48.61 s |

| Segment | FMWT | zA3P | share |
| --- | --- | --- | --- |
| restore + boot to vsock | 2.62 s | 3.88 s | 6–8 % |
| **guest post-resume hygiene** | **24.77 s** | **26.76 s** | **55–58 %** |
| **post-sshd SSH handshake** | **15.53 s** | **17.88 s** | **36–37 %** |
| CLI return overhead | 0.08 s | 0.07 s | 0.2 % |

### Snapshot restore is not the problem

**All eight microVMs resume from the snapshot within half a second** — 0.47 s
(FMWT) and 0.53 s (zA3P), with the eight `firecracker` spawns landing inside a
10 ms window. Restore and boot to a connected vsock agent is 6–8 % of the budget.

This is worth stating plainly because it is the density thesis working: the fork
itself is fast, and nothing in the 43 s is waiting on snapshot machinery. The
entire miss is post-resume — hygiene plus handshake are **~93 %** of the wall
clock.

### Reading the tables together

The per-stage Δs are medians and sum to the **median** clone's completion
(23.56 s / 23.81 s); the fleet table is built from **maxima**, because the CLI
cannot return until the *last* clone authenticates. Those two are different
statistics and are not additive with each other — the 2.86 s / 3.78 s difference
between the median clone's finish and the slowest clone's ACK is exactly that,
not an unaccounted stage.

That gap is itself a finding. The eight clones do not finish together:

| Run | per-clone hygiene completion (s) | spread |
| --- | --- | --- |
| FMWT | 19.4, 19.9, 20.1, 22.8, 24.3, 25.8, 26.1, 26.4 | 7.01 s |
| zA3P | 22.6, 22.6, 22.7, 23.2, 24.4, 24.5, 27.6, 27.6 | 4.99 s |

A 5–7 s spread across eight clones doing identical work, launched together, is
what contention looks like — and it is consistent with the shape argument below.

Two costs dominate and together account for ~93 % of the wall clock: guest
post-resume hygiene (55–58 %, with `hostkeys` the largest stage inside it) and
the post-sshd SSH handshake (36–37 %).

## What this rules out

**It is not the protocol, and it is not vsock.** The `clock` stage carries the
most host round-trips of any stage — READY / CLOCK+challenge / APPLIED /
CONTINUE+challenge / STEP, five exchanges — and costs **60–72 ms**. Transport
and handshake design are three orders of magnitude away from being the problem.

**It is not the fan-out.** Both phases are already properly concurrent:
`restore_clone_batch` (`src/main.rs:2841`) spawns all eight restores into a
`JoinSet`, and `run_clone_readiness_barrier` (`src/main.rs:3074`) does the same
for the eight SSH probes. Nothing is serialized at the orchestration layer.

**It is not the poll grid.** `wait_for_ssh_observed` probes immediately and only
then sleeps `guest_reach_poll_interval` (2 s). A 2 s grid cannot produce a
15–18 s handshake segment.

**It is probably not entropy.** `reseed()` uses `RNDRESEEDCRNG` on an
already-initialized CRNG (the base booted normally before the snapshot), so
`getrandom()` in `ssh-keygen` should not block — and `reseeded` itself completes
in 0.2–0.7 s. Worth one direct check before closing this out entirely.

## What the shape points at

Sort the stages by what they actually do:

- pure syscall / vsock work — `clock` (60 ms), `reseeded` (0.2–0.7 s) → **fast**
- fork/exec + file writes — `identity`, `hostkeys`, `privilege` → **seconds each**

`hostkeys` is the most write-heavy stage in the sequence (`clear_host_keys`
unlinks, `ssh-keygen` forks and writes two files, then four `chmod`s and four
`stat` validations — `scripts/lib/rooms-resume-apply.c:1067`), and it is the
most expensive. An ed25519 keygen is ~1 ms of arithmetic; 14 s means the cost is
not the crypto.

The same explanation covers the 15–18 s handshake segment: sshd forks a child
per connection and reads its host keys off disk, so an accept path slow for the
same reason would show up exactly where it does. That the two largest costs are
the two most fork- and write-heavy segments, while restore — which is neither —
finishes in half a second, is the whole of the argument.

**Working hypothesis: guest-side `fork`/`exec` and small-file writes are the
bottleneck under eight-way concurrency** — a storage/exec path cost, not a
design flaw in the resume protocol. Note that all eight clones point at a single
shared `prepared.image` (`src/restore_exec.rs:465`), and the restore path does
not thread `readonly_rootfs` the way the boot path does
(`src/firecracker.rs:658`) — the restored device model comes from the snapshot's
own state. That asymmetry is worth confirming.

## The experiment that settles it

One run, on the host, timing three things inside a single restored clone with no
other clones present, then again with eight:

1. `time ssh-keygen -q -t ed25519 -N "" -f /tmp/k` — isolates exec + write.
2. `time dd if=/dev/zero of=/tmp/probe bs=4k count=256 conv=fsync` — isolates
   small-file write latency.
3. `time sh -c 'for i in $(seq 50); do /bin/true; done'` — isolates fork/exec.

If (1) goes from milliseconds at N=1 to seconds at N=8, the hypothesis holds and
the fix is in the storage path, not the protocol. If (1) is slow even at N=1,
the cost is in the restored guest's device model instead.

## Measured on 2026-09-02: it is the stage-2 fault path

The experiment above was run on the Lima rooms-host (6 vCPU / 8 GiB, kernel
6.8, Firecracker v1.15.0) against the retained `FMWT` snapshot, driven through
`rooms clone --command` so every number below comes from the shipped resume
protocol and the same `elapsed_ms` stamps as the tables above. It settles the
question, but not the way the working hypothesis expected.

### Scaling sweep

Hygiene ACK of the *last* clone, and the `hostkeys` range, with `--command true`:

| n | last ACK | `hostkeys` | CLI wall |
| --- | --- | --- | --- |
| 1 | 1.79 s | 1.30 s | 6.2 s |
| 1 | 2.24 s | 1.64 s | 8.1 s |
| 2 | 5.22 s | 4.00–4.10 s | 11.7 s |
| 4 | 9.01 s | 5.47–5.54 s | 29.6 s |
| 8 | 31.35 s | 20.3–28.4 s | 81.6 s |

Two clones on six host cores are not competing for CPU, yet n=2 is already
2.7× n=1, and n=8 is ~15× n=1. That is a serialized shared resource, not a
scheduler.

### Microbenchmark inside one restored clone (n=1)

Each line is a `sh -c` timed from `/proc/uptime`; the second run of each pair
repeats it in the same guest.

| probe | first | second | what it isolates |
| --- | --- | --- | --- |
| shell loop, 200k iterations | 0.99 s | 0.77 s | pure compute, no faults |
| `dd` 100 MiB into tmpfs | **31.43 s** | 13.09 s | first touch of fresh guest pages |
| 200 × `/bin/true` | 2.10 s | 0.90 s | fork/exec |
| `ssh-keygen -t ed25519` | 0.24 s | 0.13 s | the whole "expensive" keygen, warm |
| `cat /usr/lib/*.so* /usr/bin/*` | 7.51 s | 4.90 s | block reads + page-cache fill |

Compute is normal. Touching 25,600 fresh 4 KiB pages costs 31 s — **~1.2 ms per
page**. The same 1 GiB touch on the Lima host itself costs 3.3 s (~13 µs/page),
so the cost is not the host's memory: it is the guest's **stage-2 page fault**
under nested virtualization (Firecracker/KVM inside a `vz` VM). A warm
`ssh-keygen` is 0.1 s; the 1.3 s `hostkeys` stage at n=1 and the 14 s at n=8 are
that keygen's first-touch pages, and the SSH handshake segment is `sshd`'s.
Storage is not involved: the overlay is tmpfs, so `fsync` is a no-op there.

This replaces the working hypothesis above. It is not the exec path or small-file
writes; it is every first access to a guest-physical page after resume, and
those faults appear to serialize across clones in the L0 hypervisor (`vz`), below
the rooms-host kernel and its KVM module.

### A zero-code mitigation: 2 MiB read mappings

Every clone `MAP_PRIVATE`s the same `snapshot.mem` (§4 D6). On a regular file
that mapping can only ever be 4 KiB pages, so KVM maps stage-2 at 4 KiB and
pays one nested fault per page. If the file lives on **tmpfs mounted
`huge=always`**, read faults populate a 2 MiB PMD and KVM/arm64 upgrades the
stage-2 mapping to a 2 MiB block (`transparent_hugepage_adjust`) — 512× fewer
read faults. Writes still CoW at 4 KiB (the PMD is split on first write), so
this only helps the read side. Setting `chattr +i` works on tmpfs since 6.0, so
the inode-seal check is satisfied unchanged.

Bind-mounting a `huge=always` tmpfs copy of the snapshot directory over
`~/.r2/FMWT/snapshot` and rerunning, nothing else changed:

| n | last ACK (4 KiB file) | last ACK (THP tmpfs) | `hostkeys` (THP) |
| --- | --- | --- | --- |
| 1 | 1.79–2.24 s | 1.34–1.46 s | 0.93–1.00 s |
| 4 | 9.01 s | 5.38 s | 3.45–3.55 s |
| 8 | 28.7–31.4 s | 9.6–11.6 s (three runs) | 6.1–10.4 s |

`/proc/vmstat` confirms the mechanism: `thp_file_mapped` went 0 → 752 and
`thp_split_pmd` 0 → 519 across the runs. In the n=1 microbenchmark the fresh-page
touch dropped from 31.4 s to 6.8 s and the page-cache fill from 7.5 s to 2.3 s.

Fleet readiness improves ~3× from a mount option. It does not reach the gate:
the remaining ~10 s is write faults at 4 KiB granularity, and a fleet-wide
`< 1 s` bound is roughly another 10× away.

### What would close the rest

- **hugetlbfs-backed `snapshot.mem`.** A `MAP_PRIVATE` mapping of a hugetlbfs
  file makes the *write* CoW copy a whole 2 MiB page in one fault, so a clone
  bounds at 128 faults for all 256 MiB instead of up to 65,536. Not yet
  measured: hugetlbfs has no inode flags, so the current seal check refuses it
  outright. Trying it needs either a seal policy that accepts hugetlbfs on its
  own terms (the mount is already root-only and non-writable through `write(2)`)
  or an explicit experiment flag — an operator decision, not a tuning knob.
  The cost is the density thesis: 2 MiB CoW granularity inflates per-clone
  private memory, which is the other open gate bound (fleet PSS).
- **Fewer distinct pages touched post-resume.** Faults scale with the working
  set, not the guest size. Everything `hostkeys`/`sshd` need that can be
  resident and *read-only* in the snapshot benefits from THP; every page the
  guest must *write* after resume is a 4 KiB fault regardless.
- **A host that is not nested.** The ~1 ms/fault is the nested-virtualization
  tax on this development host; the same code on bare-metal KVM should see
  faults two to three orders of magnitude cheaper. The gate number is host-
  specific and should say so.

Productizing the THP win is small: stage snapshot artifacts on a `huge=always`
tmpfs (they are memory-resident across eight clones anyway through the page
cache), or have the restore preflight warn when `snapshot.mem` is not
THP-backed. Either belongs in a separate change.

## Consequences for the gate

The `< 1 s` target is a **fleet-wide authenticated-readiness** bound, and today
~93 % of the budget is spent in guest-side work that has nothing to do with fork
speed — the fork itself completes in under half a second. Two implications worth
an explicit decision:

- **`hostkeys` is on the critical path by construction.** Fresh per-clone host
  keys are a *correctness* requirement of the gate (distinct sshd identity per
  clone), so it cannot simply be deleted — but it does not have to sit between
  resume and ingress. Generating the key material *before* the readiness barrier
  releases, or pre-staging a distinct key per pool slot, would take it off the
  measured path without weakening the identity property.
- **The barrier measures more than fork.** If the intent of `< 1 s` is "a fork is
  fast", the current measurement window (CLI entry → all eight authenticated)
  bundles in provisioning work that a warm pool would have already done. Whether
  the target or the window is what needs adjusting is an operator call, not an
  implementation detail — but it should be made deliberately rather than by
  tuning until the number passes.

Neither of the other two gate blockers is touched by this analysis: fleet PSS
(~154 MiB vs a ~115 MiB bound) and the eight independently dispatched
`/work-driver` tasks remain open on their own terms.
