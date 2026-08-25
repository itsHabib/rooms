# Snapshot matrix positive/mutant proof — 2026-08-25

This is the retained proof for the first `rooms matrix` candidate. It used the
immutable snapshot from the Phase-2 proof as a shared starting point, then ran
the checked-in [`positive-mutant.json`](../../examples/matrix/positive-mutant.json)
manifest with host-side witness capture and `--egress none`.

## Invocation

```sh
sudo -E rooms matrix "$SNAPSHOT_DIR" \
  --image "$ROOTFS" \
  --cases examples/matrix/positive-mutant.json \
  --out "$EVIDENCE_DIR" \
  --witness --egress none --max-wall 30s --json
```

- Snapshot ID: `01m0mqyxys20q9fe621043kyf0`
- Matrix SHA-256:
  `910aea4934a7888dd7484b340946f0774d90b884ebf08c00a612b0fb83333084`
- Candidate release-binary SHA-256:
  `20e340b520839ef841dc071cb3918cb3cf880eb60b11b4189692a80bab8ac908`

## Result

| Case | Command SHA-256 | Observation | Host exit | Clone net | Result |
| --- | --- | --- | --- | --- | --- |
| `clean` | `cda802e0decedd4d22e66e4ff721c8bec5e40d88f6719f81e4d926dbc81839b9` | `green` | 0 | `rooms-c2` | positive control passed |
| `mutant-detected` | `4e5231d4a418e4c77dacbf4c3459fe974ef4dd0e89126b81b683778fb531d568` | `red` | 0 | `rooms-c1` | negative control detected its mutant |

Both cases emitted their own `result.json`, `changeset.json`, `witness.json`,
`witness.pcap`, and `observation.txt`. Both witness receipts were complete,
recorded policy `none`, and reported empty permitted, destination, blocked, and
DNS-query sets. The PCAPs had different hashes, demonstrating separate captures
rather than one copied receipt (`2aacd65c…` for `clean`, `6f98a57c…` for
`mutant-detected`).

The terminal `rooms ls --json` roster was empty. The clone namespace and link
audit was also empty, so the run returned both clone-network identities and left
no kept room behind.

## Boundary

This proves snapshot lineage, distinct command assignment, case-partitioned
evidence, host-side no-egress custody, and clean teardown for a deliberately
small positive/mutant pair. It does not prove a RoxIQ outcome, inject a real
browser mutant, or grant deploy, repair, PR, or merge authority. That consumer
experiment is the next layer.
