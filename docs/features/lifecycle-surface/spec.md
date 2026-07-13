**Status**: implemented
**Owner**: @michael (human:mh)
**Date**: 2026-07-12
**Dossier**: `rooms/structured-lifecycle-surface` (tsk_01KXBGZ83MS9YJ4VE62YV88QE3)
**Related**: `src/lifecycle.rs`, `src/main.rs` (`run_room_inner`), `src/runner.rs` (`wait_for_ssh_observed`), [multi-agent-execution-gate/spec.md](../multi-agent-execution-gate/spec.md)

# `rooms run --lifecycle` — a machine-readable lifecycle stream

## Goal

A supervising process driving `rooms run` today can observe exactly two
machine-readable facts: the exit code and the `--json` terminal record.
Everything between — slot claimed, VMM started, guest booted, workload running,
artifacts collected, room reaped — exists only as human log lines on stderr.
The multi-agent execution experiment showed why that's not enough: a
Firecracker `InstanceStart` was logged as "booted" while the guest kernel
panicked less than a second later. A consumer that collapses VMM-started /
guest-ready / workload-started into one `running` state cannot tell a real
failure from a slow boot.

`rooms run --lifecycle <path>` appends a machine-readable NDJSON stream — one
event per line — to a host path the caller selects, distinguishing every
externally visible transition of the run.

## Design

### Stream contract

- **Envelope** (every line): `seq` (monotonic from 1, contiguous), `ts`
  (RFC 3339), `room_id`, `event` (the kind tag), plus kind-specific fields.
- **Durability**: each line is flushed and fsynced before the run proceeds to
  the next externally visible transition — stdout is never the only copy, and
  a crash never loses an already-visible event.
- **Per-run**: the file is created (truncating a stale one) before the slot
  claim, so an unwritable path fails fast without leaking a claim, and an
  admission rejection is already recordable.
- **Never load-bearing**: after creation, a write failure is logged and the
  run continues; observation cannot break a workload.

### Events

| Event | Fields | Meaning |
| --- | --- | --- |
| `slot_allocated` | `slot`, `tap` | pool slot claimed; the room owns its /30 |
| `pool_full` | `cap` | admission rejected: every slot up to the cap is claimed |
| `vmm_started` | `pid` | firecracker up, API answered, instance started — **not** readiness |
| `boot_failed` | `error` | boot never reached a started VMM |
| `guest_ready` | — | guest kernel is up (answered ICMP; or proven via SSH, below) |
| `ssh_ready` | — | sshd accepted a pubkey connection; workload channel usable |
| `guest_unreachable` | `error` | guest never became usable within the reach timeout |
| `workload_started` | `command` | workload handed to the guest |
| `workload_exited` | `exit_code`, `status` | workload finished or was aborted; `status` uses `result.json`'s vocabulary (`succeeded` / `failed` / `timed_out` / `cancelled`) |
| `workload_failed` | `error` | exec machinery failed; when the workload finished first and a post-run step failed (e.g. `--push-branch`), a `workload_exited` with the real exit precedes this |
| `collection_started` / `collection_done` / `collection_failed` | `error` on failure | `--out` artifact collection |
| `cleanup_done` / `cleanup_failed` | `error` on failure | teardown outcome (`--keep` records neither: cleanup is suppressed, not done). `cleanup_done` is **verified** against on-disk residue (room dir, jail dir, slot claim), never inferred from teardown returning — a partial teardown that parks the room for `rooms gc` reports `cleanup_failed` |

Terminal failures are **distinct kinds** — admission (`pool_full`), boot
(`boot_failed`), readiness (`guest_unreachable`), workload (`workload_failed`
/ nonzero `workload_exited`), collection (`collection_failed`), cleanup
(`cleanup_failed`) — so a consumer branches on the tag, never on a message
string. Events are rooms-native: rooms does not know any consumer's phase
vocabulary; a consumer maps these onto its own state machine.

### `guest_ready` vs `ssh_ready` (the boot≠ready fix)

While waiting for sshd, each poll cycle also pings the guest (`ping -c 1 -W 1`)
until it first answers — the kernel-is-up signal that is earlier than and
independent of sshd. A guest that panics after `InstanceStart` never brings its
network up, so `guest_ready` is never emitted — the panic is distinguishable
from a slow boot by the stream alone. When ICMP never answers but SSH succeeds,
an accepted connection proves the kernel booted, so `guest_ready` is emitted
immediately before `ssh_ready`: the ordering guarantee (guest before ssh,
each exactly once on the success path) holds either way. The ping probe runs
only when a stream is attached; runs without `--lifecycle` keep the exact
probe behavior they had. One deadline (`guest_reach_timeout`) bounds the whole
wait.

### Ordering caveats

- `workload_exited` may appear **without** a prior `workload_started` when
  Ctrl-C or the `--max-wall` cap fires during the readiness wait — mirroring
  the `result.json` an aborted run records (`cancelled` / `timed_out`). On an
  abort the event is emitted the instant the outcome is decided, before the
  best-effort (grace-bounded) `result.json` write to the possibly-unresponsive
  guest.
- A `--keep` run ends without a cleanup event, by design.
- Only `pool_full` is a structured admission rejection. Any other slot-claim
  error (e.g. a reserve-by-index collision) leaves an **empty stream** and a
  generic exit 2 — the `--json` terminal record is the fallback surface there.
- A write failure after the stream is created never advances `seq`, so the
  on-disk sequence stays contiguous; the failed event itself is lost (logged
  on stderr) rather than blocking the run.

## Non-goals

- **No consumer contract types.** Rooms emits rooms-native events; the
  workbench adapter translates them to its own phases. No `execution` types
  in this repo.
- **No deadline policy.** The authoritative deadline lives with the caller;
  `--max-wall` remains the substrate-level cap it already was.
- **No daemon, queue, or scheduler.** The stream is a per-run file, nothing
  more.

## Acceptance

- [x] `rooms run --lifecycle <path>` emits contiguous NDJSON (seq from 1)
      distinguishing allocation, VMM start, guest/SSH readiness, workload
      start/exit, collection, and cleanup.
- [x] Structured `pool_full` on an N+1 admission rejection.
- [x] `guest_ready` / `ssh_ready` distinct from `vmm_started`; a guest that
      panics after `InstanceStart` never emits `guest_ready`.
- [x] Distinct terminal outcomes distinguishable by tag alone.
- [x] `make check` green; host-gated e2e (`tests/lifecycle_e2e.rs`) asserts
      the stream on a real boot and the `pool_full` path.
