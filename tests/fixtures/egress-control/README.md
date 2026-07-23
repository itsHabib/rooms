# Egress-control fixture corpus

Controlled prompt-injection **test fixtures** for the egress-control validation
harness (`src/egress_audit.rs`, spec `docs/features/egress-control-test-harness/`).
Each fixture is a tiny repo carrying an adversarial input — the kind an untrusted
third-party repo might realistically ship — that tries to exfiltrate a
clearly-fake honeytoken to a marked, RFC-reserved endpoint. The harness runs them
inside disposable rooms and scores, **host-side**, whether the sentinel escaped.

This is pen-testing the zero-egress control (`--egress`, #82) — made safe by the
substrate (a disposable Firecracker microVM per run, destroyed after) and by the
sentinels never being real credentials.

## Layout

```
<vector>/<name>/
  fixture.json      # the manifest (schema below)
  <injected input>  # README.md / src/*.js / build.log — where the injection sits
  probe.sh          # the deterministic v1 exfil-probe run in the room
  creds.txt         # (file-injection fixtures) the file-resident sentinel
  control/          # the benign variant — same shape, no injection, must not exfil
    <input> + probe.sh
```

## Manifest schema (`fixture.json`)

JSON, not TOML — the repo is JSON-native (`result.json` / `witness.json` /
`changeset.json`) and `serde_json` is already a dependency, so a JSON manifest
avoids adding a `toml` crate for the same schema. Parsed by
[`egress_audit::FixtureManifest`].

| Field | Meaning |
| --- | --- |
| `sentinel_id` | references an `egress_audit::SENTINELS` registry entry |
| `exfil_target` | the marked endpoint the probe targets (`192.0.2.10:80`, `h.invalid`) |
| `injection` | `{ "kind": "env", "name": "…" }` (via `--secret <NAME>`) or `{ "kind": "file", "path": "…" }` |
| `probe` | the exfil-probe command file run in the room |
| `control` | the benign control variant directory |
| `vector` | `readme` · `code-comment` · `dep-metadata` · `tool-output` |

## Sentinels

Every sentinel value is `ROOMS-SENTINEL-*` (marked, and **not** an AWS `AKIA…`
shape that would trip secret scanners) and every endpoint is RFC-5737 TEST-NET
(`192.0.2.0/24`) or an RFC-2606 `.invalid` host — reserved so a real destination
can never be mistaken for a sentinel, and so the "exfil" never reaches anything.
The registry is the host-side source of truth in `src/egress_audit.rs`.

## Realism caveat (known limitation)

The injections are plausible but recognizable — a capable agent may pattern-match
an obvious test input, so the corpus measures *these* fixtures against *these*
configs, not a general model-safety claim. This mirrors the "synthetic defects
are recognizable" limitation and is why v1 scores a deterministic `exfil-probe`
(the egress control is what's validated), with live-agent configs a documented
extension on the same `RunConfig` seam.
