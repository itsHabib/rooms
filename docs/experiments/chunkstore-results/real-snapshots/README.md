# Real snapshots through the chunk store

Three real Rooms snapshots from the local KVM host (aarch64 Lima VZ, 512 MiB
guests, Firecracker 1.15.0), ingested with fixed 64 KiB chunks. `snapshot.mem`
plus `snapshot.vmstate` for each; rootfs and toolstore are not included.
All-zero chunks are holes and are never stored or sent. MB is 10^6 bytes.

| Snapshot | Logical | Holes | Data to send cold | Given `cold` already local |
|---|---:|---:|---:|---:|
| `cold`: neutral base, nothing warmed | 536.9 | 478.8 | **58.1** | — |
| `pytask`: base warmed by running a Python task | 536.9 | 424.0 | 112.9 | 88.5 |
| `subset`: base warmed by reading 185 MiB of the toolstore | 536.9 | 179.9 | 353.2 | 332.9 |

## What this shows

- **A cold snapshot is 89% zeros.** Skipping holes sends 58 MB where a whole-file
  copy sends 537 MB. That is most of the available saving, and it needs no
  chunk-level deduplication at all.
- **Related snapshots share little at 64 KiB.** A warmed snapshot reuses only
  20 to 24 MB of the cold one (24.4 of 112.9 for `pytask`, 20.3 of 353.2 for
  `subset`), and the two warmed snapshots share about as little with each other
  (`pytask` needs 92.0 of 112.9 MB given `subset`). Guest page cache lands at
  different physical addresses on every boot, so identical file contents do not
  line up in 64 KiB windows. Page-sized chunks might find more; that was not run
  (three snapshots would be roughly 130,000 objects each).
- **Warming trades wire bytes for host memory.** The subset-warmed base that cut
  six clones from 1,484 MiB to 225 MiB of host memory (experiment 2b) is six
  times larger to ship than the cold base. For a snapshot that travels, warm
  after arrival or ship the cold base and the toolstore and warm on the
  destination.
- Content-defined chunking at the same average size changed these figures by
  under 3% (332.1 against 332.9 MB for `subset` given `cold`), so on real guest
  memory it does not earn its complexity. At 1 MiB chunks the same transfer was
  352.3 MB fixed and 339.5 MB content-defined.

## Limits

One host, three snapshots of one image, one chunk size saved in full. No network
transfer was timed here; the fetch timings in `run-1` are loopback with a token
bucket. The per-pair JSON files beside this one are the tool's own `stats` output.
