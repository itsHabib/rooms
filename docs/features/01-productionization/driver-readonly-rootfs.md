---
driver_version: 1
generated_at: 2026-06-19T07:00:00Z
generated_by: claude-code:michael
source:
  project: rooms
  phase: 01-productionization
repo: rooms
repo_url: https://github.com/itsHabib/rooms
branch_prefix: prod-
default_runtime: cloud
batches:
  - id: 1
    label: readonly rootfs overlay (single stream)
    depends_on: []
    status: pending
    streams:
      - task_id: tsk_01KSDNM7D0RQH6J823RFZ1S9EJ
        task_slug: readonly-rootfs-with-overlay
        spec_path: docs/features/readonly-rootfs-with-overlay/spec.md
        runtime: cloud
        touches:
          - src/firecracker.rs
          - scripts/build-rootfs-alpine.sh
          - scripts/lib/overlay-init.sh
        status: pending
runtime_notes:
  merge_gate: rooms-host-e2e
  applies_to: all streams
  decided: 2026-06-19
  approach: "init-wrapper (/sbin/overlay-init + init= boot arg); NOT initramfs"
  policy: |
    Cloud agent writes code + unit tests + opens a PR + runs the 4-bot review.
    The driver drives the PR to review-clean but PAUSES before merge for the
    operator's rooms-host e2e: RO rootfs + tmpfs overlay boots, sshd up, `mount`
    shows overlay as /, write-then-reboot-gone, host rootfs mtime stable. Cloud
    VMs have no KVM/Firecracker, so acceptance can only be verified on rooms-host.
skipped_during_resolution: []
---

# Read-only rootfs driver manifest (rooms 01-productionization)

Single-stream drive for `readonly-rootfs-with-overlay` (init-wrapper approach).
Spec: [`docs/features/readonly-rootfs-with-overlay/spec.md`](../readonly-rootfs-with-overlay/spec.md).
The `secret-injection-via-vsock` task is parked (stale premise — keys already
reach the guest via SSH `SendEnv`, not the kernel cmdline). Merge gate: rooms-host
e2e, operator-confirmed.
