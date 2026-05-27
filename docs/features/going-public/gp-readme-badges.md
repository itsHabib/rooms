**Status**: draft
**Owner**: @michael (human:mh)
**Date**: 2026-05-27
**Related**: dossier task `gp-readme-badges` (id: `tsk_01KSMXEKQ0DSTVR13Q0PEB2NDQ`), [going-public driver](driver.md)

# docs: add CI + MIT license badges to top of README

## Scope

| Bucket | Files | Est. LOC | Weighted |
|---|---|---|---|
| Docs | `README.md` (modify — insert two badge lines after the title) | ~3 | 0 |
| **Total weighted** | | | **~0** |

Band: **amazing**.

## Goal

First-impression signal — visitors see live CI status + license at a glance, before the elevator pitch.

## Fix

Insert two badges right after the `# rooms` title in `README.md`, before the elevator pitch paragraph:

```markdown
# rooms

[![CI](https://github.com/itsHabib/rooms/actions/workflows/ci.yml/badge.svg)](https://github.com/itsHabib/rooms/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

Disposable Firecracker microVMs...
```

## Acceptance

- README opens with two badges in a horizontal row.
- CI badge links to the Actions tab; license badge links to LICENSE.
- GitHub renders both correctly.

## Non-goals

- Crates.io badge — rooms isn't published; revisit after first crates.io release.
- docs.rs badge — same reason.
- Coverage badge — depends on `gp-ci-coverage-workflow` landing first.

## Conflict note

Touches `README.md` (same file as `gp-readme-doctor-drift`). They modify DIFFERENT regions (this inserts badges at the top; doctor-drift edits the CLI surface table further down). Safe to parallel-run; second-to-merge needs a small rebase.
