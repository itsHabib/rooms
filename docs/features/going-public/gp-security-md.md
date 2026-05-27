**Status**: draft
**Owner**: @michael (human:mh)
**Date**: 2026-05-27
**Related**: dossier task `gp-security-md` (id: `tsk_01KSMXFT5SE6K5HT57YGR0E4EX`), [going-public driver](driver.md)

# P3: add SECURITY.md with vuln-reporting channel

## Scope

| Bucket | Files | Est. LOC | Weighted |
|---|---|---|---|
| Docs | `SECURITY.md` (new) | ~12 | 0 |
| **Total weighted** | | | **~0** |

Band: **amazing**.

## Goal

Document a clear vuln-reporting channel for a project that boots microVMs and calls external APIs.

## Fix

Create `SECURITY.md`:

```markdown
# Security Policy

## Reporting a vulnerability

Email <reporting-address> with details. Please do NOT open a public GitHub issue for security-sensitive findings.

We'll acknowledge within 72 hours and aim to land a fix within 14 days for high-severity issues.

## Supported versions

Only `main` and the most-recent tagged release receive security fixes.
```

Operator picks the reporting address. GitHub Security Advisories + an email both work.

## Acceptance

- `SECURITY.md` exists at the repo root.
- Contact line filled with a real, monitored address.
- GitHub Community Standards tab detects it.

## Non-goals

- Formal vuln-disclosure pipeline (HackerOne / GitHub Security Advisories machinery).
- Threat-modeling the substrate.
