"""Exhaustive (breadth-first) check of the abstract lease protocol.

Two peers, one task, one lease with a fence counter. A working peer may pause
for longer than the ttl; only then can its lease expire, because a running
holder heartbeats. The invariant is "at most one accepted completion".

Each peer moves idle -> ready (it saw the task pending) -> working -> finished,
with working <-> paused on the side. Three switches describe the store:

    fencing      complete is accepted only with the newest token granted
    done_flag    complete is accepted only if the task has no accepted completion
    stale_read   the "is it pending?" check is not atomic with the grant, so a
                 peer may acquire on the strength of an observation made earlier

    python3 model.py      # prints each configuration and any counterexample;
                          # exits 1 if a verdict is not the expected one
"""

import sys
from collections import deque, namedtuple

State = namedtuple("State", "holder counter accepted peers")  # peers: ((phase, token), ...)
Config = namedtuple("Config", "fencing done_flag stale_read")
Result = namedtuple("Result", "holds states trace")

INITIAL = State(holder=None, counter=0, accepted=0, peers=(("idle", 0), ("idle", 0)))


def _with_peer(state, index, phase, token, **changes):
    peers = list(state.peers)
    peers[index] = (phase, token)
    return state._replace(peers=tuple(peers), **changes)


def _peer_steps(state, index, config):
    phase, token = state.peers[index]
    name = "peer%d" % index
    if phase == "idle" and state.accepted == 0:
        yield name + " sees the task pending", _with_peer(state, index, "ready", 0)
    fresh = config.stale_read or state.accepted == 0
    if phase == "ready" and state.holder is None and fresh:
        grant = state.counter + 1
        yield ("%s acquires the lease, token %d" % (name, grant),
               _with_peer(state, index, "working", grant, holder=index, counter=grant))
    if phase == "paused":
        yield name + " resumes", _with_peer(state, index, "working", token)
    if phase != "working":
        return
    yield name + " pauses", _with_peer(state, index, "paused", token)
    newest = not config.fencing or token == state.counter
    first = not config.done_flag or state.accepted == 0
    holder = None if state.holder == index else state.holder
    if newest and first:
        yield ("%s completes with token %d: ACCEPTED" % (name, token),
               _with_peer(state, index, "finished", token, holder=holder,
                          accepted=state.accepted + 1))
        return
    yield ("%s completes with token %d: rejected" % (name, token),
           _with_peer(state, index, "finished", token, holder=holder))


def steps(state, config):
    """Every (label, next_state) enabled in `state`."""
    for index in range(len(state.peers)):
        yield from _peer_steps(state, index, config)
    if state.holder is not None and state.peers[state.holder][0] == "paused":
        yield ("lease expires while peer%d is paused" % state.holder,
               state._replace(holder=None))


def check(config):
    """Explore every reachable state; return the shortest trace to a double acceptance."""
    parent = {INITIAL: None}
    queue = deque([INITIAL])
    while queue:
        state = queue.popleft()
        if state.accepted > 1:
            return Result(False, len(parent), _trace(parent, state))
        for label, nxt in steps(state, config):
            if nxt not in parent:
                parent[nxt] = (state, label)
                queue.append(nxt)
    return Result(True, len(parent), [])


def _trace(parent, state):
    labels = []
    while parent[state] is not None:
        state, label = parent[state]
        labels.append(label)
    return labels[::-1]


# (title, config, whether the invariant is expected to hold)
CASES = (
    ("no token check", Config(fencing=False, done_flag=False, stale_read=False), False),
    ("token check", Config(fencing=True, done_flag=False, stale_read=False), True),
    ("token check, pending check not atomic with the grant",
     Config(fencing=True, done_flag=False, stale_read=True), False),
    ("token check + done flag, pending check not atomic",
     Config(fencing=True, done_flag=True, stale_read=True), True),
)


def main():
    """Print every verdict; exit 1 if any differs from what CASES expects."""
    surprises = 0
    for title, config, expected in CASES:
        result = check(config)
        surprises += result.holds != expected
        verdict = "holds" if result.holds else "VIOLATED"
        print("%s: at most one accepted completion %s (%d states)"
              % (title, verdict, result.states))
        for number, label in enumerate(result.trace, 1):
            print("  %d. %s" % (number, label))
    if surprises:
        print("%d verdict(s) differ from the expected ones" % surprises, file=sys.stderr)
    return 1 if surprises else 0


if __name__ == "__main__":
    sys.exit(main())
