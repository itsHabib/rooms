# Phase-2 rooms-host proof — 2026-08-22

This is the durable index for the retained Rooms-owned snapshot/fork run, proof ID `zA3P`. The full machine artifacts remain on `rooms-host`; their hashes below make accidental drift visible. An explicit [public summary projection](phase2-killer-2026-08-22-summary.public.json) keeps the result reviewable while naming the two omitted operator-local path fields and binding back to the raw summary hash. This run demonstrates the Rooms broadcast-fleet substrate's named hard checks. It does not claim either performance gate or the still-missing distinct-task `/work-driver` consumer adapter.

## Invocation and source

```sh
cd <rooms-checkout>
./scripts/phase2-killer.sh
```

- Admitted source and warmed repository HEAD: `52a361c474cc503a293d82c75a98d90063fe8f83`
- Source manifest: `686f1e48de16b268310efc564966353fe422571ec43f459dd3fec66061219e08`
- Fresh immutable rootfs: `fd2fcde2a633ab7c79411ea93ef8970bb419d99dfe62f97bfc427d7a2ede0bb7`
- Snapshot local attestation: `795d59b3d8dd54c110a6149e4b4b82a9133b98b172eb320bb9c225c67ef97dc2`
- Exit status: `1`, from two named performance failures; hard failures: `0`

## Result

| Assertion | Result |
| --- | --- |
| Fresh image and neutral snapshot | pass |
| Unchanged flat restore path and exact reservation return | pass |
| Eight authenticated, workload-ready clones | pass |
| Shared snapshot-memory inode and distinct namespace/veth identities | pass |
| Bidirectional two-hop NAT and root-namespace isolation | pass |
| Cross-clone isolation | pass |
| Per-clone clock, RNG, SSH host key, application key, Git identity, sudo, and one-shot secret hygiene | pass |
| Eight clean witnessed repository workloads | pass |
| Eight distinct witness ports, pcap inodes, and pcap contents | pass |
| Final roster, teardown, protected-state, and leak audit | pass |
| Authenticated fleet readiness `<1s` | fail: `48.607564485s` |
| Fleet PSS `<2×` one-clone PSS | fail: `156,474 KiB` versus `<114,696 KiB` (`57,348 KiB` baseline) |
| Eight independently dispatched `/work-driver` tasks | not exercised; broadcast `git fsck` only |

The kept fleet's late clock-application acknowledgements were `2–7ms`; the witnessed command fleet's were `1–12ms`, all inside the strict four-second post-read freshness bound. `summary.json` records `rooms_subgate_completed: true`, meaning terminal audit was reached rather than that the run passed, and `full_phase2_gate_completed: false`.

## Retained artifact digests

| Artifact | SHA-256 |
| --- | --- |
| `summary.json` | `40b454048f28500d9b03d502a7f047c8d850d5e987e33e9771fa5e6db01ba2cf` |
| `phase2-killer-2026-08-22-summary.public.json` | `88f1d7bb6e697d115204948b57269fed8f97130ffddf76d2cc9c1a63473a4bc6` |
| `witness-manifest.tsv` | `954e436b698025d537613725ef4e5aaf5e6589e60a52cce058d0af186325289d` |
| `fleet-topology.ndjson` | `e0997e42dd2f320c42c4b09aa9c7c93c41d66cdc9af038f58c39eca0a1a6958f` |
| `fleet-guest-evidence.tsv` | `3c106f6a2a3c9cecfe5bf2c1b8f9d9d4292c16f5656b5fb6b117b4ce478a0809` |
| `flat-restore-resource-audit.tsv` | `2a5159a4e06775e0b91b94427416ef8f6659ff872411c4da74f358a7a746eef0` |
| `logs/cleanup.log` | `844ed2edcb110a4002b2c5195a88ed6f6956d79b555c3a312f732d42cec63964` |

The retained `final-ls.json` is exactly `{"schema_version":1,"rooms":[]}` modulo whitespace. The only failure records are `fleet_not_under_one_second`, `pss_density_missed`, and the terminal `killer_gate_failed` summary stating `0 hard and 2 performance failures`.
