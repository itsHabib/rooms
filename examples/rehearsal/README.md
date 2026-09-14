# See a change on its worst day

A small experiment you can run now: compare a payment handler with an
idempotency patch. Both pass ordinary delivery. Lose one acknowledgement after
the ledger append, then redeliver through a fresh process: the original records
the payment twice. A third scenario checks that two legitimate payments for the
same amount both survive the fix.

This is a synthetic sequential specimen, not production payment code. It is a
thin consumer of `rooms matrix`; no payment logic lives in the Rust runtime.

## Try it without a VM

From the repository root, with Python 3.9+ and POSIX `sh`/`awk`:

```sh
python3 examples/rehearsal/lab.py demo --out /tmp/payment-rehearsal
open /tmp/payment-rehearsal/report.html     # macOS; open the file in any browser
```

The report explicitly says **Local processes — no VM isolation evidence**.
All six cases execute real shell handlers in separate directories. Nothing is
simulated in the displayed ledger, but these processes have no sandbox boundary.
Only the checked-in trusted specimen runs; there is no arbitrary-command input.

## Run the same specimen in six Rooms

On an already provisioned Linux/KVM rooms-host, supply a compatible neutral
snapshot, its immutable image, and a built Rooms binary. Snapshot creation and
retention use the existing [warm workflow](../../README.md#cli-surface).

```sh
sudo -E python3 examples/rehearsal/lab.py run \
  --rooms /absolute/path/to/rooms \
  --snapshot /absolute/path/to/snapshot \
  --image /absolute/path/to/rootfs.ext4 \
  --out /tmp/payment-rehearsal-rooms
```

This calls `rooms matrix` with six distinct commands, `--witness --egress none
--max-wall 30s --json`. Each command embeds the specimen; the snapshot does not
need this checkout or Python. Every case starts from the same neutral snapshot.
No network services, guest installation, model calls, or credentials are needed.
Rooms owns allocation, cancellation, collection, and teardown. The wrapper
preserves terminal stdout, stderr, and process exit status, including on failure.
The wall cap applies to workloads; restore readiness can take longer.

The output path must be new. A reused output path is refused. Generated files
from a privileged run may require `sudo` to read; copy only the report artifacts
for viewing if desired. The tool does not delete snapshots or existing evidence.

## What to inspect

| Condition | Original | With idempotency |
| --- | --- | --- |
| Ordinary delivery | Pass: one payment | Pass: one payment |
| Reply lost, redelivery | Fail: two entries for one event | Pass: one entry |
| Two events, same amount | Pass: both preserved | Pass: both preserved |

The host oracle checks exact ledger entries **and** the delivery trace, separately
from handler exit status. A guest file claiming `passed: true` has no effect.
Incomplete execution, mismatched command/manifest identity, missing artifacts,
malformed observations, and nonzero command exits are **inconclusive**. An empty
but successfully collected ledger is a complete observation of incorrect behavior.

Exit **0** means the comparison completed, even if a behavior failed (the baseline
failure is intentional). Exit **2** means invalid/incomplete evidence or an execution
error. Neither result grants merge or deployment authority.

The retained run contains:

- `report.html`: offline comparison, expandable raw evidence, no external assets.
- `summary.md` and `comparison.json`: human and machine-readable findings.
- `experiment.json` and `matrix.json`: specimen/oracle identity and exact commands.
- `execution.json` and `process.json`: execution outcome, including failure records.
- `evidence/<case-id>/`: ledger and trace; Rooms also collects runner logs,
  changeset, and witness artifacts. Witnesses are retained for inspection; this
  example's behavioral oracle does not interpret network captures.

Rebuild the report with the original checkout, without executing the manifest:

```sh
python3 examples/rehearsal/lab.py report --out /tmp/payment-rehearsal
```

The tool verifies the retained commands against the checked-in specimen and pins
its oracle source. Changed specimen/oracle code makes old evidence inconclusive;
keep the original revision when archiving an experiment. Hashes bind inspected
bytes, not authorship. Collected guest observations remain guest-origin evidence.

## Scope of the result

This proves the behavior of the exercised **sequential** delivery schedules.
The patch uses a ledger lookup and append; it is deliberately not a concurrent
transaction. Acknowledgement loss occurs after append completes. This does not
exercise crashes during append, disk durability, concurrent deliveries, external
payment APIs, arbitrary hostile guests, or byte-for-byte replay.

The next specimen can target another invariant, but should bring its own explicit
fault, independent oracle, and a negative control against a plausible wrong fix.
The [design](../../docs/features/rehearsal-lab/spec.md) records the contract.

## Test

```sh
python3 -m unittest discover -s examples/rehearsal -p 'test_*.py' -v
```

Tests execute the actual shell specimen, including a wrong patch that deduplicates
by amount and loses a legitimate event, plus malformed-evidence controls.

The `rehearsal` CI job publishes a `payment-rehearsal` artifact containing the
local-process report and its underlying evidence, so reviewers can inspect the
experience without provisioning a VM.
