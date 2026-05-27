**Status**: draft
**Owner**: @michael (human:mh)
**Date**: 2026-05-27
**Related**: dossier task `gp-no-changelog` (id: `tsk_01KSMXDG38XGDTW31M9WC76HRD`), [going-public driver](driver.md)

# P2: add CHANGELOG.md (keepachangelog format)

## Scope

| Bucket | Files | Est. LOC | Weighted |
|---|---|---|---|
| Docs | `CHANGELOG.md` (new) | ~30 | 0 |
| **Total weighted** | | | **~0** |

Band: **amazing**.

## Goal

Give consumers a single place to read "what changed between releases" without spelunking `git log`.

## Fix

Create `CHANGELOG.md` at the repo root following keepachangelog.com format:

```markdown
# Changelog

All notable changes to rooms are documented here. Format adapted from [keepachangelog.com](https://keepachangelog.com).

## [Unreleased]

## [0.1.0] — TBD

### Added
- POC substrate: `rooms run --image <ext4> --command <cmd>`, with TAP networking, SSH-to-guest, exit-code propagation.
- M4: outbound HTTPS from guest (Anthropic curl example at `examples/drive-anthropic.sh`).
- Batch 1 productionization: structured `FirecrackerError`, `RoomGuard` cleanup, real `doctor` with `--json`, debootstrap rootfs builder, runner-contract artifact layout.

### Changed
- n/a (first release)

### Fixed
- n/a (first release)
```

Anchor the v0.1.0 date when the tag lands (see `gp-no-git-tags`).

## Acceptance

- `CHANGELOG.md` exists at the repo root.
- v0.1.0 section enumerates the major shipped features.
- File parses as valid markdown.

## Non-goals

- Backfilling per-commit changelog entries before v0.1.0 — pre-launch changes roll up into the v0.1.0 line.
- Automating CHANGELOG generation from commit messages — operator's call later (release-please etc.).
