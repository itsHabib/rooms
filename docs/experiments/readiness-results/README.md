# When is a restored Room actually ready?

**Resume acknowledgement precedes usable SSH. Measure both.** The Rust CLI now
reports per-clone timings. The cloud-lab harness retains every sample plus
requested/measured/unavailable counts.

## What we observed

Existing local Lima VZ host: aarch64, 6 vCPU, 4 GiB RAM, nested KVM,
512 MiB guests. Two trials per concurrency, tiny Python arithmetic workload.
All 14 requested workloads completed; all 14 supplied both timestamps.

| Concurrent clones | Samples | Resume acknowledgement, seconds | Authenticated SSH, seconds |
|---|---:|---:|---:|
| 1 | 2 | 0.676–0.687 | 1.200–1.215 |
| 2 | 4 | 0.929–0.960 | 1.656–1.686 |
| 4 | 8 | 2.524–2.664 | 4.454–4.673 |

These are observed ranges, not p99 estimates. Timing begins at each clone's
dispatch **after shared preparation and network allocation**. Resume acknowledgement
means the guest hygiene handshake completed. SSH timing includes waiting for the
batch restore barrier and an authenticated SSH probe; it is an upper bound on when
SSH became available, not a continuously observed transition.

A separate intentional exit-7 workload reached SSH at 1.264 seconds but failed.
A commandless kept clone reached SSH at 1.250 seconds and was explicitly killed.
Final inventory had zero Rooms, Firecracker processes or network namespaces.
The new sealed snapshot reservation remains for reproduction. The existing Lima
host was stopped after export. No paid cloud
resources or model calls were used.

## Repeat it

Use a Linux KVM host, current Alpine image and matching sealed toolstore.
Follow the [cloud lab setup](../cloud-lab.md), then:

```sh
cargo build --release --locked --bin rooms --example cloud-lab
sudo -E target/release/rooms base-create --image "$IMAGE" --toolstore "$TOOLSTORE" --memory 512 --warm true --json > base-receipt.json
# Set ROOM_ID from base-receipt.json.
sudo -E target/release/rooms snapshot "$ROOM_ID" --out "$SNAPSHOT" --json > snapshot-receipt.json
sudo -E target/release/examples/cloud-lab --rooms target/release/rooms --snapshot "$SNAPSHOT" --image "$IMAGE" --toolstore "$TOOLSTORE" --command-file workload.sh --counts 1,2,4 --repeats 2 --wall-seconds 45 --out "$RESULTS"
```

Use disjoint snapshot and output directories. The archived workload sets the
toolstore PATH and asserts a small Python result. Repeat with an exit-7 command
to check that readiness does not erase execution failure.

## Lessons

- **Start the clock at a named boundary.** These fields exclude admission and
  shared preparation. Keep those costs separate.
- **Ready is not done.** Preserve workload exit status alongside readiness.
- **Missing is not fast.** Old CLI output and pre-readiness failures retain the
  requested denominator and mark missing samples unavailable.
- **Check the image and host first.** An older local image failed toolstore setup;
  rebuilding with the current Alpine builder worked. Host forwarding also needed
  the existing setup script after VM startup. Both diagnostics are retained.
- **Keep receipts outside snapshot roots.** Naming a CLI receipt
  `snapshot.json` in the output ancestor tripped the existing snapshot protection
  check. The refused first ramp is retained separately from the completed ramp.

## Receipts and remaining work

[Summary](summary.json) · [source/binary provenance](provenance.json) ·
[raw receipts](evidence.tar.gz)

Archive SHA-256:
`dda6b0e9969b390772ad1c2d9a11fc7696074f55c90e90f8e94d362d9ea64845`.

The archive includes raw CLI outputs, per-Room results/logs, command inputs,
host/memory audits, the failed workload, kept-clone cleanup and setup diagnostics.
It excludes VM memory, disk images and credentials. Source hashes match code
commit `180aa087fef961b634a4af06dc83e9572f7d1680`; later changes package docs
and add the example tests to normal CI.

This closes the missing instrumentation, not experiments 1 and 3. Still needed:
full admission-to-ready timing, a controlled cold/File/UFFD comparison with these
measurements, larger repeated workloads, and bare-metal measurements.
