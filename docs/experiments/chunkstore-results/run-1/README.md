# chunkstore run 1: synthetic snapshots on one Mac

A local run of [`examples/chunkstore/bench.py`](../../../../examples/chunkstore/)
with its defaults. No VM, no network, no real snapshot: this checks that the
mechanism works and shows which knobs matter before anyone spends host time.
Raw output is committed here in full ([`summary.json`](summary.json),
[`raw.ndjson`](raw.ndjson), 14 KB together). Every fetch run restored files
whose SHA-256 matched the source; the run exited 0.

```sh
python3 examples/chunkstore/bench.py --out docs/experiments/chunkstore-results/run-1
```

## Machine

| | |
| --- | --- |
| Host | Apple M5, 10 cores, 16 GB, macOS 26.6.2, APFS on internal SSD |
| Python | 3.14.6 (the unit tests were also run on 3.9.6) |
| Date | 2026-09-17 |
| Wall time | about 80 s for the whole benchmark |

## Input

Two snapshot-shaped sets (`snapshot.mem` 512 MiB sparse, `snapshot.vmstate`
30 KiB, `snapshot.json`), seed 1, 536,901,686 logical bytes each. Memory is runs
of zero pages (about 55% of runs), runs of one page repeated from a pool of 16
(20%) and random runs (25%). The second snapshot rewrites about 10% of pages in
runs of 1 to 32 pages, three quarters of them inside 8 hot windows, and shifts
the span from 40% to 60% of the file by a 38,358-byte insertion (same byte count
removed after the span, so the size is unchanged).

## Bytes to transfer the second snapshot

Cold: the host has nothing. Warm: the host already holds the first snapshot.
MB is 10^6 bytes; percentages are of the logical size.

| Strategy | Cold MB | Cold % | Warm MB | Warm % | Cold chunks | Warm chunks |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| whole-file copy | 536.9 | 100 | 536.9 | 100 | | |
| whole-file, skipping zero 4 KiB pages | 270.8 | 50.4 | 270.8 | 50.4 | | |
| fixed 64 KiB | 200.2 | 37.3 | 90.6 | 16.9 | 3,056 | 1,382 |
| fixed 256 KiB | 244.9 | 45.6 | 137.1 | 25.5 | 936 | 523 |
| fixed 1 MiB (default) | 354.4 | 66.0 | 249.6 | 46.5 | 340 | 238 |
| fixed 4 MiB | 515.9 | 96.1 | 482.3 | 89.8 | 125 | 115 |
| content-defined, avg 64 KiB | 210.8 | 39.3 | 73.6 | 13.7 | 3,893 | 1,101 |
| content-defined, avg 256 KiB | 243.6 | 45.4 | 111.7 | 20.8 | 1,060 | 422 |
| content-defined, avg 1 MiB | 274.0 | 51.0 | 181.5 | 33.8 | 386 | 247 |
| content-defined, avg 4 MiB | 274.0 | 51.0 | 223.1 | 41.6 | 211 | 177 |

Chunking and hashing both 512 MiB sets took 0.4 to 0.8 s per fixed strategy and
2.7 to 2.9 s per content-defined strategy.

What the table says, for this synthetic input only:

- **Chunk size matters more than the chunker.** Going from 1 MiB to 64 KiB fixed
  chunks cuts the warm transfer from 249.6 MB to 90.6 MB. Switching chunker at
  64 KiB saves a further 17 MB.
- **The 1 MiB default is a poor fit for page-granular memory.** A fixed chunk is a
  hole only when all of it is zero, so 1 MiB chunks move 354.4 MB cold, more than
  a copy that merely skips zero pages (270.8 MB). At 4 MiB nearly nothing is a
  hole or a repeat and chunking buys almost nothing (96% cold, 90% warm).
- **Content-defined chunking wins warm, not cold.** Its exact hole trimming keeps
  the cold cost near the nonzero byte count at large sizes (274.0 MB at 1 and
  4 MiB, where it finds no duplicate chunks at all), but at 64 KiB fixed chunking
  is slightly better cold (200.2 vs 210.8 MB). Likely causes, not isolated here:
  aligned 64 KiB chunks deduplicate page-aligned repeated runs more often, and
  the content-defined path only turns zero runs that cover a whole 64 KiB block
  into holes. Warm, the shifted span costs
  fixed chunking every nonzero chunk inside it and content-defined chunking
  almost none.
- The floor for the warm case is the rewritten pages themselves, at most about
  54 MB (10% of 512 MiB; overlapping rewrites make it a little less). The best strategy here moves 1.4 times that; the default
  moves 4.6 times that.

## Fetch time by seed count

Fixed 1 MiB chunks, 8 workers, each seed throttled to 20 MB/s by a token bucket.
`fetch` is manifest plus chunk download and verification; `wall` adds sparse
reassembly with a second verification of every chunk.

| Run | Seeds | MB fetched | fetch s | wall s | Per-seed chunks | Retries |
| --- | ---: | ---: | ---: | ---: | --- | ---: |
| unthrottled, cold | 1 | 354.4 | 0.486 | 1.339 | 340 | 0 |
| cold | 1 | 354.4 | 17.731 | 18.560 | 340 | 0 |
| cold | 2 | 354.4 | 8.865 | 9.397 | 170 / 170 | 0 |
| cold | 3 | 354.4 | 5.928 | 6.538 | 113 / 114 / 113 | 0 |
| warm (first snapshot local) | 1 | 249.6 | 12.483 | 13.187 | 238 (102 already local) | 0 |
| one corrupt seed of three | 3 | 354.4 | 8.868 | 9.920 | 0 / 171 / 169 | 3 |

- Download time is bytes divided by total seed bandwidth almost exactly:
  354.4 MB / 20 MB/s = 17.7 s, and 2 and 3 seeds give 2.00x and 2.99x. That is
  the token bucket talking. It shows the least-busy scheduler keeps every seed
  saturated and splits chunks evenly; it says nothing about real networks.
- Unthrottled loopback moved 354 MB in 0.49 s (about 730 MB/s), so the client is
  not the bottleneck at these rates. Reassembly adds 0.5 to 1.1 s.

## Corrupt seed

Seed 0 of 3 flipped the first byte of every chunk it served. The fetcher
rejected 3 chunks from it (consistent with the requests already in flight when
the first mismatch was seen), dropped it with `served bad bytes for chunk b998c4db…`,
accepted 0 bytes from it, refetched those 3 chunks from the other two seeds, and
finished in 8.868 s, the same as the clean 2-seed run. The restored files matched
the source hashes and no unverified chunk entered the local store. **Pass.**

## Limits

- Loopback plus a token bucket is not a network: no latency, loss, congestion or
  shared uplink, and all seeds and the fetcher share one Python process.
- Synthetic memory is not a real guest. The zero share, the 10% dirty rate, the
  hot-window clustering and above all the shifted span are choices, and they
  decide every number in the first table. Real guest RAM does not shift by
  unaligned insertions, so the fixed versus content-defined gap here is closer to
  what a rebuilt rootfs or toolstore image might show than to `snapshot.mem`.
- No compression, so nothing here compares byte for byte with the 371 MB
  experiment 5 bundle. Manifest bytes (36 KB at 1 MiB chunks, 455 KB at 64 KiB)
  and `/have` listings are not counted.
- One run, one seed value, one machine. No variance was measured.
- The fetch runs used the 1 MiB default only; 64 KiB chunks would move fewer
  bytes in 10 times as many requests, which was not timed.

## Next

Run `chunks.py ingest` and `chunks.py stats` on two real related Rooms snapshots
plus the rootfs and toolstore on a KVM host (commands in the
[example README](../../../../examples/chunkstore/README.md)), then repeat the
fetch between rented hosts.
