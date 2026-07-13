# Multi-agent Rooms experiment: 2026-07-09

## Status

- Stage 1 host rebuild and qualification: passed
- Stage 2 concurrent CLI execution: passed
- Stage 3 concurrent Ship agents: passed
- Stage 4 execution-contract capture: completed below

## Environment

| Field | Value |
| --- | --- |
| Windows host | `HA-D3TLRJ4` |
| Rooms host | `mh@172.21.244.109` |
| Host kernel | Ubuntu `6.8.0-124-generic` |
| Firecracker | `v1.10.1` under jailer |
| Rooms | current checkout plus the fixes recorded below |
| Guest | Alpine 3.21.7, 512 MiB ext4, key-paired `rooms` user |
| Pool | slots 1-8 available; `172.16.0.0/24` split into per-slot `/30` networks |

The VM was recreated from `C:\Hyper-V\rooms-host\os.vhdx` using
`scripts/provision-hyperv-auto.ps1 -Force`. The pristine base VHDX was not
booted or modified.

## Host qualification

The fresh host exposed two reproducibility failures before Firecracker testing:

1. `setup-rooms-host.sh` invoked a downloaded rustup script using the
   pipe-to-shell-only `-s --` argument sequence. Current rustup rejected both
   `-s` and, after a partial correction, `-y`. The correct downloaded-file form
   is `sh "$rustup_tmp" -y --default-toolchain ...`.
2. `sha_drift_reports_ok_when_no_artifacts_present` resolved kernel, rootfs, and
   jailer from the real host. It therefore failed after setup installed those
   artifacts. The test now injects an empty target resolver without mutating
   process-global `HOME` or `PATH`.
3. The setup script installed Node 20 while the current Ship workspace requires
   Node 22. The host default and NodeSource checksum pin now target Node 22; the
   rebuilt host runs Node `v22.23.1`.
4. A non-login SSH shell did not put rustup's cargo directory on `PATH`, causing
   an idempotent setup rerun to attempt a second Rust installation. Setup now
   sources `~/.cargo/env` before testing for cargo.
5. A checksum file copied from Windows carried CRLF endings, making the shell
   checksum lookup compare artifact names with a trailing carriage return. The
   lookup now normalizes that character.

After those fixes:

- `make check`: passed
- Rust library tests: 152 passed
- CLI tests: 32 passed
- integration and preflight tests: passed
- clippy with all targets and features: passed
- formatting: passed

The Firecracker E2E harness passed all three host tests:

- single-room boot and byte-identical reap
- three concurrent room boots with distinct slots
- bounded-pool rejection with the machine-readable `pool_full` path

With the key-paired agent image promoted to the canonical
`~/rooms/images/rootfs.ext4`, all three guests resolved `github.com` through
NAT and `ROOMS_FWD`. The behavioral guest-to-guest isolation probe ran, and the
harness reported zero host leaks.

## Concurrent CLI proof

Three independent top-level `rooms run --command` processes started together.
Each wrote a unique token and hostname into `/workspace/out`, slept for ten
seconds to force overlap, and collected its artifacts into a distinct host
directory.

Live state during overlap:

| Run | Room ID | Slot | Tap | Guest IP |
| --- | --- | ---: | --- | --- |
| 1 | `01kx5c5fzz9t812s2enqv07yft` | 1 | `tap-fc1` | `172.16.0.6` |
| 2 | `01kx5c5fzzrq6h93m0e0tv1x6b` | 2 | `tap-fc2` | `172.16.0.10` |
| 3 | `01kx5c5fzzptg0jk7at9fw2avj` | 3 | `tap-fc3` | `172.16.0.14` |

All three Firecracker processes were live simultaneously. Their guest commands
started within 66 ms of one another and completed successfully after the
intentional sleep.

Results:

- each collected `token.txt` matched only its invocation's expected token
- each collected `hostname.txt` contained `rooms-agent`
- each `result.json` reported `status: succeeded` and `exit_code: 0`
- final `rooms ls --json` returned an empty list
- final Firecracker process count was zero

### Post-reboot live snapshot

After a Windows restart changed the Hyper-V DHCP address, the host auto-started
cleanly. A second N=3 readonly run captured the previously omitted live process
and memory evidence while all three commands overlapped:

```text
room 01kx6k1hkd6ns77vscpqzedy6x  slot=3  tap=tap-fc3  guest=172.16.0.14  pid=54303
room 01kx6k1hkdf0p6bwgaseecz44x  slot=2  tap=tap-fc2  guest=172.16.0.10  pid=54304
room 01kx6k1hkd8wbbrwj9vqwgke6w  slot=1  tap=tap-fc1  guest=172.16.0.6   pid=54311

pid=54303  rss=56,788 KiB
pid=54304  rss=56,720 KiB
pid=54311  rss=56,712 KiB
total_rss=170,220 KiB  mean_rss=56,740 KiB
```

All three output tokens matched their invocation. The terminal snapshot again
showed an empty room registry and zero Firecracker processes.

The first pass used `--readonly-rootfs` and found a production-path failure.
The rootfs journal had been dirtied by an earlier read-write E2E boot. Because
Firecracker correctly exposed the drive as read-only, ext4 could not replay the
journal and the kernel panicked before `overlay-init` ran. Readonly boot args now
include `rootflags=noload`. A control run against the same dirty image then:

- reached SSH
- completed the command
- collected the output
- reported `overlay_active: true`
- captured an ephemeral `/tmp` write in `changeset.json`
- reaped the room and Firecracker process

This fix matters directly to Stage 3 because the Cursor runner automatically
enables readonly rootfs mode.

## Ship and Cursor image readiness

Ship was cloned to `~/dev/ship`, installed with pinned pnpm `10.13.1`, and its
CLI starts successfully under Node 22.

The first Cursor image build also found a real capacity boundary. The base
512 MiB image cannot temporarily hold Node, npm, Python, GCC, headers, and the
native SQLite build used by `@cursor/sdk`. Rebuilding with `--size 1G` passed;
the extension removed the compiler toolchain afterward and the final sparse
image occupies about 440 MiB on disk.

The exact Stage 3 image is:

```text
/home/mh/rooms/images/agent-alpine-cursor.ext4
sha256 16ce6e5dca8ebbc60b08bd5a2ef0d805f459c93c37158eb6ee06ec11304719b6
```

A no-token readonly Firecracker smoke verified Node `v22.23.0`, the existence
and syntax of `/opt/rooms/cursor-runner/cursor-runner.js`, overlay change-set
collection, clean room teardown, and zero Firecracker residue.

GitHub authentication was forwarded ephemerally from the Windows `gh` keyring.
The Cursor key was loaded from a mode-`600` operator-owned file on `rooms-host`;
neither secret was printed or written into an experiment artifact.

## Concurrent Ship proof

Two independent Ship CLI processes started together against
`https://github.com/itsHabib/agent-sandbox`. Both used the pinned Cursor image,
`composer-2.5` with `fast=true`, distinct task docs, and distinct push branches.

Live overlap evidence:

| Job | Workflow | Room | Slot | Guest IP | Branch |
| --- | --- | --- | ---: | --- | --- |
| alpha | `wf_01KX6ED7Q3KRCGGPR594RS9HJQ` | `01kx6ed7rze1sba5fm5sqy3rvz` | 2 | `172.16.0.10` | `rooms/gate-alpha-20260710-164008` |
| beta | `wf_01KX6ED7Q0C5MYR03EZNYW3GPW` | `01kx6ed7ryd6qs6jm7n2c2766h` | 1 | `172.16.0.6` | `rooms/gate-beta-20260710-164008` |

Both Firecracker processes started within the same millisecond and remained
live together until alpha completed.

Terminal results:

| Job | Status | Agent duration | Commit | Verified remote diff |
| --- | --- | ---: | --- | --- |
| alpha | succeeded | 29,532 ms | `97851670fbb4d8b70a1c9b637849797c7d9ad75a` | one added README line, no other files |
| beta | succeeded | 159,920 ms | `0c843a4a3b325e22e3064b0c1a391156ebe695f7` | one added README line, no other files |

Ship returned a terminal Rooms result and structured summary for each run. Both
branches exist on GitHub exactly one commit ahead of `main`. Each room collected
`events.ndjson`, logs, `result.json`, summary, and overlay changes before
teardown. Final `rooms ls --json` was empty and the Firecracker process count
was zero.

### Ordered event evidence

Alpha preserved 255 Cursor event records. The boundary ordering, combining the
Rooms lifecycle log with the first and last durable agent events, was:

```text
16:40:10.268  Rooms invocation accepted
16:40:10.283  Firecracker process spawned
16:40:10.356  network attached (tap-fc2, 172.16.0.10)
16:40:10.388  Firecracker InstanceStart accepted
16:40:13.585  guest SSH ready
16:40:19.704  Cursor status RUNNING
16:40:21.689  first retained thinking event
...           tool calls, thinking, and assistant events
16:40:44.014  Cursor status FINISHED
16:40:44.199  Cursor result event
16:40:46.911  /workspace/out collected
16:40:47.089  overlay changes collected
```

This is evidence for two streams, not yet one protocol: Rooms lifecycle events
are timestamped logs, while agent events are durable NDJSON. The later contract
must merge them without pretending `InstanceStart` means guest readiness.

The host upgrade exposed one final setup issue before dispatch: Ship's
`better-sqlite3` addon had been installed under Node 20 and could not load under
Node 22. A forced lockfile-pinned pnpm reinstall rebuilt native dependencies for
Node 22. No model call occurred before that preflight passed.

## Operational findings

### Canonical image path is underspecified

The builder's example writes `agent-alpine.ext4`, while the E2E harness is
deliberately hard-coded to `~/rooms/images/rootfs.ext4`. The first E2E therefore
used the setup script's quickstart image and could prove only structural
isolation. The build and runbook need one canonical production image path.

### Sudo splits ownership and observability

Top-level room execution currently needs `sudo`. Room directories and
`room.json` are consequently root-owned. A simultaneous unprivileged
`rooms ls` saw three `unknown` rooms because it could not read their metadata;
`sudo rooms ls` showed the correct running descriptors and slots.

Collected `--out` directories are also root-owned. This is already visible at
the Ship boundary and should not be hidden by a generic execution protocol. We
need either an explicit privileged daemon boundary or deliberate ownership
handoff, not a convention that every observer invokes the whole CLI with sudo.

### A boot API success is not guest readiness

The failed readonly runs logged `microVM booted` because Firecracker accepted
`InstanceStart`, even though the guest kernel panicked less than a second later.
The execution contract must distinguish VMM started, guest ready, and workload
started. Collapsing these into one `running` state makes timeout diagnosis slow
and produces misleading liveness.

### Host workdir is not a guest path

Both agents received `/home/mh/room-gate` in Ship's rendered prompt and reported
it missing or inaccessible inside the guest. They correctly used
`/workspace/repo`, but the false blocker polluted both terminal summaries. A
portable WorkSpec should name a logical workspace, while each runtime maps that
to its own path.

### Agent execution needs a hard cap

Two equivalent one-line tasks differed from 29.5 seconds to 159.9 seconds. The
beta event stream remained healthy, but `RoomCursorRunner` does not pass
`--max-wall`, so a genuinely wedged agent has no substrate deadline. Placement
or execution policy must carry a cap that Rooms enforces independently of the
agent SDK.

### Host hooks do not magically enter the room

Both agents reported that the requested `code-reviewer` and `validator`
subagents were unavailable. The guest carries the SDK runner, but repo-specific
agent definitions and workbench hooks only exist when they are present in the
cloned repository or explicitly mounted/materialized by the execution request.
This should be an explicit WorkSpec input, not ambient host state.

## Preliminary contract evidence

### WorkSpec

Observed inputs are image path, runner kind, command or agent task, repository,
base SHA, model, push branch, environment, and output path. A wall-clock limit
was notably **absent** — `RoomCursorRunner` does not pass `--max-wall` (see
"Agent execution needs a hard cap" above) — and must become a required input
rather than be assumed observed. Several inputs are currently split between
CLI arguments, inherited environment, and files.

### Placement

Observed placement data includes runtime, immutable image identity, readonly
mode, host pool cap, claimed slot, TAP, guest IP, and network policy. Slot and
network details are Rooms receipts, not caller-authored work fields.

### RunEvent

The events we actually needed while debugging were: admitted, slot claimed,
Firecracker process started, VMM API ready, network attached, guest kernel
ready, SSH ready, command started, output collection, and terminal cleanup.
Rooms currently logs several of these but does not expose them as one ordered
event stream.

### RunResult

The durable result is currently spread across the process exit code,
`result.json`, stdout/stderr logs, `changeset.json`, collected files, and the
pushed branch for agent runs. Admission failure, guest boot failure, task
failure, timeout, collection failure, and cleanup failure need distinct terminal
reasons even when they share a nonzero process exit.

## Next checkpoint

Use this experiment as the evidence base for the provider-neutral execution
protocol TDD. Keep the first contract to one work request, one isolated run, an
ordered event stream, cancellation plus a hard deadline, and one terminal
receipt. Do not design a scheduler yet.
