# Rehearsal lab: see a change on its worst day

Status: implementation

## Experience

Run a six-case payment-delivery experiment and open one self-contained HTML
report. Compare an append-on-delivery baseline with an event-ID idempotency
patch under normal delivery, acknowledgement loss followed by redelivery in a
new process, and two legitimate events carrying the same amount.

This is a deliberately small synthetic specimen, not a production payment
processor or a claim about a historical incident. The baseline should pass
ordinary delivery and fail redelivery. The candidate must preserve distinct
events as well as suppress duplicates. The report shows ledger entries,
delivery traces, command completion, and an independent host-side oracle.

## Boundary

This is an example consumer under `examples/rehearsal/`. No changes to the
Rust runtime, VM lifecycle, matrix schema, snapshot policy, or merge authority.
Python 3 standard library on the host; POSIX shell/awk in the guest. No remote
APIs, credentials, dependencies, or guest installation. Commands embed the
fixture, so any compatible neutral snapshot can run the experiment.

`demo` executes the same fixture in separate local directories and explicitly
labels its report as local processes, without VM isolation evidence. `run`
uses the real `rooms matrix` with no egress, witnessed traffic, and a wall cap.
Both produce a retained experiment manifest, execution record, raw observations,
JSON comparison, Markdown summary, and offline HTML report. `report` rebuilds
the comparison without executing any command from the retained manifest.

## Evidence and acceptance

- The host checks exact ledger entries and acknowledgement/attempt traces;
  process exit zero alone never means the behavior passed.
- Six explicit cases, two candidates times three scenarios; the expected ledger
  and trace come from the host oracle, separately from the guest handler.
- Validate the execution schema, terminal status, exact manifest/command hashes,
  case membership, and (for Rooms) unique room/network identities and common
  snapshot lineage before evaluating observations.
- Missing, malformed, partial, cancelled, failed, or mismatched execution
  evidence yields `inconclusive`, never a passing behavior result. Nonzero
  command exits also yield inconclusive. Complete incorrect observations fail.
- Artifact content is escaped in HTML. Reports load no remote assets or code.
  Inputs are bounded regular files under the retained run directory; refuse
  symlinks and existing output roots to avoid accidental reuse of evidence.
- Exit 0 means a complete comparison, including expected behavioral failures;
  exit 2 means incomplete/invalid evidence or execution failure. This consumer
  grants no publication or deployment authority.
- Unit/CLI tests exercise the real shell specimen, duplicate handling, the
  distinct-event negative control, forged self-reported success, truncated and
  mismatched evidence, output reuse, and report escaping. Run `make check` and
  a real six-clone matrix on rooms-host when available.

## Limits

The handler's append/check is sequential and deliberately not concurrency-safe.
The controlled fault is acknowledgement loss after a completed ledger append;
this does not prove atomicity during a crash, disk durability, arbitrary retry
schedules, hostile-guest evidence integrity, or deterministic model/network
replay. Hashes bind inspected bytes; they are not signatures. Snapshot and image
lifecycle remains operator-managed. The HTML is an artifact viewer, not a
hosted app or a new Rooms control plane.
