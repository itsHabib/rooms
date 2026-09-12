# Validation — 2026-09-12

Host: the existing `rooms-host` Lima VM on Apple Silicon; Ubuntu aarch64,
Firecracker 1.15.0, guest kernel 6.1.155. The image was rebuilt using this branch's
Alpine builder and passed its Claude binary smoke gate. No model calls were made.
Image SHA-256: `5632625fdccdf4f6495834aa9124d3ddde96524e4c1e8432d0acafd9bca59841`.

`make check` passes on macOS (fmt, all-target/all-feature clippy, portable tests).
The standalone real-host proof uses `scripts/test-command-rooms.py`, with the
branch's Linux binary, that image, and a separate short HOME with the existing
Rooms SSH key. A short HOME avoids the existing Unix socket path-length limit.
After starting the previously stopped host, its ephemeral firewall was restored
using `scripts/setup-tap.sh --host`.

The first four-case run passed:

| Case | Result | Evidence |
| --- | --- | --- |
| Repository command | succeeded, 0 | 2 vCPUs, ~1 GiB RAM, `/dev/vdb` ext4 overlay; Rooms repo at `e8c4504`; shell syntax check; 768 MiB written; patch exported |
| Fresh room / failed command | failed, 7 | Prior room's files absent; edit from failed command exported |
| Wall timeout | timed_out, 124 | Both partial log markers retained |
| SIGTERM | cancelled, 143 | Both partial log markers retained |

All four ended with `cleanup_done`, no collection/cleanup failure events, and
artifacts owned by the sudo caller. The shared image hash was unchanged.
The 768 MiB write took 48.60 s (~15.8 MiB/s) on this nested host: this is capacity
and lifecycle evidence, not a throughput improvement claim.

The checked-in script also tests an output path that is an existing regular file:
collection must fail, preserve that file, and still finish cleanup. The final
run's result is recorded on the PR. This is targeted cold-run validation; it does
not qualify snapshot restore with extra drives, x86_64, hostile-guest patch
verification, or a complete Fleet/agent workflow.

Review fixes were revalidated with the full smoke sequence. A locked Git index
now returns CLI error 2 while preserving command exit 7, its failed status and
logs in result.json, without claiming a patch exists. Rejected symlink archives
leave caller-owned changeset diagnostics. Both a deliberately stalled formatter
and a stalled Firecracker API client were cancelled with SIGTERM: exit 143,
no boot/workload handoff, and no child process, slot or jail residue. The existing
pre-feature Alpine image was separately rejected by --disk before a slot claim.
