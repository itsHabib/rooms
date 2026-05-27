**Status**: draft
**Owner**: @michael (human:mh)
**Date**: 2026-05-27
**Related**: dossier task `gp-no-git-tags` (id: `tsk_01KSMXD1T4BXZD8B745PMGE5T7`), [going-public driver](driver.md)

# P1: tag v0.1.0 + GitHub release (must be LAST in the going-public sweep)

## Scope

| Bucket | Files | Est. LOC | Weighted |
|---|---|---|---|
| Repo ops | (no source files; `git tag` + `gh release create`) | 0 | 0 |
| **Total weighted** | | | **~0** |

Band: **amazing**.

## Goal

Anchor the first public-facing release of rooms at v0.1.0. After this lands, consumers can pin to a stable reference.

## Fix

After every other `gp-*` task in the going-public phase has landed on main:

```sh
git tag -a v0.1.0 -m "v0.1.0: POC + batch 1 productionization shipped"
git push origin v0.1.0
gh release create v0.1.0 \
  --title "v0.1.0 — POC + batch 1" \
  --notes-file CHANGELOG.md
```

(`--notes-file CHANGELOG.md` assumes `gp-no-changelog` has landed first and the v0.1.0 section is populated. If not, inline `--notes "First public-facing release. Substrate verb: rooms run --command. POC end-to-end via examples/drive-anthropic.sh."`)

The tag must match `Cargo.toml version = "0.1.0"` (already set).

## Acceptance

- `git tag --list` shows `v0.1.0`.
- `gh release view v0.1.0 --repo itsHabib/rooms` returns the release with `tag_name: v0.1.0`.
- The release notes reference the major shipped pieces (POC substrate, M4, batch 1 productionization).

## Test plan

- After tagging: visit `https://github.com/itsHabib/rooms/releases/tag/v0.1.0` and confirm the page renders.
- `git checkout v0.1.0` resolves to the expected commit (current `main` HEAD at task-claim time, modulo any in-flight `gp-*` work — the agent claiming this task is responsible for confirming all sibling gp-* have merged first).

## Non-goals

- Publishing to crates.io — separate gate (v0 is a binary substrate).
- release-please / release-drafter automation — manual is fine for v0.1.0.

## Ordering

This task MUST be the last `gp-*` to ship. The driver manifest's batch 4 (`depends_on: [3]`) enforces this; the agent claiming this task should verify all sibling tasks are in `done` status before tagging.
