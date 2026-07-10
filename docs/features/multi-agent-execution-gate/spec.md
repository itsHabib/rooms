# Multi-agent Rooms execution gate

**Status:** Complete  
**Owner:** Codex  
**Created:** 2026-07-09  
**Completed:** 2026-07-10  

**Run evidence:** [2026-07-09 multi-agent Rooms experiment](../../experiments/multi-agent-rooms-2026-07-09.md)

## Goal

Prove that Rooms can run real concurrent work end to end, first as generic commands and then as independent Ship agent jobs.

The multi-room pool has already been exercised at the Firecracker process level. This task closes the more important gap: multiple callers doing useful work at the same time, with distinct room state, outputs, branches, and terminal results.

The observations from this gate will also provide concrete input for a later TDD covering a provider-neutral execution contract. That design should be extracted from a working boundary, not invented ahead of it.

## Why now

Rooms already has the main substrate:

- jailed Firecracker VMs
- per-room networking and slot allocation
- bounded pool admission and backpressure
- command execution and artifact collection
- a Cursor SDK runner
- direct `runtime: rooms` support in Ship

What is not yet proven is the consumer-facing behavior under concurrency. A successful N=3 boot test shows that the substrate can allocate VMs. It does not prove that three independent jobs can enter, execute, return evidence, and clean up without state crossing between them.

## Scope

This task has four stages. Each stage produces evidence needed by the next one.

### Stage 1: Rebuild and qualify `rooms-host`

Finish the current clean-room host rebuild using the checked-in provisioning path.

Operator checkpoint: run the following from an elevated PowerShell session:

```powershell
cd C:\Users\MichaelHabib\pers\rooms
.\scripts\provision-hyperv-auto.ps1
```

Then:

1. Confirm `ssh rooms-host` reaches the new VM.
2. Clone or update Rooms on the host.
3. Run the documented host setup and build the current guest image.
4. Pair the guest SSH key with the image instead of reusing stale image/key state.
5. Verify TAP setup, forwarding, and `ROOMS_FWD` behavior.
6. Run `make check` and `sudo -E make e2e`.
7. Confirm `rooms doctor` is green apart from explicitly accepted credential warnings.

The host is qualified only when it was built from the current scripts and no manual, undocumented repair is required.

### Stage 2: Run three concurrent CLI rooms

Start three top-level `rooms run --command` executions concurrently. Each command must:

- write a unique token to its room-local filesystem
- emit that token to stdout
- sleep long enough to guarantee execution overlap
- produce a collected output artifact
- exit with a known status

While all three jobs are live, capture:

- room IDs and allocated slots
- guest IPs and TAP devices
- Firecracker process IDs
- `rooms ls` output
- host memory usage

After completion, verify:

- every result contains only its own token
- every collected artifact belongs to the correct invocation
- all Firecracker processes and TAP resources are reaped
- no stale slot remains allocated
- the next room can reuse a released slot successfully

Run one bounded-capacity case with `--max-pool 2` or its current equivalent. The third request must receive the documented `pool_full` response rather than hanging, racing into an occupied slot, or failing opaquely.

### Stage 3: Run two concurrent Ship agents on Rooms

Dispatch two direct `ship ship --runtime rooms` jobs concurrently. Do not add Ship driver-engine support as part of this task; exercise the already-supported direct runtime boundary first.

Use a small fixture repository with two independent, mechanically verifiable changes. Each job must receive:

- a distinct task prompt
- a distinct branch name
- the same pinned room image
- an explicit model/provider selection
- enough execution time to overlap with the other job

Operator checkpoint: obtain approval immediately before dispatch because this stage may incur Cursor/model usage.

Verify that both jobs:

- occupy distinct rooms and pool slots concurrently
- clone and modify the repository independently
- push only their assigned branch
- return valid Ship and Rooms terminal artifacts
- preserve logs after the VM is destroyed
- leave no room, process, network, or slot residue

The fixture should avoid requiring an expanded language toolchain in the guest. This is a Rooms execution proof, not an image-completeness project.

### Stage 4: Capture the execution boundary

Record the exact data that crosses the Ship-to-Rooms boundary during Stage 3. Capture facts, including awkward ones, rather than normalizing them into a speculative API.

Map the observed behavior into four candidate protocol shapes:

| Shape | Evidence to capture |
| --- | --- |
| WorkSpec | repository, revision, command or agent prompt, environment, secrets references, timeout, expected outputs |
| Placement | local versus Rooms, image identity, CPU and memory, network policy, pool constraints |
| RunEvent | admitted, booting, ready, running, output, collecting, terminal, timestamps and ordering |
| RunResult | exit reason, status, artifact references, branch or commit, room identity, diagnostics |

Also record:

- how callers discover liveness today
- how cancellation propagates today
- which identifier is stable across Ship and Rooms
- what survives room teardown
- how admission failure differs from execution failure
- what data is currently passed through argv, environment variables, files, and JSON
- which details are Rooms-specific and must not leak into a generic execution contract

Save the run report under `docs/experiments/` with the execution date. This report is the primary source material for the later spawner/execution-protocol TDD.

## Acceptance criteria

- A fresh `rooms-host` is reproducibly provisioned from the current scripts.
- Host checks and Firecracker end-to-end tests pass on that host.
- Three concurrent CLI jobs overlap in time and return distinct, correct outputs.
- Pool capacity produces an explicit, machine-readable backpressure result.
- Two concurrent Ship agent jobs overlap in time, push distinct branches, and return valid terminal artifacts.
- No tested path leaks filesystem state, tokens, processes, TAP devices, or slot ownership between jobs.
- A dated experiment report captures the real execution boundary using WorkSpec, Placement, RunEvent, and RunResult as analytical headings.
- Any failure becomes either a narrowly scoped Rooms fix or a documented input to the later TDD.

## Non-goals

- implementing a generic spawner or execution protocol
- adding `runtime: rooms` support to the Ship driver engine
- integrating `/work-driver`
- snapshot or restore support
- Nix-based guest environment construction
- running Claude Code or Codex inside Rooms
- broadening the guest image for arbitrary project toolchains
- PR review, merge automation, or production scheduling

## Risks and recovery

### Guest key mismatch

The previous behavioral test path was blocked by an SSH key baked into the image that did not match the host-side key. Rebuild the image and key pair together; do not patch around the mismatch after boot.

### Environment loss through `sudo`

Credentials and forwarding variables may disappear across privilege elevation. Use the repository's documented `sudo -E` path and record the exact required environment in the experiment report.

### False concurrency

Starting three commands sequentially and observing three successful completions is not enough. Commands must deliberately overlap, and process, slot, and network evidence must be captured during that overlap.

### Paid-agent ambiguity

Keep Stage 3 to two tiny deterministic tasks. Record provider, model, elapsed time, and approximate usage so future gates have a known cost.

### Dirty teardown

After every failed run, inspect `rooms ls`, Firecracker processes, TAP devices, and slot state before retrying. Preserve logs first, then clean only resources owned by the failed invocation.

## Outcome

The gate passed. The dated experiment report records the host rebuild, N=3 CLI
proof, N=2 Ship proof, fixes discovered during qualification, and execution
contract evidence.
