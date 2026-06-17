# scripts/

Host-side helpers for the `rooms` Firecracker substrate.

## Agent rootfs (Alpine) — current

Build the agent guest image on Alpine (musl/busybox/openrc) with the claude-code native musl binary, paired with a Firecracker-tuned virtio-rng kernel. Boots to sshd in ~2 s; ~276 MB. The script is the source of truth; built images are **not** committed (see `images/.gitignore`).

### Build

```sh
sudo ./scripts/build-rootfs-alpine.sh \
  --out images/agent-alpine.ext4 \
  --ssh-key ~/.ssh/id_rooms.pub
```

Pinned by default to Alpine `3.21.7` and `claude-code=2.1.148-r1` (override with `--alpine-version` / `--claude-version`). The Alpine minirootfs sha256 is hardcoded (see `scripts/checksums.txt`), not taken from the CDN sidecar alone. `claude-code` installs from Anthropic's official signed apk repo; the build verifies the signing-key sha256 and **aborts** if `claude --version` doesn't link cleanly against musl. The agent runs as the unprivileged `rooms` user (uid 1000) — claude-code refuses `--dangerously-skip-permissions` as root. There is **no Node** in the base image; `cursor-sdk-runner` adds it via the `--extend` hook (sibling files in `scripts/rootfs/`, e.g. `package-lock.json`, are staged into the chroot automatically).

### Kernel

`scripts/setup-rooms-host.sh` downloads the Firecracker CI kernel `vmlinux-6.1.155` (uncompressed ELF, `CONFIG_HW_RANDOM_VIRTIO=y`) to `~/rooms/images/vmlinux.bin` — the sibling `rooms` boots next to the rootfs. virtio-rng means the guest CRNG seeds from the `/entropy` device with no host-side workaround.

### Smoke test

```sh
./scripts/test-rootfs-alpine.sh images/agent-alpine.ext4
```

Boots the image under Firecracker and asserts: key-only `rooms@` SSH, DNS resolves (`getent hosts github.com`), `/dev/hwrng` present, `claude`/`git` work, claude links against musl, TLS to the Anthropic API verifies, password auth refused, and size < 300 MB. Uses the private key matching `--ssh-key` (default `~/.ssh/id_rooms`).

## Rootfs builder (Ubuntu/noble — legacy)

Build the v0 Ubuntu rootfs from scratch (debootstrap). The script is the source of truth; built images are **not** committed (see `images/.gitignore`).

### Prereqs

Run on an Ubuntu host (rooms-host VM or bare metal) as **root**:

```sh
sudo apt install debootstrap e2fsprogs util-linux curl ca-certificates gnupg
```

You also need an SSH keypair. The builder bakes your **public** key into the `rooms` user (uid 1000); there is no default password.

### Build

```sh
sudo ./scripts/build-rootfs.sh \
  --suite noble \
  --size 4G \
  --out images/node-dev.ext4 \
  --ssh-key ~/.ssh/id_rooms.pub
```

Optional flags:

- `--extend <script>` — bash script copied into the chroot and executed after baseline installs (used by task #4 cursor-sdk-runner).
- `--node-source <url>` — override the NodeSource setup script (default: `setup_20.x`, pinned 2026-05).

Output lands at `--out` (default `images/node-dev.ext4`). The script prints a **sha256** digest at the end — record it when comparing builds.

### Verify

```sh
sha256sum images/node-dev.ext4
```

Re-running the builder with the same flags should produce a functionally equivalent image. Byte-identical sha256 is **not** guaranteed: residual non-determinism includes file timestamps in some configs and `/etc/machine-id` if the guest regenerates it on first boot.

For a stronger check, loop-mount two builds and compare installed packages:

```sh
sudo chroot /mnt/a apt list --installed | sort > a.txt
sudo chroot /mnt/b apt list --installed | sort > b.txt
diff -u a.txt b.txt
```

### Smoke test

Requires:

- Firecracker on PATH and `vmlinux.bin` next to the rootfs (`scripts/setup-rooms-host.sh`).
- `tap-fc0` configured (`scripts/setup-tap.sh`).
- Host commands the test invokes: `jq`, `ip`, `ssh`, `curl` — all on PATH. `setup-rooms-host.sh` installs them.

```sh
./scripts/test-rootfs.sh images/node-dev.ext4
```

Uses `rooms@172.16.0.2` with the private key matching `--ssh-key` (default `~/.ssh/id_rooms`).

### Fallback image

If you only need to boot once before building locally, `scripts/setup-rooms-host.sh` downloads the Firecracker quickstart bionic rootfs to `~/rooms/images/rootfs.ext4`. Replace it with `node-dev.ext4` once the builder has run.

## Other scripts

| Script | Purpose |
| --- | --- |
| `setup-rooms-host.sh` | Bootstrap the rooms-host VM (Firecracker, kernel, Rust, Node) |
| `checksums.txt` | Central sha256 pins for every build input; bump here when upstream versions change |
| `setup-tap.sh` / `teardown-tap.sh` | TAP + NAT for guest networking |
| `test-tap-rules.sh` | Host-only iptables rule assertions (run before merge on rooms-host) |

### TAP / iptables hardening

`setup-tap.sh` gives the guest internet egress only:

- NAT is source-restricted to `172.16.0.0/24` (rooms guest subnet).
- FORWARD drops guest traffic to RFC1918 (`192.168.0.0/16`, `10.0.0.0/8`, and other `172.16.0.0/12` destinations) before the egress accept.
- IPv4 forwarding is scoped to `tap-fc0` via `net.ipv4.conf.tap-fc0.forwarding=1`; the prior global `net.ipv4.ip_forward` value is saved under `/run/rooms/` for `teardown-tap.sh` to restore.

Verify on rooms-host:

```sh
sudo ./scripts/test-tap-rules.sh
```

From inside a booted guest, confirm `curl https://api.anthropic.com` succeeds while ping/connect to an RFC1918 host on the operator LAN is blocked.
| `bake-rootfs-ssh.sh` | POC helper for the quickstart bionic image (superseded by `build-rootfs.sh` for new images) |
| `provision-hyperv.ps1` | Create the Hyper-V VM from Windows |
