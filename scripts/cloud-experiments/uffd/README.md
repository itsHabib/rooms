# Disposable userfaultfd handler

Adapted from Firecracker v1.15.0's `src/firecracker/examples/uffd/` under the
included Apache-2.0 license. Upstream source:
https://github.com/firecracker-microvm/firecracker/tree/v1.15.0/src/firecracker/examples/uffd

The modifications add bounded accept/read/poll waits and page-fault counters.
The separate `../uffd-lab.patch` changes only the disposable measurement build:
it starts this static handler inside the jail under the Firecracker uid/gid,
and retains the handler for the complete command lifecycle. `--keep` is refused
by that lab variant; no production option or default changes.

Build with the checked-in Cargo.lock on Linux:

```sh
rustup target add x86_64-unknown-linux-musl
cargo build --release --locked --target x86_64-unknown-linux-musl
```

Requires musl tools, clang/libclang and Linux UAPI headers. On Ubuntu, the lab
used a local include directory pointing at `/usr/include/linux`,
`/usr/include/asm-generic` and `/usr/include/x86_64-linux-gnu/asm`, passed as
`CFLAGS_x86_64_unknown_linux_musl="-isystem /path/to/kernel-headers"`.
Install the resulting binary at `/usr/local/libexec/rooms-lab-uffd` in the
DISPOSABLE host. Its `/dev/userfaultfd` is accessible only to the Firecracker
user. Follow the upstream jail/device requirements:
https://github.com/firecracker-microvm/firecracker/blob/v1.15.0/docs/snapshotting/handling-page-faults-on-snapshot-resume.md

This research handler is not production hardening. In particular, upstream's
balloon/remove-event caveats remain. The measured Rooms workload has no balloon.
The handler has a120-second lab deadline, and the paired Rooms command a90-second
wall limit. Their failures are recorded, never treated as completed workloads.
