**Status**: draft
**Owner**: @michael (human:mh)
**Date**: 2026-05-27
**Related**: dossier task `gp-personal-path-in-spec-doc` (id: `tsk_01KSMXCW5RFB7NXXZY8KWNMDQ2`), [going-public driver](driver.md)

# P0: scrub operator-personal Windows path from spec doc

## Scope

| Bucket | Files | Est. LOC | Weighted |
|---|---|---|---|
| Docs | `docs/features/poc-m2-1-hardening-followups/spec.md` | ~2 | 0 |
| **Total weighted** | | | **~0** |

Band: **amazing** (under 500 weighted).

## Goal

Remove the operator's Windows user directory + portfolio layout from a checked-in spec doc so public consumers don't see machine-specific paths.

## Fix

`docs/features/poc-m2-1-hardening-followups/spec.md:241` currently contains:

```
**Workdir for `ship.ship`:** `C:\Users\MichaelHabib\pers\rooms\.claude\worktrees\m2-1-hardening`.
```

Replace with a portable placeholder:

```
**Workdir for `ship.ship`:** `<repo-root>/.claude/worktrees/m2-1-hardening/`.
```

`<repo-root>` is the convention used in other rooms specs.

## Acceptance

`git grep -niE 'C:\\\\Users\\\\MichaelHabib|/Users/[A-Za-z][A-Za-z0-9._-]+/|/home/[A-Za-z][A-Za-z0-9._-]+/' -- docs/` returns no operator-personal hits (with the legitimate `/home/rooms/` guest-user path in `scripts/build-rootfs.sh` already excluded by being outside `docs/`).

## Non-goals

- Sweeping every other spec doc for similar drift — surface other hits as separate tasks if/when they show up. This task is scoped to the one confirmed hit.
- Migrating spec docs into the dossier — that's a portfolio-wide decision, not a public-readiness gate.
