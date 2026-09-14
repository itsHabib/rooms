# Usable command rooms

A development command needs a repository, enough CPU/RAM, writable space that does
not consume its RAM budget, and output that survives disposal. This is the first
cold-run slice of the agent-development-environments (#118) and local-hardening
(#119) designs.

## Contract

- `rooms run --cpus N --memory MiB --disk GiB` configures a cold VM. Existing CPU
  and RAM defaults remain 1/256; disk is opt-in and implies a read-only image.
- Each disk is a sparse ext4 file inside its room's jail. The guest uses it as the
  overlay upper/work backing; the existing room guard removes it during teardown.
  Capacity is a guest limit, not a reservation of host free space.
- `--repo URL --command CMD` clones into `/workspace/repo`, pins `--base-sha`
  (default HEAD), executes there, and exports a binary-capable `result.patch`
  including new files. URLs containing embedded credentials are rejected.
- `--out` retains logs and result metadata. SIGINT/SIGTERM and wall timeout keep
  existing partial logs. Collection failure makes a cold run fail after teardown.
  Completed and partial artifacts belong to the invoking sudo user rather than a guest UID.
- A scratch run requires a freshly rebuilt Alpine image. Admission rejects older
  overlay-init scripts. Repository runs also reject a missing overlay-init before claiming a slot. Missing/unmountable scratch fails boot without RAM fallback.

## Deliberate cuts

No new service, daemon, filesystem, scheduler, profile format, or dependency cache.
No snapshot-device changes: scratch is for disposable cold runs; `--disk --keep`
is rejected. Existing sealed-base/restore paths retain their current machine shape.
No local working-tree import, environment/file staging, live log streaming, Claude
runner, or Fleet adapter in this slice. The command uses toolchains already in the
image; image extensions remain the existing way to install them.

Patch/changeset contents are guest-produced, not trusted evidence. Abrupt VM loss
can still prevent collection. SIGKILL cannot run cleanup. SIGTERM handling here
covers cold command runs (including boot), not the legacy keep/idle modes.
Blocking jail staging finishes before cancellation can safely unwind its mounts.
A termination signal received during finalization is acknowledged after cleanup;
the collected result continues to describe the guest command that already finished.
Patch-export failure preserves result.json and the command exit code there, while
the CLI reports the export failure. Cancellation preserves
logs but does not promise a complete repository patch. The existing 15-second
collection grace and in-memory tar transport bound practical output size.

## Acceptance

`make check` must pass. On a configured Linux/KVM host, run
`scripts/test-command-rooms.py` with this branch's binary, a rebuilt image,
`HOME` containing `.ssh/id_rooms`, and a fresh output directory. It checks CPU/RAM
and ext4 backing, writes beyond the equivalent tmpfs capacity, runs repository
shell validation, exports edits on success/failure, proves a fresh second room,
preserves logs on timeout/SIGTERM, and checks lifecycle cleanup and output ownership.

The useful next decision comes from this path: measure repeated real repository
commands before adding prepared dependency storage or connecting a Fleet worker.
