# Swarm state plane: first local results

One run of [`examples/swarm/bench.py`](../../../examples/swarm/README.md) in
process mode on one laptop, 2026-09-16. It compares lease coordination on a
shared directory with the same protocol spoken over RESP, then replays five
seeded fault schedules against each. Raw data is in [`run-1/`](run-1/):
`summary.json`, and per run `peers.ndjson` (each peer's view),
`history.ndjson` (the store's grants and completions) and, for fault runs,
`schedule.json`.

```sh
python3 examples/swarm/bench.py --out docs/experiments/swarm-plane-results/run-1 \
  --tasks 640 --fault-seeds 5
```

## Machine

Apple M5, 10 cores, 16 GB, macOS 26.6.2 (arm64), APFS on the internal disk,
Python 3.14.6 for the run (the tests also pass on 3.9.6). Every peer is its
own `python3` process; the RESP backend is `fake_resp.py` as one more process
on loopback.

**No real Redis or Valkey was measured.** Neither `valkey-server` nor
`redis-server` is installed on this machine, so the `real` backend was skipped
and `RespStore` has only run against the fake.

## Claim latency and throughput

640 tasks, 20 ms mean simulated work (sleep), 2 s ttl. `claim` is one scan from
listing pending tasks to a granted lease; `acquire` is the winning call alone.
Milliseconds.

| Backend | Peers | claim p50 | claim p95 | claim max | acquire p50 | acquire p95 | tasks/s | claim attempts rejected | accepted duplicates |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| file | 4 | 0.87 | 1.42 | 13.6 | 0.35 | 0.52 | 158 | 31 | 0 |
| file | 16 | 1.75 | 3.65 | 7.0 | 0.60 | 1.20 | 524 | 207 | 0 |
| file | 64 | 12.40 | 44.32 | 101.5 | 2.78 | 30.64 | 454 | 1660 | 0 |
| fake RESP | 4 | 1.23 | 2.50 | 12.1 | 0.23 | 0.55 | 158 | 19 | 0 |
| fake RESP | 16 | 1.89 | 5.84 | 15.9 | 0.36 | 2.41 | 486 | 197 | 0 |
| fake RESP | 64 | 27.77 | 57.25 | 236.2 | 15.18 | 24.01 | 448 | 1772 | 0 |

All six runs finished with 640 accepted completions, no rejected completions,
no lost leases and no store retries.

What this does and does not say:

- At 4 and 16 peers the two backends are within noise of each other, and
  throughput is set by the simulated work (4 peers x 50 tasks/s is 200).
- At 64 peers on 10 cores throughput does not rise. The run is about 1.4 s
  long and a large part of it is starting 64 Python interpreters, so tasks/s
  at 64 peers is a statement about process start-up here, not about either
  store.
- The fake RESP server is *slower* than the directory at 64 peers (acquire
  p50 15 ms against 2.8 ms). That is one Python process serialising every
  command behind a lock while 64 clients wait; it is a property of the fake
  and says nothing about Valkey. The directory, meanwhile, is a local APFS
  volume with a shared page cache, which is the best case a file store will
  ever see. **This run cannot answer "does a networked store beat files once
  peers are isolated machines"**; it only shows the harness measures both
  through one interface, and gives a local baseline to compare guest numbers
  against.
- A single run per cell. No repeats, so no confidence intervals.

## Seeded fault runs

8 reckless peers, 48 tasks, 500 ms mean work, 400 ms ttl, so a holder paused
past its ttl is overtaken in the middle of a task. Each schedule has 8
disruptions over 3 s drawn from: pause/resume, kill/restart, store
kill/restart, clock skew. Seeds 1 to 5, same schedules for both backends.

| Backend | Seed | Invariants | Accepted | Stale completions rejected | Accepted duplicates | Store retries | Peers gave up |
| --- | ---: | --- | ---: | ---: | ---: | ---: | ---: |
| file | 1 | pass | 48 | 3 | 0 | 40 | 0 |
| file | 2 | pass | 48 | 7 | 0 | 62 | 0 |
| file | 3 | pass | 48 | 8 | 0 | 71 | 0 |
| file | 4 | pass | 48 | 0 | 0 | 21 | 0 |
| file | 5 | pass | 48 | 7 | 0 | 79 | 0 |
| fake RESP | 1 | pass | 48 | 3 | 0 | 40 | 0 |
| fake RESP | 2 | pass | 48 | 2 | 0 | 65 | 0 |
| fake RESP | 3 | pass | 48 | 3 | 0 | 70 | 0 |
| fake RESP | 4 | pass | 48 | 1 | 0 | 21 | 0 |
| fake RESP | 5 | pass | 48 | 1 | 0 | 79 | 0 |

10 of 10 runs kept all three invariants: exactly one accepted completion per
task, none accepted after a newer token was granted, none left unfinished.
Stale completions were attempted and refused in 9 of the 10 runs, so the
fencing check was exercised and not merely present. The file backend rejected
more of them in three of five seeds (7, 8, 7 against 2, 3, 1). A likely reason, not isolated in this run:
clock skew only affects the backend that judges expiry by the peer's clock, so
a peer whose clock is stepped forward takes over live leases and the original
holders then lose at `complete`.

An earlier fault workload (300 short tasks) passed trivially with zero stale
completions: with many more pending tasks than peers, nobody tried the paused
holder's task before it resumed. The workload above was chosen so that
takeovers happen.

## Limits

- One machine, one kernel, loopback: no partition, no packet loss, no real
  clock drift. The "store kill" for the file backend is `chmod 000`.
- The fake server is not Redis, and no real server was available.
- A local APFS directory is not the shared filesystem guests would use.
- Passing five seeds is evidence, not proof. The proof-shaped part is
  `python3 examples/swarm/model.py`, which covers two peers and one task
  exhaustively, and the checker's control test, which shows a store without
  fencing is caught.
- The schedule for a seed is reproducible byte for byte; process timing is
  not, so numbers will differ run to run.
