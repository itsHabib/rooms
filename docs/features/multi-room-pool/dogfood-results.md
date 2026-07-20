# Multi-room pool — dogfood results (multi-model concurrency)

**Date:** 2026-07-20 · **Host:** rooms-host (Ubuntu, Hyper-V; real Firecracker + KVM)
**Binary:** `rooms 0.1.0` at main · **Image:** `agent-alpine-cursor.ext4` (RO overlay)
**Task (identical for every room):** create `HAIKU.md` at the repo root — one
5-7-5 haiku about ephemeral Firecracker microVMs, add only that file.

## What this exercised

Four `rooms run --runner cursor` invocations fired concurrently (`… & … & wait`),
one frontier model each, against identical clean rooms — the v0.2 multi-room-pool
keystone dogfooded on real agent work: per-room network slots, multi-model
dispatch, overlapping boots, and clean concurrent teardown with zero leaks.

## Pool concurrency — proven (the keystone)

The substrate ran all four rooms genuinely in parallel, in distinct slots, every
time — independent of whether the workload model succeeded:

| Model | slot | tap | `vmm_started` |
| --- | --- | --- | --- |
| grok-4.5 | 3 | tap-fc3 | 16:26:07.805 |
| composer-2.5 | 4 | tap-fc4 | 16:26:07.797 |
| claude-opus-4-8 | 2 | tap-fc2 | 16:26:07.807 |
| gpt-5.5 | 1 | tap-fc1 | 16:26:07.813 |

- **Distinct slots:** slots 1–4, taps tap-fc1–4 — no collision (cap is 8, so N=4
  is comfortably under).
- **Concurrency:** all four VMMs started within a **16 ms** window (…797 → …813),
  i.e. all four alive at once — boots overlapped, not serialized.
- **Zero leaks:** every lifecycle ended `cleanup_done`; after the run
  `pgrep -a firecracker` was empty and `rooms ls` reported no rooms. Same on a
  second concurrent 4-room run.

## Model results

| Model | Status | Wall-clock (workload) | Patch | Notes |
| --- | --- | --- | --- | --- |
| **grok-4.5** | ✅ succeeded | **14.9 s** | 1 file, clean | valid 5-7-5 |
| **composer-2.5** | ✅ succeeded | **21.1 s** | 1 file, clean | valid 5-7-5 |
| claude-opus-4-8 | ❌ failed | — | — | Cursor SDK `status: ERROR` (unavailable) |
| gpt-5.5 | ❌ failed | — | — | Cursor SDK `status: ERROR` (unavailable) |

A follow-up concurrent run swapped the two failures for the kickoff's named
alternates — `claude-sonnet-4` and `gemini-3.1-pro` — and **both also failed
identically**. On this Cursor account, only Cursor's own bundled models
(`composer-2.5`) and `grok-4.5` are reachable through the SDK; every third-party
frontier model (Anthropic / OpenAI / Google) returns a post-run
`status: ERROR` → *"Cursor SDK reported error without a message."* This is an
**account/plan access limit, not a rooms substrate issue** — the pool booted,
isolated, and reaped all four rooms flawlessly in both runs; the workload layer
(the Cursor agent) is what rejected the unavailable models. A single failed
model leaves the other rooms untouched: failure is per-room, and teardown of a
failed room is as clean as a successful one.

## Grok 4.5 vs Composer 2.5 (the head-to-head)

Both ran the identical task in identical clean rooms. Head-to-head:

| | grok-4.5 | composer-2.5 |
| --- | --- | --- |
| Wall-clock (workload_started → workload_exited) | **14.9 s** | 21.1 s |
| Patch correctness | one `HAIKU.md`, valid 5-7-5 | one `HAIKU.md`, valid 5-7-5 |
| Patch noise (files beyond the ask) | none (1 file) | none (1 file) |

Both nailed the contract exactly — a single `HAIKU.md`, nothing else touched —
so on *correctness* and *discipline* they tie. The separation is speed: **Grok
finished the workload ~30% faster** (14.9 s vs 21.1 s). Neither wandered, neither
added scaffolding or commentary files.

The haikus themselves:

**grok-4.5**
```
Boot, run, then vanish—
Firecracker microVMs
leave no lasting trace.
```

**composer-2.5**
```
Ephemeral guest
Firecracker sparks to life
Gone when task is done
```

Both are on-theme (ephemerality, Firecracker) and structurally haiku-shaped.
Grok's is the tidier 5-7-5 and leans on the disposable-VM lifecycle directly
("boot, run, then vanish"); Composer's is more imagistic ("sparks to life") and
reads a hair looser on the middle line. A wash on quality; Grok wins on latency.

## Acceptance vs. reality

- ✅ **4 concurrent rooms, distinct slots, overlapping boots, zero leaks** — the
  pool keystone, proven twice.
- ✅ **Grok vs Composer comparison** on identical tasks in identical clean rooms.
- ⚠️ **"all four succeeded"** was not reachable: only `grok-4.5` and
  `composer-2.5` are available through this Cursor account. Two baseline columns
  are unavailable-by-access, not by substrate failure — documented above rather
  than papered over.

## Reproduce

```
source ~/.rooms-creds.env   # export CURSOR_API_KEY=…
printf 'Create a file HAIKU.md at the repo root: one haiku (5-7-5) about\nephemeral Firecracker microVMs. Add only that file.\n' > /tmp/task-haiku.md
for M in grok-4.5 composer-2.5 claude-opus-4-8 gpt-5.5; do
  sudo -E rooms run --runner cursor \
    --image ~/rooms/images/agent-alpine-cursor.ext4 \
    --repo https://github.com/itsHabib/rooms --base-sha main \
    --task /tmp/task-haiku.md --model "$M" \
    --out /tmp/cc-$M-out --lifecycle /tmp/cc-$M-lc.ndjson \
    > /tmp/cc-$M.log 2>&1 &
done
wait
```
