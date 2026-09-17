# Swarm state plane: file coordination versus a networked store

A host-free harness for two questions about a swarm of agent peers that share
work through leases:

1. Does a networked key-value store beat coordination on a shared directory,
   and what does each cost in claim latency as the swarm grows?
2. Do the lease rules hold when peers are paused, killed, restarted and
   clock-skewed and the store itself is taken away, under a fault schedule that
   a seed reproduces?

Everything is Python 3 standard library (3.9+), with no pip dependencies and no
Redis client, so the same files can run inside a guest that has nothing else.
Nothing here touches the Rust runtime.

| File | Role |
| --- | --- |
| `store.py` | `FileStore(dir)` and `RespStore(host, port)` behind one surface: `acquire`, `extend`, `release`, `complete`, `pending`, `history`, `receipts` |
| `fake_resp.py` | In-process threaded RESP2 server with only the commands `RespStore` sends, plus an optional write journal |
| `peer.py` | The peer loop: claim, work, heartbeat, complete |
| `faults.py` | Seeded fault schedules, a replayer, and the invariant checker |
| `model.py` | Exhaustive model check of the abstract lease protocol |
| `bench.py` | Process-mode benchmark and seeded fault runs |

## The protocol

A lease has a holder, an expiry and a **fencing token** that only grows per
task. `complete` is accepted only with the newest token granted for the task,
and only once. A peer that was paused past its ttl still believes it holds the
lease; the token is what makes the store refuse its late completion.

- **FileStore** keeps a directory of numbered slot files per task. Slot *n* is
  the grant of token *n* or the completion of token *n - 1*. Slots are created
  by hard-linking a fully written temp file, which fails if the slot exists, so
  each slot has one winner. Completing with token *n* means winning slot
  *n + 1*, which is impossible once token *n + 1* has been granted. Expiry is
  judged by the reader's clock.
- **RespStore** uses `SET key holder NX PX ttl` for the lease, `INCR` for the
  token, Lua via `EVAL` for compare-and-act `extend`, `release` and `complete`,
  and one stream (`XADD`) for grants and completions. Expiry is judged by the
  server, so peer clock skew does not matter.

## Run it

```sh
make test-swarm                       # or: python3 -m unittest discover -s examples/swarm -p 'test_*.py'
python3 examples/swarm/model.py       # the model's verdicts and counterexample traces
python3 examples/swarm/faults.py --seed 7 --peers 8       # print a schedule
python3 examples/swarm/bench.py --out /tmp/swarm-run      # 4/16/64 peers, then 5 fault seeds
```

One peer by hand, against a directory or a server:

```sh
python3 examples/swarm/peer.py --store file:/tmp/swarm-store --peer-id a --tasks 20
python3 examples/swarm/fake_resp.py --port 6390 &
python3 examples/swarm/peer.py --store resp:127.0.0.1:6390 --peer-id a --tasks 20
```

If `valkey-server` or `redis-server` is on `PATH`, the contract tests also run
against it and `bench.py` adds a `real` backend. Nothing is installed for you.

## What the benchmark reports

Per backend and peer count: claim latency (`claim_ms`: one scan from listing
pending tasks to a granted lease; `acquire_ms`: the winning `acquire` call
alone) as p50/p95/max, throughput, `claim_attempts_rejected` (acquire calls
that lost to another holder), rejected completions, and
`accepted_duplicates`, which must be 0. Then, per fault seed, whether the run
kept three invariants: every task has exactly one accepted completion, no
completion was accepted after a newer token had been granted, and no task is
left unfinished while a live peer exists. Fault-run peers are `--reckless`:
they keep working after a failed heartbeat, so it is the store's fencing that
is under test and not the peer's good manners.

`bench.py` refuses to write into an existing directory. Results from one
machine are in
[`docs/experiments/swarm-plane-results`](../../docs/experiments/swarm-plane-results/README.md).

## What the model shows

`model.py` explores every interleaving of two peers and one task, where a
holder may pause for longer than the ttl. Without the token check a double
accepted completion is reachable, and the shortest trace is printed. With it,
the invariant holds in every reachable state. Two further configurations show
why `complete` also needs a done flag: if a peer's "is this task pending?"
read is not atomic with the grant, the token check alone is not enough. Both
stores close that gap inside the atomic step (a completion slot; a tombstoned
lease key plus `HSETNX`). The contract tests replay the model's counterexample
against each real backend.

## Honest limits

- **Process mode on one machine is not a network partition.** Peers share a
  kernel, a page cache and a loopback interface. A store "kill" for the file
  backend is renaming the directory away; for RESP it is `SIGKILL` of the server. Neither drops packets in one
  direction, delays them, or splits the swarm into sides that each see a store.
- **The fake server is not Redis.** It runs the commands under a Python lock in
  a threaded server, so its latency under load measures the fake and not a
  networked store. There is no Lua: the three scripts have Python twins matched
  by exact text, so a script edit that is wrong Lua but right Python passes
  here. On a machine with neither server installed, `RespStore` has only been
  exercised against the fake.
- **A local directory is not a shared filesystem.** The file store leans on
  `link(2)` failing atomically when the target exists. That is true on APFS
  and ext4; it must be re-checked on whatever carries the directory between
  guests (virtiofs, 9p, NFS) before the file numbers mean anything there.
- **A seed fixes the schedule, not the interleaving.** The schedule is
  byte-for-byte reproducible. Process timing is not, so a failing seed is a
  fault pattern to re-run, not a guaranteed replay.
- **Persistence is assumed for store kills.** The fake journals every write
  and the real server is started with `appendonly yes`. A store that loses
  its fence counters on restart would hand out old tokens again.
- **Latency includes paused time.** A peer stopped by `SIGSTOP` in the middle
  of a claim reports the pause as claim latency; look at p50/p95, not max, in
  fault runs.
- `acquire` on RESP is two commands (`SET NX PX`, then `INCR`). A peer stalled
  between them for longer than the ttl can hold the newest token without the
  lease. That costs one wasted lease, not a double completion.
