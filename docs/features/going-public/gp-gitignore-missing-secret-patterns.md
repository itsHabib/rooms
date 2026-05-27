**Status**: draft
**Owner**: @michael (human:mh)
**Date**: 2026-05-27
**Related**: dossier task `gp-gitignore-missing-secret-patterns` (id: `tsk_01KSMXD86ZG2P32WJFD98F9KY4`), [going-public driver](driver.md)

# P2: add universal env/secret patterns to .gitignore

## Scope

| Bucket | Files | Est. LOC | Weighted |
|---|---|---|---|
| Config | `.gitignore` | ~8 | 0 |
| **Total weighted** | | | **~0** |

Band: **amazing**.

## Goal

Set the floor against accidental secret commits before rooms goes public.

## Fix

Append to `.gitignore`:

```
# Secrets — never commit
.env
.env.*
*.pem
*.key
credentials*
secrets/
```

Existing `.gitignore` already covers `target/`, IDE noise, and rootfs build artifacts (`/images/*.ext4`, `*.tmp`, `*.bin`). These additions are universal-secret prevention, not stack-specific.

## Acceptance

- `.gitignore` contains each of the six entries above.
- `git check-ignore .env` (after creating an empty `.env` file at repo root) reports it as ignored.

## Non-goals

- Scanning history for already-committed secrets — `gitleaks` ran clean over 82 commits on 2026-05-27. No remediation needed.
- Stack-specific entries already present (`target/`).
