"""Seeded fault schedules for a swarm, and the invariants a run must keep.

A schedule is a list of timed events. Disruptions come in pairs, so every
schedule ends with all peers and the store running again:

    pause / resume               SIGSTOP then SIGCONT one peer
    kill / restart               SIGKILL one peer, start it again later
    store_kill / store_restart   take the store away, bring it back
    skew                         step one peer's clock by a signed offset (no undo)

The same arguments always give the same schedule, byte for byte: only
rng.random() is drawn from, and the encoding is canonical JSON. The schedule is
deterministic; the processes it is replayed against are not, so a seed names a
fault pattern to retry rather than an exact interleaving.

    python3 faults.py --seed 7 --peers 8      # print a schedule
"""

import argparse
import json
import random
import sys
import time

KINDS = ("pause", "kill", "store", "skew")
PAIRS = {"pause": ("pause", "resume"), "kill": ("kill", "restart"),
         "store": ("store_kill", "store_restart")}


def _pick(rng, below):
    return int(rng.random() * below)


def schedule(seed, peers, ttl_ms=400, duration_ms=3000, faults=8):
    """Disruptions last 0.5 to 3 ttl, so some pauses outlive the lease and some do not."""
    rng = random.Random(seed)
    events = []
    for _ in range(faults):
        kind = KINDS[_pick(rng, len(KINDS))]
        at_ms = _pick(rng, duration_ms)
        peer = _pick(rng, peers)
        span_ms = ttl_ms // 2 + _pick(rng, ttl_ms * 5 // 2)
        sign = 1 if rng.random() < 0.5 else -1
        events.extend(_expand(kind, at_ms, peer, span_ms, sign))
    return sorted(events, key=lambda event: event["at_ms"])


def _expand(kind, at_ms, peer, span_ms, sign):
    if kind == "skew":
        return [{"at_ms": at_ms, "action": "skew", "peer": peer, "offset_ms": sign * span_ms}]
    begin, end = PAIRS[kind]
    target = None if kind == "store" else peer
    return [{"at_ms": at_ms, "action": begin, "peer": target},
            {"at_ms": at_ms + span_ms, "action": end, "peer": target}]


def dumps(events):
    """Canonical bytes: equal schedules encode identically."""
    return json.dumps(events, sort_keys=True, separators=(",", ":")).encode() + b"\n"


def replay(events, swarm):
    """Call swarm.<action>(event) at each event's offset from now."""
    started = time.monotonic()
    for event in events:
        wait_s = event["at_ms"] / 1000.0 - (time.monotonic() - started)
        time.sleep(max(0.0, wait_s))
        getattr(swarm, event["action"])(event)


def check(tasks, history, live_peers):
    """Check a finished run. `history` is the store's ordered grant and completion
    events; `live_peers` counts peers still able to work when the run ended.

    exactly-once   every task has exactly one accepted completion
    stale-token    no completion was accepted once a newer token had been granted
    lost-task      no task is left unfinished while a live peer exists
    """
    # The stale-token rule reads `history` in order: it relies on history()
    # returning each task's grants before the completions that followed them.
    newest = {}
    accepted = {task: 0 for task in tasks}
    violations = []
    for event in history:
        task, token = event["task"], event["token"]
        if event["kind"] == "grant":
            newest[task] = max(newest.get(task, 0), token)
            continue
        if not event["accepted"]:
            continue
        accepted[task] = accepted.get(task, 0) + 1
        if token < newest.get(task, 0):
            violations.append({"rule": "stale-token", "task": task, "token": token,
                               "newest": newest[task]})
    for task, count in sorted(accepted.items()):
        if count != 1:
            violations.append({"rule": "exactly-once", "task": task, "accepted": count})
        if count == 0 and live_peers > 0:
            violations.append({"rule": "lost-task", "task": task, "live_peers": live_peers})
    return {
        "ok": not violations,
        "violations": violations,
        "accepted": sum(accepted.values()),
        "accepted_duplicates": sum(count - 1 for count in accepted.values() if count > 1),
        "rejected_completions": sum(
            1 for e in history if e["kind"] == "complete" and not e["accepted"]),
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--seed", type=int, required=True)
    parser.add_argument("--peers", type=int, required=True)
    parser.add_argument("--ttl-ms", type=int, default=400)
    parser.add_argument("--duration-ms", type=int, default=3000)
    parser.add_argument("--faults", type=int, default=8)
    opts = parser.parse_args()
    events = schedule(opts.seed, opts.peers, opts.ttl_ms, opts.duration_ms, opts.faults)
    sys.stdout.write(dumps(events).decode())
    return 0


if __name__ == "__main__":
    sys.exit(main())
