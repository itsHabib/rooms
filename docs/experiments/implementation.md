# Rooms experiments: implementation

Owner: Michael's coordinator session. Other agent/model workers and monitors remain paused. Michael subsequently authorized up to USD50 GCP for this batch; other model sessions remain paused.

The first implementation is `scripts/experiment-report.py`: an offline reader for the existing cold-run artifact layout. It does not launch commands or infer semantic correctness from tests. It counts execution completion only when CLI, guest result, collection and cleanup agree. Incomplete attempts stay in the denominator. Raw evidence hashes make each reported attempt inspectable.

Run `python3 scripts/test-experiment-report.py` for local tests. Run `python3 scripts/experiment-report.py manifest.json` to print a report. Paths resolve relative to the manifest:

```json
{
  "schema_version": 1,
  "label": "cold, same pinned workload",
  "preparation_seconds": 0,
  "batches": [
    {"name": "cold-1", "concurrency": 1, "wall_seconds": 18.288108005,
     "attempts": ["headless-cold-e5d16e5d3c2b"]}
  ]
}
```

Each attempt directory contains `summary.json` with collector `cli_exit` and monotonic `elapsed_seconds`, `out/result.json`, and `lifecycle.ndjson`. Missing result/lifecycle evidence counts incomplete; missing collector timing or malformed evidence rejects the report instead of inventing timings. Batch wall time is measured around the complete batch, not summed across parallel attempts. Preparation is charged once; the manifest must list sequential, nonoverlapping batches. Keep different machines/workloads in separate manifests. Include every launched attempt, including failed launches with a collector summary. Record workload, binary, image and store identities alongside the manifest. This reader cannot establish that an operator supplied every attempt or measured wall time honestly.

Cost is optional, `estimated_total_cost_usd`, and remains explicitly estimated. No p99 or memory savings claim is extrapolated from a few samples. Execution completion is separate from independent verification of a useful result.

## Delivery order

1. **Evidence reader (implemented):** test against the retained real cold baseline, then use the same collector format for cold/restored batches.
2. **Density/cold/restore pilot (run):** See [GCP snapshot pilot results](snapshot-pilot/README.md). The fixed host harness and raw evidence are retained locally. A generalized launcher is not implemented. Further work: reuse existing Rooms commands and #123 toolstore snapshot support. Pin one workload; preserve exact argv, full CLI interval, exit and lifecycle for each attempt. Start1/2/4/8 within actual memory/pool capacity. Report warm/cold separately and include snapshot preparation. Do not exceed63network slots without changing the allocator.
3. **Branch and verify:** fixed candidate patches for one real task, sequential/worktree/cold/restore comparison, independent verifier consuming the exact published patch digest and base. Exercise partial publication, duplicate delivery and restart. Same total model budget when comparing candidate quality.
4. **Agents inside Rooms / sharded CI:** start with one verifier, then useful independent test shards. Scope provider credentials and keep them out of captured snapshots/artifacts.
5. **Remote migration / spot recovery:** compatible hosts, complete snapshot artifact set, cold reconstruction comparison, late old-host result rejection. Remote cleanup must distinguish unreachable old processes from reconciled cloud resources.
6. **Memory sharing / hostile workloads:** diagnose host cache and guest duplication before a new storage device; run concrete resource/isolation probes at measured density. Probe success is not general containment proof.
7. **GPU:** compare a host model service first; another VMM is a separate experiment if actual device requirements demand it.

The broad backlog stays in PR124. The coordinator's detailed proposed corrections are Mac-only `/Users/mh/dev/ROOMS-124-REVIEW.md`. GCP execution within the explicit USD50 batch budget is authorized; no repeat approval is needed. Model reviewers and other sessions remain paused. Record actual provisioning, estimated cumulative cost, evidence and cleanup before another batch.
