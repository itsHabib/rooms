**Status**: draft
**Owner**: @michael (human:mh)
**Date**: 2026-06-15
**Related**: dossier task `harden-tap-rules` (id: `tsk_01KSDN8TWMW75N47EJ31XGFF6S`), [docs/follow-ups.md](../../follow-ups.md), retroactive security review 2026-05-24 (findings #6 + #7)

# Harden TAP / iptables rules — design spec

## Scope

| Bucket | Files | Est. LOC | Weighted |
|---|---|---|---|
| Production source (1×) | `scripts/setup-tap.sh`, `scripts/teardown-tap.sh` | ~70 | 70 |
| Tests (0.5×) | host-gated egress/LAN-block smoke (documented manual step or `e2e` script) | ~25 | 12 |
| Docs (0×) | `scripts/README.md` note on the new rules | ~10 | 0 |

Band: **amazing** (~82 weighted). Single PR.

## Goal

The POC's `scripts/setup-tap.sh` is too permissive in three independent ways (retroactive security review, 2026-05-24). On the single-interface Hyper-V VM the gaps are mostly cosmetic, but the moment a second interface appears (bridge, VPN, docker0) — or on the workbench-cloud trajectory where the host isn't the operator's lap — a compromised guest can reach the operator's LAN, and unrelated traffic gets silently NATed. Tighten the TAP/iptables setup so the guest gets internet egress and **nothing else**, and so teardown fully restores prior host state.

## Behavior / fix

### `scripts/setup-tap.sh`

1. **Source-restrict NAT.** Current rule NATs all outbound:
   `iptables -t nat -A POSTROUTING -o eth0 -j MASQUERADE`. Add `-s 172.16.0.0/24` so only rooms traffic is masqueraded.
2. **Block guest → LAN** with explicit drops, ordered **before** the egress accept:
   - `FORWARD -i tap-fc0 -d 192.168.0.0/16 -j DROP`
   - `FORWARD -i tap-fc0 -d 10.0.0.0/8 -j DROP`
   - `FORWARD -i tap-fc0 -d 172.16.0.0/12 ! -s 172.16.0.0/24 -j DROP`
   Then preserve internet egress: `FORWARD -i tap-fc0 -o eth0 -j ACCEPT` (after the drops).
3. **Scope forwarding per-interface.** Replace the kernel-wide `net.ipv4.ip_forward=1` with `net.ipv4.conf.tap-fc0.forwarding=1`. Capture the prior global `ip_forward` state so teardown can restore it.

### `scripts/teardown-tap.sh`

Undo every rule the setup adds — the source-restricted MASQUERADE, the three guest→LAN drops, the scoped egress accept — and restore the prior global `ip_forward` value captured at setup time. Teardown must be idempotent (safe to run when rules are already gone).

## Acceptance

- MASQUERADE rule carries `-s 172.16.0.0/24`.
- The three guest→LAN DROP rules exist and sit **before** the egress ACCEPT in the FORWARD chain.
- Internet egress preserved via `FORWARD -i tap-fc0 -o eth0 -j ACCEPT`.
- Per-interface `conf.tap-fc0.forwarding=1` is set instead of the global flag; prior global state recorded.
- `scripts/teardown-tap.sh` removes all of the above and restores the prior global `ip_forward`; re-running it is a no-op.
- **Host e2e (rooms-host, gates merge):** from inside a booted guest, `curl https://api.anthropic.com` succeeds; `ping <operator home router IP>` and a connect to an RFC1918 host are **blocked**.

## Test plan

- Shell-level: a small assertion script (or documented manual steps) that greps the live `iptables -S` / `iptables -t nat -S` output for the expected rules after `setup-tap.sh`, and asserts they're gone after `teardown-tap.sh`. Host-only / `e2e`-gated — needs root + a real TAP.
- The egress-succeeds / LAN-blocked check is the host e2e above; it can only run on `rooms-host`.

## Non-goals

- TAP ownership by a dedicated `firecracker` user — that's `firecracker-under-jailer`'s concern (it also edits `setup-tap.sh`; sequenced after this lands).
- Per-room network namespaces or nftables migration — iptables hardening is the v0 mitigation.
- Egress allow-listing by destination (only api.anthropic.com etc.) — broad internet egress stays for v0; LAN isolation is the win here.

## Validation / drive notes

**Cloud-written, host-gated merge.** The cursor cloud agent edits the two scripts and the rule-assertion harness; `make check` covers nothing here (pure bash), so correctness rides on review + the **rooms-host e2e**, which the operator runs before merge (cloud VMs have no KVM/Firecracker/TAP). Reconcile against the current `setup-tap.sh` before editing — confirm the live rule set matches what this spec assumes (the script may have drifted since the security review).
