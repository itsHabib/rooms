# Reproducing the Rooms cloud lab

This is the testing harness used to turn the ten cloud ideas into measured runs.
It is experimental. Reuse the runner and receipts for your own workload; do not
copy a benchmark number without its machine, workload, revision and preparation
cost. The runtime and lab are Rust. Provider setup and the retained first-run
recipes use shell and Python where they already exist.

## Repeat a workload with the Rust runner

Use an otherwise idle Linux host with KVM, the Rooms jailer stack and an already
prepared neutral snapshot. Follow [host setup](../preflight.md) and the
[snapshot contract](../features/snapshot-fork-replay/spec.md). Attach the same
sealed image and Nix toolstore used to create the snapshot.

```sh
cargo build --release --locked --example cloud-lab
cargo test --locked --example cloud-lab
sudo target/release/rooms doctor --image /srv/rooms/images/agent.ext4 --json
mkdir -p /srv/rooms/lab
cat > /srv/rooms/lab/workload.sh <<'SH'
set -eu
export PATH=/nix/var/rooms/env/bin:$PATH
cd /workspace/repo
python3 -m unittest discover -s tests
SH
sudo target/release/examples/cloud-lab \
  --rooms "$PWD/target/release/rooms" \
  --snapshot /srv/rooms/snapshots/neutral \
  --image /srv/rooms/images/agent.ext4 \
  --toolstore /srv/rooms/toolstores/python \
  --command-file /srv/rooms/lab/workload.sh \
  --counts 1,2,4,8 --repeats 2 --wall-seconds 180 \
  --out /srv/rooms/lab/my-first-ramp
```

Replace the example workload with a command that exists in your frozen checkout.
The output directory must be new. For patch-producing workloads,
`--expected-patch-sha256 <digest>` checks each collected patch against an
independently selected expected artifact. A successful command alone does not
establish that the patch is right.

The runner uses the runtime's per-workload wall limit. It stops the ramp after
an incomplete batch and retains evidence. It does not start agents, decide which
candidate wins, or provision cloud resources. A host-level interruption still
requires the provider cleanup and recovery steps below.

## What to retain

Each run writes:

- `inputs.json`: binary, image, toolstore, snapshot and command digests; machine
  facts; the hashing preparation cost and requested ramp.
- `clones-*/argv.json`, `stdout.json`, `host.log`: the exact invocation and raw
  CLI output, including failures.
- `clones-*/out/<room>/`: the original collected guest result, logs and patch.
- `clones-*/memory.ndjson`: timestamped host memory and each live VMM's PSS/RSS
  source data. A process that disappears during sampling is marked unavailable.
- `host-*.json`: live Room/VMM inventory, namespaces, interfaces and mounts.
- `summary.json`: CLI status, actual receipt count, expected-patch checks,
  sampling errors, batch wall time and useful completions per minute.

Keep preparation separate from reuse. Batch wall time is not per-job p99.
Guest `started_at` to `ended_at` measures command execution, not host admission
or guest readiness. A Firecracker resume log occurs before the hygiene handshake;
it must not be labeled workload-ready. Small repeat counts are descriptive, not
stable tail-latency estimates. Record missing measurements explicitly.

The harness verifies that no live Rooms/VMMs or lab network/jail remnants remain.
Snapshot reservations are intentionally durable and do not mean a VM is running.
Preserve the full before/after files even when the summary is green. Execution
checks are not an independent semantic verdict or a security qualification.

## Renting a disposable GCP host

Use an explicitly selected lab project, a subnet you own and an SSH firewall
limited to your source address. Every command should name `--project` and
`--zone`; do not rely on whichever project happens to be globally selected.
The September 14 lab used Ubuntu 24.04, nested KVM and Intel Cascade Lake. Pin a
compatible CPU platform on both hosts for the snapshot travel experiment.

The relevant lifecycle options on the measured hosts were:

```text
--enable-nested-virtualization
--no-service-account --no-scopes
--max-run-duration=3h --instance-termination-action=DELETE
--maintenance-policy=TERMINATE --no-restart-on-failure
```

For the replacement host, the lab also used `--provisioning-model=SPOT` and a
two-hour maximum duration. Check actual machine availability and prices before
provisioning. A billing budget alert does not stop Compute Engine: the provider
deadline and an inventory-based cleanup are what bound unattended resources.
Retain the actual instance response, disks, machine shape and creation/deletion
timestamps. Mark cost calculations as estimates until billing data arrives.

Run the repository's setup scripts and `make check` before measuring. The
128-clone research build additionally applies the three retained patches under
`scripts/cloud-experiments/`: density bounds, fixture expectations and complete
network geometry. Run the patched tests too and retain each patch/binary hash.
Do not change production defaults just to make a chart look better.

## Moving one frozen Room to another host

The tested bundle contains `snapshot.json`, `snapshot.vmstate`, `snapshot.mem`,
the exact backing image, the toolstore image/metadata, the pinned guest kernel
and a checksum manifest. This snapshot uses an immutable base plus private guest
state in memory; there is no separate writable disk diff to invent.

Upload to a private, temporary object-storage bucket. On the destination, verify
the bundle hash before extraction, reject unsafe archive paths, verify every
manifest entry, restore the memory file's private Firecracker ownership/mode and
seal the files and directories. The frozen slot must then be explicitly adopted:

```sh
cargo build --release --locked --example lab-import-snapshot
sudo target/release/examples/lab-import-snapshot \
  /srv/travel/snapshot /srv/travel/images/agent.ext4 /srv/travel/python
```

The example uses normal restore validation, hashes the sealed input and claims
the snapshot's exact slot. It refuses an occupied slot. Use it on the fresh
destination, once; it is not an overwrite or merge operation. The destination
also needs the guest SSH identity through a separate protected transfer. Do not
put private keys, signed URLs or model credentials in the public evidence packet.

Two measured restores on the second host produced the same selected patch hash
and passed the same 16 tests, with fresh runtime identities. The 371,472,408-byte
bundle uploaded in 3.335s and downloaded in 1.599s. Packing took 18.085s; destination
unpacking/hash verification 8.435s; slot adoption 3.470s; subsequent restores 9.079s
and 9.578s. These stages exclude host provisioning and do not establish arbitrary
CPU portability or recovery of an interrupted external side effect.

## Recovering after the host is deleted

The Spot test wrote a Fleet checkpoint outside the worker host, naming the
snapshot/object hash, repository revision, expected patch and unfinished attempt.
After observing `patch-applied-before-tests` and the absence of `result.json`,
the coordinator deleted the Spot instance. SSH exited 255. That attempt stays
`interrupted`, with no successful completion receipt.

A fresh matching Spot host downloaded and verified the same checkpoint, adopted
its frozen slot and replayed the task. Two recovery runs passed 16 tests and
reproduced the same patch hash, taking 9.273s and 9.302s after setup. The saved
checkpoint predates task execution: uncheckpointed progress was replayed, not
magically recovered. This was an injected deletion on Spot capacity, not a
naturally observed preemption. The coordinator read Fleet's durable handoff;
automatic watcher-driven host replacement was not implemented by this run.

## Lessons already worth sharing

- **Network geometry is one system.** Raising clone counts required consistent
  allocator, route, CIDR and firewall validation changes. The first 64-clone run
  correctly refused an inconsistent setup. Keep that failed run beside the fix.
- **Shared storage is not shared guest cache.** The compressed 182.5MiB store was
  resident once in the host cache, but full scans still left roughly 661MiB peak PSS
  per VMM at 64 guests. The virtio-blk fallback is not a DAX result.
- **Copying bytes is not enough.** Snapshot memory must retain private ownership
  and mode before sealing. Copied snapshots also pay full image hashing; compare
  them against other copied inputs, not a locally attested fast path.
- **A lazy loader lives as long as the VM.** The initial userfaultfd prototype
  released its handler at readiness and the later workload hung. The failed
  attempt is retained. The corrected variant passed all nine backing/trial
  combinations, but took 9.7–10.0s versus 8.7–9.1s for copied file-backed inputs on
  this workload. That result supports keeping the simpler file-backed default.
- **KVM does not imply GPU passthrough.** A GCP G2/L4 host exposed nested KVM and
  the GPU PCI device but no IOMMU groups. Normal VFIO binding and a Cloud
  Hypervisor launch failed. No GPU-in-Room inference was established.

## Finish and share

Export receipts before deleting a host. Hash the archive and verify the downloaded
copy. Delete the owned instances, check auto-delete disks actually disappeared,
remove temporary storage objects/buckets and identities, then verify project
inventory. Keep failed attempts and the cleanup receipt in the results report.

Publish a short result for each of the [ten experiments](ten-experiments.md): the
question, reproduction command, source/input identities, observation, limitation
and next action. Link the result and usable guide from the README. A colleague
should be able to reproduce the behavior without this conversation or your cloud
credentials. All ten remain the scope; a hardware or authorization blocker stays
visible rather than being counted as a completed substitute.

## Running a repository's CI inside Rooms

Freeze the repository head and its complete package list first. The Go experiment
used an offline Nix toolstore containing Go1.26.7, gcc, `gh` and the frozen module
cache. Set `GOPROXY=off`, `GOSUMDB=off` and `GOTOOLCHAIN=local` in the guest, warm a
neutral base without changing its checkout, then snapshot it. `go list ./...`
produced100 unique packages; striding that list into32shards preserved every
package exactly once. Each matrix case ran:

```sh
go test -race -count=1 -json \
  -coverprofile=/workspace/out/coverage.out -covermode=atomic <shard-packages> \
  > /workspace/out/test-events.ndjson
```

Retain the matrix cases, original package list, result receipts, test events and
coverage files. A complete receipt count alone is insufficient: each exit status
must pass. Do not count a module-cache preparation or compilation warm-up as a
successful test run. Include base creation and snapshot costs when evaluating a
one-off job; repeated jobs may amortize them.

The successful Go runs excluded extra workflow stages from their88–90s timing.
Adapter, fuzz and corpus/build/vet checks are separate cases in the retained
`ci_extra_v2.py` preparation and `ci_extra_v3.py` execution recipes. Its Codex CLI is pinned and used for offline rule checks;
it does not start model inference. This is still not an identical recreation of
all GitHub Actions services, lint jobs or artifact uploads.

A newly built rootfs must have its pinned `vmlinux.bin` alongside it. Warm stdout
belongs in diagnostic stderr, not the provisioning ACK channel. The lab first
worked around that by redirecting warm logs, then fixed the actual guest agent
and added a regression covering successful and failed warm commands.
