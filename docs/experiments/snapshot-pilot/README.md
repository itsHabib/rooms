# Snapshot fan-out on GCP: measured pilot

Rooms source `4c62611661f377b9c725c3d2e59e65b6e9bdc4bb` (PR123), built on Ubuntu24.04, nested KVM on one GCP `n2-standard-16` (16 vCPU,64GiB), `us-east1-b`. Guests:2vCPU,1024MiB, sealed Python Nix store. No model sessions or model API calls were run.

**Outcome:** snapshots roughly halved end-to-end time for this small real patch-and-test workload at matched concurrency1/2/4. Eight clones finished together in about7.46seconds. Candidate failure and guest timeout both preserved a successful sibling's result and cleaned up runtime resources.

## Timings

Each invocation applies the same frozen Workbench patch at base `92a706a7982a527ade967e43b68afd4bc1e5d667`, runs16 Python tests, and exports its patch. All successful baseline executions returned byte-identical patches. Snapshot commands additionally assert a clean pinned checkout and absent sentinel, record guest identity, and explicitly export the patch. Cold commands use the cold runner's repository setup/export. This compares those complete supported paths, not instruction-identical shell programs.

| Concurrent jobs | Cold median, seconds | Snapshot median, seconds | Repetitions per path |
|---|---:|---:|---|
|1|12.973|6.296|3|
|2|13.487|6.488|2|
|4|13.776|6.770|2|
|8|Incomplete:7/8 admitted|7.457|Cold1 incomplete; snapshot2|

Times include CLI execution, collection and cleanup; concurrent cold batch times also include lightweight collector overhead. These are tiny samples of one workload, not p95/p99 or a general capacity benchmark. The original suite completed34/34 workload executions. Additional cold batches completed19/20 attempts; one admission refusal remains recorded.

Base preparation17.095s plus snapshot creation17.234s = **34.329s upfront**, excluding one-time image/toolstore/host builds. At the measured single-job difference, preparation pays back after about6 reuses. Eight-way snapshot throughput was64.1–64.7 completed executions/minute during those short batches; no eight-way cold speedup is claimed.

## Branch, reject, verify

Two fixed candidates ran from the same neutral snapshot. The original succeeded. A seeded mutation changed `all(SUCCESS)` to `any(SUCCESS)` and failed3 of16 tests. Matrix CLI exit1 correctly retained both result directories. Candidate batch:6.532s.

A separate restored Room consumed the selected patch with SHA256 `83da8be43c9032f1e44bceeae7a3af135ffe7934ec722963c6d99031feac6c87`. It passed the16 tests and an independently authored exhaustive oracle for341 combinations of0–4 checks with SUCCESS/FAILURE/SKIPPED/UNKNOWN. Verifier:6.428s. This demonstrates artifact handoff and independent execution with an extra oracle; it is not independent-agent review, generated candidate quality, or general semantic certification.

The first matrix submission was rejected before admission because an uncompressed inline patch exceeded the16KiB command limit. The retry gzip-compressed that same patch. Both attempts are retained.

## Failure and capacity observations

A second matrix ran a fast receipt writer beside `sleep120`, with a10s guest wall limit. The slow guest exited124; the matrix CLI exited124 after12.362s including teardown. The successful sibling's `survived` receipt and exit0 remained available. Post-run audits found no Firecracker processes, jail mounts, network namespaces, TAP/veth devices, live Rooms or restore intents.

A retained snapshot reserves slot1 (`@reservation 01m2gfahdh0f1kj6xjn7qggzze`). The cold pool cap is8, leaving7 cold slots. Eight namespace-isolated clones can share the snapshot reservation, while eight concurrent cold admissions cannot all fit. The eighth cold attempt reported `pool_full`; all seven admitted jobs completed. This is a capacity/visibility limitation of the present design, not proof of a race or a cloud resource ceiling. The cold harness stopped after this failed batch and did not write its aggregate wall time; individual attempt timings/results are retained, and no aggregate time is invented.

Harmless-looking duplicate jail-removal warnings also appeared (`No such file or directory`); resource audits were clean. These should be traced before suppressing logs. Snapshot reservation and lock files intentionally remained until host deletion; they are not running guests.

The sampler observed8 simultaneous Firecracker processes. Peak summed VMM proportional set size over the initial suite was381.8MiB. This excludes separately held page cache, kernel and other host allocations and reflects a small workload; it does not establish total memory per Room or a density limit. Concurrent cold follow-up memory was not sampled.

## Evidence and reproduction

Adjacent JSON files contain measured comparisons, frozen hashes and post-run audits. The complete2.9MiB compressed evidence packet includes full stdout/stderr, lifecycle events, every failed attempt, output patches, identities, memory samples and exact Python harnesses. Packet SHA256:

`c8ee2e3f2aa73b975ffe92849ae4847b4e3b32fc0b568cebbf09ff6fc4114901`

Mac packet: `/Users/mh/dev/rooms-cloud-experiments-20260914/evidence.tar.gz`; unpacked under `evidence/`. It contains no cloud credentials or guest private keys. The original local artifact is now also published, recompressed without changing its tar contents, as [first-pilot-evidence.tar.xz](../cloud-results/first-pilot-evidence.tar.xz). See the adjacent SHA256SUMS for the recompressed archive. `bootstrap.sh` and provider readbacks live alongside the packet. The historical harness is fixed to `/home/rooms`, requires root on a disposable Linux host and the frozen patch, and refuses existing run directories. It is not a general cloud launcher.

The `$50` project alert budget is not a Compute spending cap. This batch used one host with a3h automatic DELETE deadline, auto-delete disk, ephemeral IP, no service account and operator-IP-only SSH. The host and temporary subnet were deleted after about15minutes. At17:36:53UTC, project inventory contained zero instances, disks, reserved addresses or cloud snapshots. Estimated fixed cost aboutUSD0.20; actual billing, transfer and tax are not reconciled. The local `cleanup.json` records this observation. No other agents/reviewers/monitors were resumed.

## Next useful work

1. Make pool reporting explain snapshot reservations and available cold capacity. Avoid another configuration layer unless a measured workload needs it.
2. Turn the frozen branch/verify exchange into the existing headless Fleet workflow, binding actual agent output to the base and patch digest. Keep credentials outside captured snapshots.
3. Test interruption during publication and recovery on a second host, including stale-result rejection and cold reconstruction. This pilot did not test migration, host loss, live-agent continuation, adversarial network containment or GPUs.
