# Execute all ten experiments from PR124

Michael assigned all ten experiments to this coordinator, then reaffirmed that scope on September14. The first pilot is partial evidence, not completion of the backlog. Existing authorization: USD50 total GCP; aboutUSD0.20 estimated used. Only this coordinator consumes model usage unless Michael lifts the agent-process hold for the in-Room experiments. No standing monitors or extra project sessions are needed.

Source: PR124 at `f74955eb0a048e1230f573ac60b7644e12a893af`. Each numbered item keeps the original question; a narrower surrogate does not count as its completion. Negative measurements are valid outcomes; missing hardware, quota or implementation remains incomplete.

| # | Experiment | Current evidence | Remaining execution |
|---|---|---|---|
|1|Density ramp|8 clones measured; no density knee|16/32/64/128, host/VMM memory and latency distributions; bare-metal comparison remains separate from nested GCP|
|2|Shared Nix memory|One read-only toolstore works; low-density PSS only|Measure memory/cache at density; evaluate device support and run DAX comparison or the explicitly allowed host-cache fallback|
|3|Cold versus lazy restore|Nested GCP cold/restore1/2/4 comparisons|Disk/tmpfs/hugepage comparisons and userfaultfd loading; bare-metal result remains unmeasured|
|4|Fork-the-world agents|Two fixed candidate patches, separate oracle verifier|4/8/16 actual decision branches, serial baseline, scoped model budget and outcome comparison|
|5|Rooms that travel|Not run|Complete snapshot+disk bundle A→object storage→B; same evidence, transfer bytes and resume timing|
|6|Hostile tenants|One timeout with sibling preservation|64 mixed Rooms: process/memory/disk/egress/store-tamper probes; normal workload degradation and resource audits|
|7|GPU Room|Not run; project global GPU quota0|Validate GPU/VFIO host feasibility, alternative VMM, local inference and client Room; host-only inference is a comparison, not GPU-in-Room completion|
|8|Agents inside Rooms|Deterministic verifier executed in a Room|Scoped credential and mail/receipt endpoint; two agent peers, exact-head receipts and scope-rejection tests|
|9|CI in Rooms|16 Python tests in clones|Shard full pinned Workbench suite across32 Rooms, compare baseline runtime/cost and reproducibility|
|10|Chaos on Spot|Guest timeout only; no host-loss proof|Host killed during work, truthful receipts and checkpoint recovery on replacement; Spot CPU quota currently0 inus-east1|

## Active dependency: density-capable lab build

Production CLI/lease cap8 and `/24` network arithmetic prevent the specified ramp. `scripts/cloud-experiments/density-lab.patch` is an explicit disposable-host research patch on the current source including PR123. It expands clone identities to128 with wider address arithmetic, expands leases/matrix bounds, and sets the lab host cap128. Cold slots remain limited to63. This does not change production defaults, install a binary or authorize deployment. Freeze the patch and binary hashes for every run; run the patched Rust tests before measuring. Preserve clean-host preflight, per-run evidence and cleanup.

Quota observations are capacity checks, not an excuse to omit experiments. Work on other entries while hardware-specific dependencies are resolved. Preserve the USD50 ceiling and actual resource inventory between batches.
