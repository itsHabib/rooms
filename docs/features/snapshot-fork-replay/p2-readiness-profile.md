# Phase-2 gate: where the 43 seconds go

Profile of the `fleet_not_under_one_second` performance failure on task 2d
(`p2-validation-gate`). The gate requires eight clones to reach authenticated
readiness in **< 1 s**; the last two host runs measured **43.0 s** and **48.6 s**.

This doc decomposes that number. It changes nothing in `src/`.

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
next table. See "Reading the two tables together" below.

| Stage | FMWT Δ | zA3P Δ | What it does |
| --- | --- | --- | --- |
| `reseeded` | 0.69 s | 0.20 s | `RNDRESEEDCRNG` ioctl |
| `identity` | 2.82 s | 2.72 s | write identity + secrets + git config |
| **`hostkeys`** | **14.27 s** | **12.79 s** | `ssh-keygen -t ed25519` + chmods + config parse |
| `privilege` | 4.03 s | 6.12 s | restore sudo (file writes) |
| `clock` | 0.06 s | 0.07 s | 5 vsock round-trips + `clock_settime` |
| `sshd` | 1.70 s | 1.90 s | launch sshd |
| **sum — median clone done** | **23.56 s** | **23.81 s** | |

Fleet-level accounting, which is what the gate actually bounds:

| Boundary | FMWT | zA3P |
| --- | --- | --- |
| median clone finishes hygiene | 23.56 s | 23.81 s |
| **slowest** clone finishes hygiene (last ACK) | 26.42 s | 27.59 s |
| **SSH readiness barrier tail** | **16.58 s** | **21.02 s** |
| **total (`fleet-ready.ns`)** | **43.00 s** | **48.61 s** |

### Reading the two tables together

The stage Δs sum to the **median** clone's completion (23.56 s / 23.81 s), while
the barrier tail is measured from the **slowest** clone's ACK (26.42 s /
27.59 s). The 2.86 s / 3.78 s difference between those two rows is entirely
median-vs-max — it is not an unaccounted stage. The tail is measured from the
max because the CLI cannot return until the *last* clone authenticates.

That gap is itself a finding. The eight clones do not finish together:

| Run | per-clone hygiene completion (s) | spread |
| --- | --- | --- |
| FMWT | 19.4, 19.9, 20.1, 22.8, 24.3, 25.8, 26.1, 26.4 | 7.01 s |
| zA3P | 22.6, 22.6, 22.7, 23.2, 24.4, 24.5, 27.6, 27.6 | 4.99 s |

A 5–7 s spread across eight clones doing identical work, launched together, is
what contention looks like — and it is consistent with the shape argument below.

Two costs dominate and together account for ~70 % of the wall clock:
`hostkeys` (~13–14 s) and the SSH barrier tail (~17–21 s).

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
then sleeps `guest_reach_poll_interval` (2 s). A 2 s grid cannot produce a 17 s
tail.

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

The same explanation covers the barrier tail: sshd forks a child per connection
and reads its host keys off disk, so an accept path that is slow for the same
reason would show up exactly where it does.

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

## Consequences for the gate

The `< 1 s` target is a **fleet-wide authenticated-readiness** bound, and today
~70 % of the budget is spent in guest-side work that has nothing to do with fork
speed. Two implications worth an explicit decision:

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
