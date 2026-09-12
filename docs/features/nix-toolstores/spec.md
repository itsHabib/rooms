# Nix toolchains for cold rooms

This implements the toolchain-disk seam from the agent-dev-environments design
(PR #118), on top of the usable-command slice (PR #120). Nix builds on the Linux
host. Guests keep the Alpine boot image and do not need Nix installed.

## Contract

On the Ubuntu Rooms host, run `bash scripts/setup-nix-host.sh` as the normal
user, then start a fresh login to pick up Nix group membership. It installs Nix,
Python and squashfs-tools using apt and starts the packaged Nix daemon. Nothing
is installed on the Mac or inside the guest.

`presets/flake.nix` and its lock declare Rust, Go, Node, Python, and a combined
`polyglot` buildEnv for aarch64-linux and x86_64-linux. These are toolchains, not
preinstalled project dependencies. Each carries its own Nix libc and loaders.

`python3 scripts/build-toolstore.py --preset rust --out /path/to/toolstore`
builds the pinned local flake as the invoking non-root user. It retains a Nix GC
root while enumerating and packing the complete runtime closure, preserving
absolute store symlinks. A squashfs mounts at `/nix`; the buildEnv is reached via
`/nix/var/rooms/env`. Fixed filesystem timestamps and ownership permit identical
packing of the same closure with the same squashfs tool version/options.

The output directory contains `toolstore.sqfs`, `meta.json`, and the exact flake
tree under `flake/`, including imported local files and its lock. The manifest
records architecture, closure, buildEnv, and hashes of every retained source file.
The builder refuses manifests above the same 1 MiB limit enforced at admission.
Only `chattr +i` runs through sudo. A sibling `.building` reservation excludes
cooperative concurrent builders for the same output. Publication never replaces
an existing output. Normal failure removes staging; after SIGKILL an abandoned
reservation/staging directory may need operator inspection and removal.

`rooms run --image I --toolstore DIR --command C [--repo URL] [--disk GiB]`
checks manifest version, host architecture, kernel-enforced immutability,
squashfs magic and the full disk hash before claiming a slot. It holds the
verified inode open, binds that descriptor into the jail, and attaches it as a
read-only virtio drive. The bind disables mount-helper path canonicalization and
checks the mounted device/inode against the held descriptor before VM setup.
The blocking mount worker owns the cleanup guard until it completes, including
when the awaiting async task is dropped.
This prevents path replacement between hashing and mount
from substituting unchecked bytes. The existing guard unmounts the shared inode
on success, error, cancellation and orphan cleanup; it never deletes the source.

The image must declare the exact `# rooms-toolstore-v1` overlay-init capability.
This is a compatibility declaration by a trusted image builder, not image
authentication or proof of arbitrary script behavior. Mount failure aborts boot
before SSH. Boot also resolves and checks the actual buildEnv `bin` directory
inside the new root, so a missing environment cannot fall back to Alpine tools.
The hook sets `/nix/var/rooms/env/bin` first in SSH session PATH while retaining
the caller's literal command in room metadata and receipts. The
host lifecycle records the verified SHA in `toolstore_attached`. A caller should
retain `--lifecycle` beside `--out` to preserve that host-authored provenance.
The toolchain disk is vdc with scratch and vdb without it; the existing vdb
scratch convention is preserved for this cold-run slice.

## Boundaries

No snapshot/restore attachment or cached project envs yet. `--toolstore` requires
a cold `--command`, preventing kept rooms or snapshot bases from acquiring an
unrecorded extra disk. No CLI toolstore catalog/GC, proxy, cloud provisioning or
Fleet changes. The builder requires a local locked flake and one of the named
outputs; it does not fetch arbitrary unpinned flake URLs. Local flake inputs must
be regular files/directories. Symlinks are rejected in the frozen copy before
Nix runs, so later edits to a link target cannot change the declared inputs after
evaluation. The output must be outside the source flake tree (including symlink
aliases); otherwise staging would recursively copy itself. Its cache is Nix's
existing build cache, and its output is an explicit
directory rather than a second content-addressed cache managed by Rooms.

The pinned declaration is portable; successful execution on another architecture
still requires an actual host test. A kernel needs squashfs with zstd support.
The current guest kernel was boot-probed for both; another kernel that lacks
support fails at mount rather than silently dropping the toolchain. The full
TDD G1/G2 gates are not claimed by this narrower cold-command proof.

## Validation

Run `make check` and `sudo env HOME=<dedicated-short-home> python3
scripts/test-toolstores.py --rooms <binary> --image <rebuilt-image> --toolstore
<directory> --out <fresh-directory>` on a configured Linux/KVM host. The harness
compiles and checks programs across C, Go, Rust, Python and Node with the guest
default route removed (while retaining host SSH), shares
one sealed disk between concurrent rooms, checks guest-root write rejection,
retains failed-command output, and rejects incompatible/mutable artifacts before
claiming a slot. It checks lifecycle teardown, output ownership, and unchanged
shared input hashes. Build the same preset to a second directory and compare the
two squashfs hashes to test packing reproducibility independently of Nix caching.
