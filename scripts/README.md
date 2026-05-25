# scripts/

Host-side helpers for the `rooms` Firecracker substrate.

## Rootfs builder

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
| `setup-tap.sh` / `teardown-tap.sh` | TAP + NAT for guest networking |
| `bake-rootfs-ssh.sh` | POC helper for the quickstart bionic image (superseded by `build-rootfs.sh` for new images) |
| `provision-hyperv.ps1` | Create the Hyper-V VM from Windows |
