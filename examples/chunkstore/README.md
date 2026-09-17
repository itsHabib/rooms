# chunkstore: chunked, content-addressed snapshot distribution

Today a second host downloads a whole snapshot bundle before it can restore a
room. This directory is the host-free foundation for doing better: split the
artifacts into hashed chunks, never move zero pages, move only the chunks a
host lacks, and pull them from several peers at once while verifying every
byte. It is Python 3 standard library only (3.9+), and it never touches a VM.

It is the groundwork for two entries in the experiment backlog: chunked images
(deduplication and bytes transferred across two related snapshots) and
peer-to-peer snapshot distribution (time to ready as the number of seeds grows,
every chunk hash verified on receipt).

## Pieces

| File | Job |
| --- | --- |
| `chunks.py` | Fixed-size and content-defined chunking, manifests, the content-addressed store (`objects/ab/cdef…`, atomic writes), verified sparse reassembly, and the `ingest` / `stats` CLI |
| `serve.py` | Threaded HTTP seed: `GET /manifest/<name>`, `GET /chunk/<sha256>`, `GET /have`; optional token-bucket throttle; `--corrupt` fault injection |
| `fetch.py` | Restore an artifact set from one or more seeds: skip local chunks, fetch the rest in a bounded pool spread across seeds, verify before storing, drop a seed that serves bad bytes, reassemble, report per-seed stats |
| `bench.py` | Synthetic snapshots, transfer accounting per strategy, throttled multi-seed fetch timing, corrupt-seed run |

A manifest is one JSON document per artifact set: for each file, an ordered list
of `[offset, length, sha256]`. An all-zero chunk has a `null` hash; it is a hole,
is never stored or sent, and is left sparse on reassembly. File names in a
manifest must be plain names, and chunks must tile the file exactly, so a
hostile manifest cannot write outside the output directory.

The content-defined chunker first turns zero runs (found per 64 KiB block, then
trimmed to the exact first and last nonzero byte) into holes. Inside data runs a
cut may land only after one of 8 anchor byte values, and does so when the CRC-32
of the 48 bytes before it has its low bits clear; `min = avg/4` and
`max = 4*avg` bound the chunk. This is a windowed-hash cut rather than a per-byte
gear loop because the candidate scan runs inside `re` and `zlib`: on the recorded
run it chunked and hashed about 190 MB/s of nonzero data, where a per-byte gear
loop in pure Python measured about 10 MB/s on the same machine. The cut
still depends only on nearby content, which is the property that matters.
Data with none of the anchor bytes falls back to `max`-sized cuts.

## Run it

```sh
make test-chunkstore                                   # unit tests, a few seconds
python3 examples/chunkstore/bench.py --out /tmp/chunkstore-run-1
```

`bench.py` refuses to reuse an output directory. It writes `summary.json` and
`raw.ndjson` there; the synthetic snapshots and stores live in a temp directory
and are deleted. Useful flags: `--size-mib 512`, `--seed 1`, `--rate-mbps 20`,
`--workers 8`, `--sizes-kib 64 256 1024 4096`, `--fetch-chunk-kib 1024`.

Results from a real run are in
[`docs/experiments/chunkstore-results/`](../../docs/experiments/chunkstore-results/).

### Measure real snapshots (Linux KVM host)

```sh
cd examples/chunkstore
S=/var/tmp/chunkstore
# two related snapshots of the same room, plus the images a restore needs
python3 chunks.py ingest --store $S --name a-fixed --size 65536 snapA/snapshot.mem snapA/snapshot.vmstate snapA/snapshot.json
python3 chunks.py ingest --store $S --name b-fixed --size 65536 snapB/snapshot.mem snapB/snapshot.vmstate snapB/snapshot.json
python3 chunks.py ingest --store $S --name a-cdc --chunker cdc --size 65536 snapA/snapshot.mem snapA/snapshot.vmstate snapA/snapshot.json
python3 chunks.py ingest --store $S --name b-cdc --chunker cdc --size 65536 snapB/snapshot.mem snapB/snapshot.vmstate snapB/snapshot.json
python3 chunks.py stats --store $S a-fixed b-fixed      # b_given_a.needed_bytes is the warm transfer
python3 chunks.py stats --store $S a-cdc b-cdc
```

Files in one set must have distinct base names. `ingest` prints the set's
`manifest_sha256`. Ingest the rootfs `.ext4` and `toolstore.sqfs` the same way
(as their own sets, or inside the snapshot set) to see what they add.

### Move a set between hosts

```sh
python3 serve.py --store $S --host 0.0.0.0 --port 8000          # on each seed
python3 fetch.py --store /var/tmp/local --out restored \
  --seed http://seed-1:8000 --seed http://seed-2:8000 \
  --manifest-sha256 <from ingest> b-fixed                        # on the new host
python3 serve.py --store /var/tmp/local --port 8000              # it can now seed too
```

`fetch.py` prints per-seed chunks, bytes, errors and bad chunks, retries,
`fetch_seconds` (network phase) and `wall_seconds` (including reassembly).

## Trust

Chunk hashes protect chunks; the manifest is the root. Pass `--manifest-sha256`
(printed by `ingest`, carried out of band) or the first seed that answers decides
what you restore. The server has no authentication or TLS and serves anyone who
can reach the port: keep it on a private network.

## Honest limits

- **Loopback with a token bucket is not a network.** No latency, loss,
  congestion control, or shared uplink. Seeds and fetcher are threads in one
  Python process on one machine. Seed-count scaling in the bench mostly shows
  that the scheduler keeps every throttled seed busy; it is an upper bound on
  what real peers would give.
- **Synthetic memory is not a real guest.** The zero/repeated/random mix, the 10%
  dirty pages clustered in hot windows, and the shifted span are parameters we
  chose. They set the dedup numbers. Real guest RAM does not shift by an
  unaligned insertion at all; that part stands in for images such as a rebuilt
  rootfs, and it is what separates fixed from content-defined chunking here.
  Only the `ingest` / `stats` run on real snapshots answers the real question.
- **No compression.** Repeated and text-like pages compress well, so these byte
  counts are not comparable with a packed bundle such as the 371 MB experiment 5
  object. Bytes here are uncompressed chunk payloads. Manifest and `/have` bytes
  are not counted: the manifest of the 512 MiB synthetic set is 455 KB at 64 KiB
  fixed chunks and 36 KB at 1 MiB.
- **Whole-file restore only.** Nothing here starts a room after fetching only the
  chunks it touches; that needs a lazy block device or userfaultfd backend.
- **`/have` is one full listing per seed**, fine for thousands of chunks, not for
  millions. No chunk garbage collection, no partial-seed discovery beyond the
  seed URLs given.
- **Reads hash everything in Python.** Throughput is bounded by `hashlib` and the
  GIL, roughly 0.7 GB/s unthrottled on the run recorded here.
