# Initial live qualification — 2026-09-12

This is partial qualification of the cold-command toolstore seam, not the full
agent-dev-environments G1/G2 gates. Subsequent exact-head CI/reviewer results and
additional experiments belong on the PR.

Review follow-up: Codex identified that mount(8)'s default canonicalization could
resolve the proc-fd source back into a racy pathname. Attachment now uses
`--no-canonicalize` and verifies mounted device/inode identity before VM setup.
The privileged test was extended to perform the real bind and reject a different
inode. The CLI test also exposed that Clap's transitive requirements allowed
`--toolstore --task`; the conflict is now explicit.

The lifecycle receipt now records `toolstore_attached` before `vmm_started`,
which includes a successful InstanceStart. The live harness checks this order.

Later review fixes move toolstore binding to a blocking worker that owns the
cleanup guard until mount completes, and recheck the held inode seal after
hashing. Source publication now retains the whole frozen flake tree and records
per-file hashes, including imported files. The updated optimized runtime passed
three additional VM runs: scratch C/Rust compilation plus Go/Node execution
(21.537 s), no-scratch Python/OpenSSL (8.524 s), and retained exit-7 output
(8.520 s). Each ended with `cleanup_done`, preserved source hashes and emitted
attachment before VMM start. A first attempt after host reboot correctly refused
before boot because host TAP firewall setup was absent; the passing retry followed
the documented `setup-tap.sh --host` step.

## Environment and inputs

Existing Lima `rooms-host`: aarch64 Ubuntu 24.04, 6 CPUs, 4 GiB RAM, nested KVM,
Firecracker 1.15, guest Linux 6.1.155. Commands used an optimized Rooms binary,
stock rebuilt Alpine image, and dedicated short HOME `/tmp/rt0912`. No cloud
resources, model calls, project-env caches, or preinstalled project dependencies.

The host boot probe found `CONFIG_SQUASHFS=y`, `CONFIG_SQUASHFS_ZSTD=y` and
`CONFIG_OVERLAY_FS=y`. Nix 2.18.1 ran as the normal host user through its daemon.

| Input | SHA-256 |
| --- | --- |
| Alpine image | `b5100bf2dd28992c3736bdbbc71d5d597e03806e53ebe5b845e697de7d524aaa` |
| Polyglot squashfs | `5259f9bd8d6f18d4ea573740496dd220ad2f2dc58b3a8494ff222a6a45728447` |
| Python squashfs | `85074bedee9b2c34014488b422c0869c01571dbb121da5191b2e719863e760d3` |

Polyglot contains 111 store objects in 715,968,512 bytes. Two separate builder
invocations produced the same image hash and manifest. That proves packing
reproducibility on this host/tool version, not independent upstream rebuilds.

## Passed at initial publication

- `make check`: formatting, strict all-target/all-feature clippy, 534 portable
  tests. Linux `cargo test`: 564 tests. The additional root-only inode replacement
  test passed when explicitly invoked; it is ignored by ordinary CI.
- Single fresh scratch room: C, Go and Rust compiled the sum-of-squares program;
  those binaries, Python and Node all produced `333383335000`. Guest default
  routes were removed first and `ip route get 1.1.1.1` failed. Root could not
  create `/nix/ROOMS_WRITE_MUST_FAIL`; the mount was squashfs read-only.
- Versions actually executed: GCC 15.3.0, Go 1.26.7, rustc 1.98.1, cargo 1.98.0,
  Python 3.14.7 and Node 24.20.0.
- Separate Python preset without scratch: SQLite 3.53.3 and OpenSSL 3.6.4 loaded,
  `/dev/vdb` mounted at `/nix` read-only, expected output collected, cleanup
  completed. This unloaded-host run took 8.514 seconds end to end.
- Wrong architecture, hash, schema, mutable image and old boot image rejected
  before slot allocation. Manifest symlinks/directories/FIFOs rejected. A
  directory replacement after admission left the held source descriptor pointing
  to the original bytes.
- Existing output, missing lock and malformed flake refused; the failing builder
  left no published output, reservation or temporary staging directory.
- Review follow-up: linked `flake.nix`, `flake.lock` and another local input were
  rejected before Nix evaluation, with staging removed. The regular Python
  preset rebuilt successfully through that guard with its original image hash.
- Direct, nested and symlink-alias output paths inside the source flake were
  refused before creating directories; the source tree remained unchanged.
- A custom flake imported `toolchains.nix` and contained a nested regular file.
  All four source files were retained byte-for-byte with matching manifest
  hashes. After moving the original tree away, Nix rebuilt the same buildEnv
  from the retained `flake/`; the packed Python image hash was unchanged.
- A sealed 4 KiB image with matching hash/magic but invalid squashfs contents
  reached the intended guest mount failure and kernel panic before SSH. The
  host reported timeout/collection failure, released the VM and completed
  cleanup. This tests mount failure; magic/hash admission is not an fsck.
- Two smaller concurrent workloads shared the same toolstore, wrote private
  scratch files, executed Python hashing, rejected guest-root writes and cleaned
  up. Their workload intervals overlapped. A separate exit-7 workload retained
  its stdout and failed result; the backing image hash was unchanged.
- SIGTERM during a real Nix derivation containing a controlled 120-second sleep:
  the builder exited 143 in 0.022 seconds, both observed build descendants
  disappeared (PID/starttime checked), and no output, reservation or staging
  remained. Python 3.12's `subprocess.run` exception path kills and reaps its
  active child; no extra process-tracking layer was needed for this result.

## Failed or not established

Two concurrent full polyglot builds reached workload execution and shared the
same admitted disk, but both hit the 600-second wall cap while compiling Go's
standard library from empty caches. Both returned 124 and reached
`cleanup_done`. SSH artifact writes also timed out under load, although collection
subsequently completed. An earlier 300-second run likewise timed out. The full
`scripts/test-toolstores.py` suite therefore did **not** pass on this host.
Do not interpret successful attachment as concurrent build throughput or density
qualification. Repeat on a suitable real host before making those claims.

The actual Rooms repository at `5bf1d4cafbd2c58fc8c95cd0c895539d70097258`
cloned successfully, fetched its locked dependencies and compiled with Nix Rust,
but hit its 900-second cap before tests ran. It returned 124, collected logs and
recorded `cleanup_done` (909.483 seconds total). This is not a passing repository
test. Both large cold-build qualifications remain open for the real-host trial.

A 30-second no-scratch probe expired before readiness; a longer diagnostic
captured normal boot and SSH. Flat `--egress none` blocked host SSH replies and
was explicitly cancelled with cleanup; see `docs/follow-ups.md`. Removing a
guest default route tests offline compatibility, not adversarial network
isolation. The guest remains able to change its own routes via sudo.

Not established here: x86_64 execution, bare-metal performance, snapshot restore
with toolstores, project dependency caching, multi-tenant security, or cloud
deployment. The full repository Cargo workload is a separate qualification from
the small offline programs above.

## Retained evidence

Linux `/home/mh.guest/rooms/toolchain-lab/` contains `proof3`, `proof4`,
`python-final`, `extra-proof`, `builder-failures`, `fault-proof2`, kernel logs,
build logs and manifests. The operator's exported packet is
`~/Documents/Codex/2026-09-12/rooms-toolchain-lab/`. Each experiment retains
host lifecycle JSONL plus available result/log artifacts; failed runs remain
present. Local logs are supplemental evidence, not portable CI.

For the explicit privileged test, build `cargo test --lib --no-run`, then run
the emitted test binary under sudo with
`toolstore::tests::attachment_holds_admitted_inode_after_directory_replacement
--ignored --exact`. It creates and unseals only its own temporary test inode.
