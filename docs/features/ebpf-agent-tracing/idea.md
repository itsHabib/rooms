# eBPF agent tracing — idea

**Status: idea / unscheduled (2026-08-08).** Operator note for a future session:
see this through or at least run the experiment. Not a spec — write one
(`spec.md` here) before building.

## Pitch

Trace what the agent *actually did* inside a room at the kernel level: exec,
file opens, and outbound connects, streamed out of the guest as they happen.
Today the only in-guest record is `rooms diff`'s post-hoc overlay enumeration,
which the trust-boundary section of the rooms-diff spec already concedes is
forgeable by an adversarial root guest. A kernel-side event stream is the
complementary signal: append-only on the host as it arrives, so a compromised
agent can stop *future* events but cannot retract what already streamed.

## Sketch

- Tiny eBPF tracer baked into the agent rootfs, loaded by init **before** the
  agent process starts: tracepoints on `sched_process_exec`, `openat`,
  `connect` (filter to the agent's cgroup/uid), ring buffer to a userspace
  shipper.
- Shipper forwards events over **vsock** to the host (the vsock-secrets work
  already gives rooms a vsock lane), host appends to `trace.ndjson` next to
  the existing `--out` artifacts.
- Consumers: `rooms diff` cross-check (did the trace see writes the changeset
  omitted?), tracelens (agent-behavior forensics), gate (evidence artifact).

## Constraints discovered up front

- Host-side eBPF cannot see into the guest — Firecracker guests run their own
  kernel, so the tracer must live in-guest. Host-side you only get the
  firecracker process + tap traffic.
- The custom guest kernel needs BPF support compiled in (CONFIG_BPF_SYSCALL,
  tracepoints, and BTF if we want CO-RE); check the current vmlinux config
  before anything else — this is the cheapest kill-or-confirm step.
- Root guest agent can kill the tracer. That's acceptable: the goal is raising
  the forgery bar and making tampering *visible* (stream stops = loud signal),
  not a containment boundary.

## Experiment result (2026-08-08)

Ran the kill-or-confirm check against the current guest kernel
(`~/rooms/images/vmlinux.bin`, ARM64 Image 6.1.155, via `extract-ikconfig` on
the Lima rooms-host):

- **Present:** `CONFIG_BPF=y`, `CONFIG_BPF_SYSCALL=y`, `CONFIG_CGROUP_BPF=y`,
  `CONFIG_PERF_EVENTS=y`, `CONFIG_VIRTIO_VSOCKETS=y` (vsock lane confirmed
  in-guest).
- **Absent:** `CONFIG_FTRACE` is not set — so no tracepoints, no
  `CONFIG_BPF_EVENTS`, no kprobes, no `CONFIG_DEBUG_INFO_BTF` (no CO-RE), no
  BPF JIT.

Verdict: not killed, but the sketch's hook points don't exist in the current
kernel. Prerequisite phase zero for the spec: rebuild the guest kernel with
`FTRACE`, `TRACEPOINTS`/`BPF_EVENTS`, `KPROBES`, `BPF_JIT`,
`DEBUG_INFO_BTF`.

## Why it's worth a phase

Rooms currently manufactures isolation + attribution evidence; this adds
*behavioral* evidence no other plane can produce. It is also the operator's
stated learning track (eBPF): implementation language open — Aya (Rust, fits
the repo) vs libbpf-rs vs a Go cilium/ebpf sidecar; decide in the spec.
