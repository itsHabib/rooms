**Status**: draft
**Owner**: @michael (human:mh)
**Date**: 2026-06-20
**Related**: [readonly-rootfs-with-overlay/spec.md](../readonly-rootfs-with-overlay/spec.md) (the overlay this reads), `src/runner.rs`, `src/artifacts.rs`, `docs/runner-contract.md`

# `rooms diff` — surface the agent's exact change set

## Goal

The read-only rootfs + tmpfs overlay ([readonly-rootfs-with-overlay](../readonly-rootfs-with-overlay/spec.md), #45) gives us something for free that nothing currently uses: the overlay **upperdir is, literally, the exact set of files the agent created / modified / deleted this run** — anywhere on the filesystem, including binaries, dotfiles, and changes outside git. Today that change set evaporates on teardown, unread.

Make it a first-class artifact. Enumerate the upperdir before teardown, land a `changeset.json` in the `--out` directory, and expose it via `rooms diff`. Two payoffs:

1. **Forensics** — "what did this agent actually touch?", answerable after the room is gone, without trusting the agent's own `git diff`.
2. **A lane tripwire** — the agent works in `/workspace`; anything it wrote *outside* `/workspace` (touched `/etc`, dropped a file in `/root`, modified a system binary) is a cheap, free "did it escape its lane" signal.

## Background (the overlay layout, verified on host)

`scripts/lib/overlay-init.sh` mounts a tmpfs overlay (`lowerdir=/`, `upperdir=/mnt/upper`, `workdir=/mnt/work`) then `pivot_root . oldroot`. In the **running guest** (new root = the overlay merged view):

- the upperdir is at **`/oldroot/mnt/upper`** (the old root, carrying the tmpfs, is parked at `/oldroot`),
- the lowerdir (the pristine RO root) is at **`/oldroot`**,
- overlayfs represents a **delete as a whiteout**: a character device `0:0` at the file's path in the upper,
- an **opaque dir** (delete-then-recreate) carries `trusted.overlay.opaque=y`.

(Confirmed live during #45 host-e2e: `sudo cat /oldroot/mnt/upper/<f>` returned the agent's write.)

Classification, per entry under `/oldroot/mnt/upper` (relative path `R`):

| upper entry | meaning |
| --- | --- |
| char device `0:0` | **deleted** (`R` removed) |
| regular file or symlink, `/oldroot/R` exists in lower | **modified** |
| regular file or symlink, `/oldroot/R` absent in lower | **added** |
| directory | structural (carried for path context; opaque dirs noted) |

Symlinks are walked (`find … -type l`) and classified by lower presence like
regular files — a symlink written outside `/workspace` (e.g. `ln -s t /etc/foo`)
is itself a persistent-path lane escape, so a `-type f`-only walk would miss it.

## Design

### `--readonly-rootfs` flag (the enabler — `src/main.rs`)
The overlay only exists when `readonly_rootfs` is true, which #45 gated to the cursor path (which needs a key). That makes the change set both **untestable without a key** and **unusable for a dev `--command` run**. Add an explicit `rooms run --readonly-rootfs` boolean (default off) and widen the gate to `args.readonly_rootfs || matches!(args.runner, Cursor)`. Now any `--command` run against an overlay-init image (the cursor image, or any built by `build-rootfs-alpine.sh`) can boot read-only + overlay, which makes `rooms diff` demoable and host-testable end to end without a cursor agent. (Caveat: the flag requires an image carrying `/sbin/overlay-init`; against a plain image the kernel panics — same constraint the cursor path already lives under.)

### What the change set actually contains (and the sharp part)
The upperdir is **everything written since boot**, not a curated agent diff, with noise on two sides:
- *Inside `/workspace`*: on the cursor path the `git clone` lands here, so the workspace side is churny by design — `rooms diff` summarizes it as counts (`--json` for the full list).
- *Outside `/workspace`*: the **OS itself writes every boot** — `/run/lock`, `/run/sudo`, `/var/log/dmesg`, `/var/log/wtmp` showed up on a *clean* run during host-e2e. So a naive "outside /workspace" tripwire fires on every run.

The sharp signal is therefore **writes outside `/workspace` to *persistent* paths** (`/etc`, `/usr`, `/root`, ...) — the lane escape — with the OS's expected ephemeral churn (`is_lane_escape` filters `/run`, `/var/log`, `/var/cache`, `/tmp`, `/dev`, `/proc`, `/sys`) excluded. That's the write the agent's own `git diff` (`result.patch`) structurally cannot see. `rooms diff` **leads with the lane escapes** (per-op `A`/`M`/`D`), notes the filtered runtime-write count, and exits **3** when any escape fired so it composes as a gate.

### Trust boundary (what exit 0 means)
The enumeration runs **in-guest**, over SSH, as the same `rooms` user the agent runs as — which holds passwordless root. So the entity whose lane escapes we want to detect also controls the reporter: a compromised or adversarial guest can omit its own out-of-lane records, forge the `NOOVERLAY` sentinel, or hide the upperdir from the walk. `rooms diff` is therefore a **forensic signal against a non-adversarial guest** — it catches a buggy or careless agent that wrote outside `/workspace`, not one actively evading detection. The containment boundary is the microVM + jailer + read-only rootfs; `rooms diff` exit 0 is **not** a security guarantee that the guest stayed in its lane. (Host-side enumeration of the upperdir via the firecracker drive, rather than guest SSH, would close this — out of scope here.)

### Enumeration (mechanism — `src/runner.rs`)
A new `collect_changeset_to_host(guest_ip, key, host_dir)`, called from `main` right after `collect_out_to_host` (both best-effort; a failure logs + is non-fatal, never blocks the run result). It runs **one** SSH command over the channel the runner already holds — a small POSIX `find`/`stat` walk of `/oldroot/mnt/upper` (via the guest's NOPASSWD `sudo`, since upper subdirs may be root-owned) emitting a NUL-delimited `op\0relpath` record stream. The **host** parses that stream into a typed `Changeset` and serializes `changeset.json` with serde (no shell-side JSON). Guest stays dumb; the host owns the contract.

If `/oldroot/mnt/upper` does not exist (a `--command` run — no overlay), the walk reports "no overlay" and `changeset.json` records `overlay_active: false` so `rooms diff` can say *why* there's nothing, distinct from an empty-but-active change set.

### Changeset type (`src/artifacts.rs`)
```rust
pub struct Changeset {
    pub schema_version: u32,   // own version, mirrors SCHEMA_VERSION discipline
    pub overlay_active: bool,  // false on a writable-rootfs run
    pub added: Vec<String>,    // guest-absolute paths sans leading / (workspace/repo/x, etc/hosts)
    pub modified: Vec<String>,
    pub deleted: Vec<String>,
}
```
Plain data, no I/O, no upward import. The tripwire is *derived* not stored — `lane_escapes()` / `is_lane_escape()` partition the three lists at read time, so the on-disk artifact stays minimal. `changeset.json` is a **standalone** artifact in `out/` — NOT referenced from `result.json` (which the guest writes before the host enumerates), so no `result.json` schema bump and no contract coupling.

### CLI verb (`src/main.rs`)
`rooms diff --from <out-dir> [--json]`, mirroring `rooms collect --from`:
- human mode: a terse summary (`+N ~N -N`, the changed paths, and a loud line if `outside_workspace` is non-empty),
- `--json`: the `changeset.json` verbatim,
- `--command`/no-overlay: a clear "no changeset (overlay not active; only the cursor/`--runner` path uses a read-only rootfs)" rather than a silent empty.
- exit codes (so `rooms diff` composes as a gate): **0** verified, no lane escape · **3** a write escaped `/workspace` to a persistent path · **2** indeterminate — the lane couldn't be verified: the `--from` dir is missing/not a directory, holds no `changeset.json` (or one that can't be read/parsed, or carries a foreign `schema_version`), or records `overlay_active: false` (a writable-rootfs run has no lane to check). A gate must never read "couldn't verify" (collection failed, wrong path, corrupt artifact, no overlay) as "clean", so the indeterminate case is exit **2**, never 0.

### Layering
Enumeration = `runner` mechanism; `Changeset` = `artifacts` (plain data); verb = `main`. Dependencies flow down only (`main → runner → artifacts`). No new downward edge.

## Acceptance

- `changeset.json` is collected into `--out` on a cursor/`--runner` run; lists `added`/`modified`/`deleted` correctly, and `outside_workspace` flags any entry not under `workspace/`.
- Whiteouts (char `0:0`) classify as **deleted**; a fresh file as **added**; an edit to a lower file as **modified**.
- `rooms diff --from <out>` prints a human summary; `--json` prints the raw changeset.
- A `--command` run (no overlay) yields `overlay_active: false`; `rooms diff` says so clearly rather than printing an empty diff, and exits **2** (indeterminate — without an overlay the lane-escape question is unanswerable, so a gate never reads it as clean).
- A missing `--from` directory, an out-dir with no `changeset.json` (e.g. a best-effort collect failure), a corrupt/unreadable `changeset.json`, or a foreign `schema_version` all exit **2** (indeterminate) — `rooms diff` as a gate never reads "couldn't verify" as "clean".
- Enumeration failure is non-fatal: the run's `result.json`/exit code are unaffected (best-effort, logged).
- `make check` green (build + clippy `-D warnings` + unit tests, Windows + Linux).
- **Host e2e (rooms-host, gates merge):** an overlay (`--runner cursor`-style, `readonly_rootfs`) run that creates a file in `/workspace`, edits a baked file, deletes one, and touches a file outside `/workspace`; `rooms diff` shows the add/modify/delete and flags the out-of-lane write.

## Test plan
- Unit: the host-side parser (NUL stream → `Changeset`), incl. whiteout → delete, add-vs-modify (lower presence), and the `outside_workspace` partition; the human formatter; the no-overlay path. Pure functions, no SSH.
- E2e (host-only, the gate): the overlay run above; assert the four classifications + the tripwire on a real upperdir.

## Out of scope (later, if asked)
- Exporting the changeset **contents** as a patch/tarball/OCI layer (this is the manifest only).
- `rooms apply` (replaying a changeset onto another base) — duplicates `git apply` + the push path; no consumer asked.
- Diffing a `--command` (writable-rootfs) run — there is no overlay to read; intentionally cursor-path-only.
