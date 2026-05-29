# Follow-ups

Out-of-scope discoveries from in-progress work. Each entry: date,
one-line title, link to the originating PR / commit / spec where it was
discovered. Per-issue depth in the linked PR; this file is just the index.

## Open

Surfaced 2026-05-29 by the first end-to-end dogfood (a `claude -p` session run inside a rooms microVM that produced [#34](https://github.com/itsHabib/rooms/pull/34)). These are the seams `cursor-sdk-runner` needs to reconcile:

- 2026-05-29 — agent rootfs (`build-rootfs.sh`) installs node + claude-code but **not Rust**, so in-room agents can't run `cargo fmt/clippy/test`. Bake a toolchain into an agent image, or scope in-room tasks to not need it. ([#34](https://github.com/itsHabib/rooms/pull/34))
- 2026-05-29 — `build-rootfs.sh` bakes a static `/etc/resolv.conf`, but noble's **systemd-resolved** replaces it with a 127.0.0.53 stub that has no upstream → guest DNS fails (NAT/routing is fine). Mask/configure resolved, or make the static resolv.conf survive boot. ([#34](https://github.com/itsHabib/rooms/pull/34))
- 2026-05-29 — `runner.rs` SSHes to the guest as **`root@`**, but `build-rootfs.sh` creates a **`rooms`** user and sets `PermitRootLogin no` — so `rooms run --command` can't drive a `build-rootfs.sh` image. The runner's guest user must be configurable / aligned with the rootfs. ([#34](https://github.com/itsHabib/rooms/pull/34))
- 2026-05-29 — `claude -p` reads stdin; when driven over an SSH heredoc it consumes following script lines. The runner must invoke it with stdin redirected (`</dev/null`). ([#34](https://github.com/itsHabib/rooms/pull/34))

## Closed

(none yet)
