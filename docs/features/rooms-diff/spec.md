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
| regular file, `/oldroot/R` exists in lower | **modified** |
| regular file, `/oldroot/R` absent in lower | **added** |
| directory | structural (carried for path context; opaque dirs noted) |

## Design

### `--readonly-rootfs` flag (the enabler — `src/main.rs`)
The overlay only exists when `readonly_rootfs` is true, which #45 gated to the cursor path (which needs a key). That makes the change set both **untestable without a key** and **unusable for a dev `--command` run**. Add an explicit `rooms run --readonly-rootfs` boolean (default off) and widen the gate to `args.readonly_rootfs || matches!(args.runner, Cursor)`. Now any `--command` run against an overlay-init image (the cursor image, or any built by `build-rootfs-alpine.sh`) can boot read-only + overlay, which makes `rooms diff` demoable and host-testable end to end without a cursor agent. (Caveat: the flag requires an image carrying `/sbin/overlay-init`; against a plain image the kernel panics — same constraint the cursor path already lives under.)

### What the change set actually contains (and the sharp part)
The upperdir is **everything written since boot**, not a curated agent diff: on the cursor path that includes the `git clone` into `/workspace`, so the change set is *noisy inside `/workspace`*. The unique, high-signal part is the partition: **writes outside `/workspace`** — files the agent (or its `sudo`) touched in `/etc`, `/root`, system binaries — which the agent's own `git diff` (`result.patch`) structurally cannot see. So `rooms diff` **leads with the out-of-lane tripwire** and summarizes the `/workspace` churn (counts + `--json` for the full list), rather than pretending the raw upperdir is a clean diff.

### Enumeration (mechanism — `src/runner.rs`)
A new `collect_changeset_to_host(guest_ip, key, host_dir)`, called from `main` right after `collect_out_to_host` (both best-effort; a failure logs + is non-fatal, never blocks the run result). It runs **one** SSH command over the channel the runner already holds — a small POSIX `find`/`stat` walk of `/oldroot/mnt/upper` (via the guest's NOPASSWD `sudo`, since upper subdirs may be root-owned) emitting a NUL-delimited `op\0relpath` record stream. The **host** parses that stream into a typed `Changeset` and serializes `changeset.json` with serde (no shell-side JSON). Guest stays dumb; the host owns the contract.

If `/oldroot/mnt/upper` does not exist (a `--command` run — no overlay), the walk reports "no overlay" and `changeset.json` records `overlay_active: false` so `rooms diff` can say *why* there's nothing, distinct from an empty-but-active change set.

### Changeset type (`src/artifacts.rs`)
```rust
pub struct Changeset {
    pub schema_version: u32,        // own version, mirrors SCHEMA_VERSION discipline
    pub overlay_active: bool,       // false on a --command run
    pub added: Vec<String>,         // workspace-relative-agnostic; absolute guest paths sans leading /
    pub modified: Vec<String>,
    pub deleted: Vec<String>,
    pub outside_workspace: Vec<String>,  // the tripwire: entries not under workspace/
}
```
Plain data, no I/O, no upward import. `changeset.json` is a **standalone** artifact in `out/` — it is NOT referenced from `result.json` (which the guest writes before the host enumerates), so no `result.json` schema bump and no contract coupling.

### CLI verb (`src/main.rs`)
`rooms diff --from <out-dir> [--json]`, mirroring `rooms collect --from`:
- human mode: a terse summary (`+N ~N -N`, the changed paths, and a loud line if `outside_workspace` is non-empty),
- `--json`: the `changeset.json` verbatim,
- `--command`/no-overlay: a clear "no changeset (overlay not active; only the cursor/`--runner` path uses a read-only rootfs)" rather than a silent empty.

### Layering
Enumeration = `runner` mechanism; `Changeset` = `artifacts` (plain data); verb = `main`. Dependencies flow down only (`main → runner → artifacts`). No new downward edge.

## Acceptance

- `changeset.json` is collected into `--out` on a cursor/`--runner` run; lists `added`/`modified`/`deleted` correctly, and `outside_workspace` flags any entry not under `workspace/`.
- Whiteouts (char `0:0`) classify as **deleted**; a fresh file as **added**; an edit to a lower file as **modified**.
- `rooms diff --from <out>` prints a human summary; `--json` prints the raw changeset.
- A `--command` run (no overlay) yields `overlay_active: false`; `rooms diff` says so clearly rather than printing an empty diff.
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
