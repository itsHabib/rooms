# Experiment 15: swarm peers inside Rooms, coordinating through Redis on the host

**It works, and it holds under faults.** Two, four and six clones of one
snapshot each ran a peer that claimed tasks from a Redis server on the host.
All 240 tasks were accepted exactly once in every run, including the runs where
the host killed a clone and restarted the store while peers held leases.

## Setup

Local Lima VZ host: aarch64, 6 vCPU, 4 GiB, nested KVM, Firecracker 1.15.0,
512 MiB single-vCPU guests. `redis-server` 7.0.15 from Ubuntu 24.04, bound to
the host's `eth0` address with append-only persistence and `appendfsync always`.
[`rooms_peers.py`](../../../examples/lease-plane/rooms_peers.py) starts the server,
seeds the tasks, and runs `rooms clone -n N --command …`. The command carries
`store.py` and `peer.py` as a base64 tarball, so the guest needs only the
toolstore's Python. Each peer takes its id from `/proc/sys/kernel/random/uuid`;
clones share one frozen hostname and IP address, so neither can name a peer.
240 tasks, 200 ms simulated work, 2 s lease TTL, one trial per cell.

In fault runs the host waits until a quarter of the tasks are done, kills one
clone with `rooms kill`, kills the Redis server, waits two seconds and restarts
it from its append-only file.

## Results

| Clones | Faults | Active time | Tasks/s while active | claim p50 / p95 ms | Store retries | Lost leases | Accepted | Duplicates |
|---:|---|---:|---:|---|---:|---:|---:|---:|
| 2 | none | 25.7 s | 9.4 | 4.1 / 6.1 | 0 | 0 | 240 | 0 |
| 4 | none | 13.2 s | 18.2 | 4.2 / 5.8 | 0 | 0 | 240 | 0 |
| 6 | none | 8.9 s | 26.9 | 3.9 / 6.0 | 0 | 0 | 240 | 0 |
| 2 | kill + store restart | 48.0 s | 5.0 | 4.0 / 5.1 | 6 | 1 | 240 | 0 |
| 4 | kill + store restart | 19.1 s | 12.5 | 4.1 / 6.0 | 18 | 2 | 240 | 0 |
| 6 | kill + store restart | 13.0 s | 18.4 | 3.9 / 7.2 | 30 | 5 | 240 | 0 |

Active time runs from the first claim to the last completion any surviving peer
logged. Whole-batch wall time was 33 to 39 s without faults, most of it restore,
the readiness barrier and teardown.

## What this shows

- **Throughput scaled with clones**: 9.4, 18.2 and 26.9 tasks/s against an ideal
  of 10, 20 and 30. A claim cost about 4 ms from guest to host and back, and did
  not grow from two peers to six.
- **The invariants held under a dead peer and a store outage together.** The
  killed clone's lease expired and a survivor took its task; peers retried
  through the outage and none gave up; the server's own history shows one
  accepted completion per task and no accepted stale token.
- **A directory cannot be the store here.** Clones share no filesystem, so the
  file backend from the process-mode benchmark has no equivalent between Rooms.
  For peers in Rooms the choice is a networked store or nothing.
- **Clones are indistinguishable by hostname and address.** Identity has to come
  from something minted after restore.

## Substrate findings

Both are recorded in [follow-ups](../../follow-ups.md).

- A guest can reach services on the host's own LAN address. The forward chain
  blocks private ranges, but this traffic arrives through `INPUT`. The experiment
  depends on it; a hardened host should allow it per service.
- After `rooms kill` of one member, `rooms clone --command` waited for the whole
  `--max-wall` (exit 124) although every task had finished, and the killed
  member's partial output was not collected. The fault rows above therefore have
  no claims from the killed peer in their latency figures.

## Limits

- One trial per cell, six clones at most, one laptop-class host. Peers, server
  and clones share that host, so this is not a network partition and says nothing
  about cross-host latency.
- The first run (`swarm-rooms-run1` in the evidence) fired faults after a fixed
  eight seconds, which at four and six clones was after the work had finished.
  It is kept as the record of that mistake; the table is from the second run.
- A kill and a store restart are the only faults. Pauses, clock steps and
  one-way partitions from the seeded schedules were not run inside Rooms.
- Redis ran unauthenticated on a lab host for the length of each run.

## Repeat it

```sh
sudo apt-get install -y redis-server && sudo systemctl disable --now redis-server
sudo -E python3 examples/lease-plane/rooms_peers.py --rooms target/release/rooms \
  --snapshot "$SNAPSHOT" --image "$IMAGE" --toolstore "$TOOLSTORE" \
  --host-ip "$(ip -4 -o addr show eth0 | awk '{print $4}' | cut -d/ -f1)" \
  --counts 2,4,6 --tasks 240 --wall-s 120 --faults --out "$RESULTS"
```

[Summary](summary.json) · [raw evidence](evidence.tar.xz), SHA-256
`30acaa075395f31dcad9bc7e2791f500fb0b44a41b0f836220f2debb8716b24d`: both runs'
clone output, peer logs and server histories, without Redis data files. No cloud
resources or model calls; final inventory had no Rooms, VMMs or Redis servers.
