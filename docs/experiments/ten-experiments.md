# Rooms experiment tracker: original ten plus three additions

Michael assigned all ten experiments to this coordinator, then reaffirmed that scope on September14. The first pilot is partial evidence, not completion of the backlog. Existing authorization: USD50 total GCP; actual bill pending; finite cloud hosts bound spend below the authorization. Only this coordinator and the separately requested Relay/Workbench design task may consume model usage. The in-Room agent-process question remains open. No standing monitors or extra project sessions are needed.

On September 14 Michael added three further experiments: NVMe/LVM storage and cache behavior, eBPF tracing, and telemetry/UI for deployments. Their prototype plans are [here](storage-observability-deployments.md); all three are planned, not yet built. The original ten retain their numbering and unfinished scope.

Source for experiments 1–10: PR124 at `f74955eb0a048e1230f573ac60b7644e12a893af`. Each numbered item keeps the original question; a narrower surrogate does not count as its completion. Negative measurements are valid outcomes; missing hardware, quota or implementation remains incomplete.

| # | Experiment | Current evidence | Remaining execution |
|---|---|---|---|
|1|Density ramp|Nested GCP8/16/32/64/128 twice:496/496 passed; useful throughput peaks around32|Published memory/command distributions; bare-metal comparison and admission-to-ready distribution remain unmeasured|
|2|Shared Nix memory|Fallback measured at16/32/64:112/112 passed,182.5MiB store cached once; guest cache still duplicates memory|Published retained PSS/cache samples and interpretation; no DAX result claimed|
|3|Cold versus lazy restore|Nested cold/restore plus nine File and nine UFFD backing/trial comparisons; UFFD passed but slower|Published loader lifetime failure/fix and matched-integrity comparison; bare-metal and readiness distributions remain unmeasured|
|4|Fork-the-world agents|Two fixed candidate patches, separate oracle verifier|4/8/16 actual decision branches, serial baseline, scoped model budget and outcome comparison|
|5|Rooms that travel|371MB bundle through private object storage; two second-host resumes passed16tests and identical patch SHA|Receipts and reproduction guide published; measured only on matching Cascade Lake hosts|
|6|Hostile tenants|64 mixed:32/32 normal passed;8diskENOSPC,4storeEROFS,4egress failures; memory7timeouts+1OOM; cleanup|Positive observe/none/observe endpoint control passed; normal p99 rose to21.4s vs11.1s32-normal baseline. Receipts published; no general multi-tenant qualification|
|7|GPU Room|Real G2/L4 probe: KVM works, no IOMMU; VFIO bind and Cloud Hypervisor launch fail; GPU host deleted|Needs a host exposing working IOMMU/VFIO before isolated inference; no GPU-in-Room result|
|8|Agents inside Rooms|Deterministic verifier executed in a Room|Scoped credential and mail/receipt endpoint; two agent peers, exact-head receipts and scope-rejection tests|
|9|CI in Rooms|Pinned current Workbench100packages, Go1.26.7 and offline cache; two32-shard runs64/64green88.535/90.190s|Additional7/7 adapter/fuzz/corpus/build/vet cases passed; baselineGo test118s, different hardware/preparation; receipts published|
|10|Chaos on Spot|Spot host deleted after patch application before tests; SSH255, no false completion; Fleet checkpoint retained|Fresh Spot hostd replay passed twice with same patch/16tests; receipts exported, replacement host deleted. Coordinator used Fleet checkpoint; no autonomous watcher failover claimed|
|11|NVMe/LVM storage and caching|Planned; operator reports earlier local PV/LV work, implementation not yet located|Recover prior work; compare ordinary files, logical volumes and justified thin/cache variants on comparable hardware; build fixture, measurements and recovery POC|
|12|eBPF diagnostics and tracing|Planned; existing agent-tracing proposal and historical kernel probe found|Recheck kernel support; build useful trace collection with identity/loss reporting, measure overhead and demonstrate failure diagnosis|
|13|Deployment telemetry and UI|Planned; existing lifecycle/registry/witness records and cloud receipts provide initial data|Build deployment overview and Room detail pages, telemetry adapter, live connection and failure-flow browser tests|

## Active dependency: density-capable lab build

Production CLI/lease cap8 and `/24` network arithmetic prevent the specified ramp. `scripts/cloud-experiments/density-lab.patch` is an explicit disposable-host research patch on the current source including PR123. It expands clone identities to128 with wider address arithmetic, expands leases/matrix bounds, and sets the lab host cap128. Cold slots remain limited to63. This does not change production defaults, install a binary or authorize deployment. Freeze the patch and binary hashes for every run; run the patched Rust tests before measuring. Preserve clean-host preflight, per-run evidence and cleanup.

Quota observations are capacity checks, not an excuse to omit experiments. Work on other entries while hardware-specific dependencies are resolved. Preserve the USD50 ceiling and actual resource inventory between batches.

## Share and reuse

See [the cloud lab guide](cloud-lab.md), the Rust `examples/cloud-lab.rs` runner,
and retained recipes in `scripts/cloud-experiments/recipes-20260914`. Michael
explicitly requested that the code, failures, receipts and lessons be reusable by
colleagues. Keep README links and per-experiment limits current as evidence lands.

Final lab inventory is clear; see [results, raw receipts and cleanup](2026-09-14-cloud/README.md). No paid test hosts remain.
