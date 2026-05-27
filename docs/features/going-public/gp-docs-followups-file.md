**Status**: draft
**Owner**: @michael (human:mh)
**Date**: 2026-05-27
**Related**: dossier task `gp-docs-followups-file` (id: `tsk_01KSMXF01G4WN7ATTE9QKVW1DZ`), [going-public driver](driver.md)

# docs: create docs/follow-ups.md as in-repo status sink

## Scope

| Bucket | Files | Est. LOC | Weighted |
|---|---|---|---|
| Docs | `docs/follow-ups.md` (new), `CLAUDE.md` (small edit) | ~20 | 0 |
| **Total weighted** | | | **~0** |

Band: **amazing**.

## Goal

Add the canonical in-repo sink for out-of-scope discoveries, per portfolio convention (memory: `feedback_status_doc_over_issues`).

## Fix

1. Create `docs/follow-ups.md`:

```markdown
# Follow-ups

Out-of-scope discoveries from in-progress work. Each entry: date,
one-line title, link to the originating PR / commit / spec where it was
discovered. Per-issue depth in the linked PR; this file is just the index.

## Open

(none yet)

## Closed

(none yet)
```

2. Update `CLAUDE.md`'s `## When you're stuck` section to add a line pointing at `docs/follow-ups.md` as the canonical place for deferred items.

## Acceptance

- `docs/follow-ups.md` exists with the standard shape.
- `CLAUDE.md` references it.

## Non-goals

- Migrating dossier task content into the follow-ups doc — different layers (dossier = portfolio memory; follow-ups = repo-local index).
- Automating PR-comment → follow-ups injection.
