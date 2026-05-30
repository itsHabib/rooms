# Follow-ups

Out-of-scope discoveries from in-progress work. Each entry: date,
one-line title, link to the originating PR / commit / spec where it was
discovered. Per-issue depth in the linked PR; this file is just the index.

## Open

Surfaced 2026-05-29 by the first end-to-end dogfood (a `claude -p` session run inside a rooms microVM that produced [#34](https://github.com/itsHabib/rooms/pull/34)). The SSH-user and `seed_entropy` seams are now resolved (see Closed); these remain:

- 2026-05-29 — agent rootfs (`build-rootfs.sh`) installs node + claude-code but **not Rust**, so in-room agents can't run `cargo fmt/clippy/test`. Bake a toolchain into an agent image, or scope in-room tasks to not need it. ([#34](https://github.com/itsHabib/rooms/pull/34))
- 2026-05-29 — `build-rootfs.sh` bakes a static `/etc/resolv.conf`, but noble's **systemd-resolved** replaces it with a 127.0.0.53 stub that has no upstream → guest DNS fails (NAT/routing is fine). Mask/configure resolved, or make the static resolv.conf survive boot. ([#34](https://github.com/itsHabib/rooms/pull/34)) — **Addressed** by the Alpine agent rootfs (`agent-rootfs-alpine-kernel`): Alpine has no systemd-resolved, so the baked static resolv.conf persists and `getent hosts github.com` resolves on first boot.
- 2026-05-29 — `claude -p` reads stdin; when driven over an SSH heredoc it consumes following script lines. The runner must invoke it with stdin redirected (`</dev/null`). ([#34](https://github.com/itsHabib/rooms/pull/34)) — partially mitigated: rooms passes a single remote command (no heredoc) and `run_wrapped` nulls the SSH stdin; the cursor path also appends `< /dev/null`. A `claude`-runner variant should keep the same discipline.
- 2026-05-30 — host-side artifact collection: `rooms run --runner cursor` writes `events.ndjson`/`summary.md`/`result.json`/`result.patch` into the guest's `/workspace/out`, but the non-`--keep` path tears the VM down without pulling them to the host, so `rooms collect --from` has nothing to read. Needs a scp/tar-back step (transport-out). ([`cursor-sdk-runner`](features/cursor-sdk-runner/spec.md))

## Closed

Resolved by `cursor-sdk-runner` (branch `prod-cursor-sdk-runner`; merge SHA on landing):

- 2026-05-29 — `runner.rs` SSHed to the guest as **`root@`**, but the agent rootfs sets `PermitRootLogin no` and runs the agent as the non-root `rooms` user (claude-code refuses `--dangerously-skip-permissions` as root), so `rooms run --command`/`--runner` couldn't drive the Alpine image. **Fixed:** the runner SSHes as **`rooms@`** (a `GUEST_USER` const) and forwards `CURSOR_API_KEY` alongside `ANTHROPIC_API_KEY`. ([#34](https://github.com/itsHabib/rooms/pull/34))
- 2026-05-29 — `runner.rs::seed_entropy` shelled a **python** ioctl over SSH to seed the guest CRNG — a bionic-kernel workaround, now obsolete under the FC CI 6.1.155 virtio-rng kernel (`/dev/hwrng` present) and broken on python-less Alpine. **Fixed:** `seed_entropy` removed (along with the now-dead `RunnerError::EntropySeed`); the `/entropy` device firecracker attaches seeds the CRNG natively. The `/entropy` attach in `firecracker.rs` stays. (`agent-rootfs-alpine-kernel`)
