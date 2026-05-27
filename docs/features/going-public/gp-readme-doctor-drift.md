**Status**: draft
**Owner**: @michael (human:mh)
**Date**: 2026-05-27
**Related**: dossier task `gp-readme-doctor-drift` (id: `tsk_01KSMXETRJRKACM7Z96E4RH8VV`), [going-public driver](driver.md)

# docs: README CLI table marks `doctor` as stub but #8 shipped real checks

## Scope

| Bucket | Files | Est. LOC | Weighted |
|---|---|---|---|
| Docs | `README.md` (modify — one table row) | ~2 | 0 |
| **Total weighted** | | | **~0** |

Band: **amazing**.

## Goal

Fix doc/code drift — README's CLI surface table claims `doctor` is a stub, but #8 shipped 7 real checks + `--json`. Public consumers need an accurate read.

## Fix

In `README.md`'s "CLI surface" table, replace the `doctor` row:

Before:
```
| `doctor` | Check `/dev/kvm`, Firecracker binary, image paths, TAP. | stub |
```

After:
```
| `doctor` | Real checks: `/dev/kvm`, Firecracker version, kernel + rootfs validation, TAP setup, nested virt, ANTHROPIC_API_KEY. `--json` for machine-readable output. | shipped |
```

Sweep the other rows for similar drift while you're there:
- `run` stays `partial` (M4 ships `--command`; `--repo --task` is not yet shipped).
- `create` / `exec` / `collect` / `destroy` stay `planned`.

## Acceptance

- No row claims `stub` where the verb is actually shipped.
- No row claims `planned` where the verb is actually shipped.
- Table accurately reflects `cargo run -- <verb> --help` output for each row.

## Non-goals

- Rewriting the broader "Status" section — accurate as of m4 + batch 1.
- Adding new verbs to the table.

## Conflict note

Touches `README.md` (same file as `gp-readme-badges`). They modify DIFFERENT regions. Safe to parallel-run with rebase.
