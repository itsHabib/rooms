---
name: Bug report
about: Report something that isn't working as expected
title: ''
labels: bug
assignees: ''
---

## What happened

<!-- Describe the actual behavior you observed. Include command output or logs if helpful. -->

## What you expected

<!-- What should have happened instead? -->

## Reproduction steps

1.
2.
3.

## rooms-host setup

<!-- rooms runs on an Ubuntu host with Firecracker + KVM — not on macOS/Windows directly. -->

- **Host OS** (e.g. Ubuntu 24.04 on Hyper-V):
- **KVM / nested virtualization** (`ls /dev/kvm` output, or how you enabled nested virt):

## Kernel + Firecracker versions

- **Kernel** (`uname -r` on the rooms-host):
- **Firecracker** (`firecracker --version`):
