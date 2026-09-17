"""A swarm peer: claim an unclaimed task, work, heartbeat the lease, complete with its token.

    python3 peer.py --store file:DIR|resp:HOST:PORT --peer-id X --tasks N

The peer seeds tasks task-0000..N-1 (idempotent), then loops until none are
pending. Every event is one JSON line on stdout or --log; `misses` counts
acquire calls that lost to another holder. Store failures are
retried with capped exponential backoff; after --retries consecutive failures
of one operation the peer logs why and exits 2 rather than crash.

A retried call may already have been applied before the connection broke, so a
peer can log a completion as rejected when the store accepted the first try.
The store's own receipts are the record; the peer log is the peer's view.
"""

import argparse
import json
import random
import sys
import time

from store import open_store, wall_ms


class GiveUp(Exception):
    """The store stayed unreachable for the whole retry budget."""


def task_ids(count):
    return ["task-%04d" % n for n in range(count)]


def offset_clock(path):
    """Wall clock plus the integer millisecond offset in `path`, re-read on every
    call so a fault injector can step this peer's clock while it runs."""
    def clock():
        return wall_ms() + _read_offset(path)
    return clock


def _read_offset(path):
    if not path:
        return 0
    try:
        with open(path, encoding="utf-8") as fh:
            return int(fh.read().strip() or 0)
    except (OSError, ValueError):
        return 0


class Peer:
    def __init__(self, store, opts, out):
        self.store = store
        self.opts = opts
        self.out = out
        self.rng = random.Random(opts.peer_id)
        self.idle_misses = 0

    def log(self, event, **fields):
        record = dict(fields, ev=event, peer=self.opts.peer_id, t=wall_ms())
        self.out.write(json.dumps(record, sort_keys=True) + "\n")
        self.out.flush()

    def retry(self, op, call):
        for attempt in range(self.opts.retries):
            try:
                return call()
            except OSError as err:
                self.log("retry", op=op, attempt=attempt, error=str(err))
                time.sleep(min(1.0, 0.05 * 2 ** attempt))
        raise GiveUp(op)

    def run(self):
        self.retry("seed", lambda: self.store.seed(task_ids(self.opts.tasks)))
        while True:
            started = time.monotonic()
            pending = self.retry("pending", self.store.pending)
            if not pending:
                return
            claim = self.claim(pending, started)
            if claim is None:
                time.sleep(self.opts.poll_ms / 1000.0)
                continue
            self.work(*claim)

    def claim(self, pending, started):
        """Try pending tasks in this peer's own random order, which spreads contention."""
        self.rng.shuffle(pending)
        ttl = self.opts.ttl_ms
        for misses, task in enumerate(pending):
            tried = time.monotonic()
            token = self.retry("acquire", lambda: self.store.acquire(task, self.opts.peer_id, ttl))
            if token is None:
                continue
            now = time.monotonic()
            self.log("claim", task=task, token=token, misses=misses,
                     claim_ms=(now - started) * 1000, acquire_ms=(now - tried) * 1000)
            return task, token
        self.idle_misses += len(pending)
        return None

    def work(self, task, token):
        me, ttl = self.opts.peer_id, self.opts.ttl_ms
        work_s = self.opts.work_ms * self.rng.uniform(0.5, 1.5) / 1000.0
        deadline = time.monotonic() + work_s
        while time.monotonic() < deadline:
            self.busy(min(ttl / 3000.0, deadline - time.monotonic()))
            held = self.retry("extend", lambda: self.store.extend(task, me, token, ttl))
            if not held and not self.opts.reckless:
                self.log("lost", task=task, token=token)
                return
        accepted = self.retry("complete", lambda: self.store.complete(task, me, token, "ok"))
        self.log("complete", task=task, token=token, accepted=accepted)

    def busy(self, seconds):
        if not self.opts.cpu:
            time.sleep(max(0.0, seconds))
            return
        until = time.monotonic() + seconds
        while time.monotonic() < until:
            pass


def parse_args(argv):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--store", required=True, help="file:DIR or resp:HOST:PORT")
    parser.add_argument("--peer-id", required=True)
    parser.add_argument("--tasks", type=int, required=True, help="seed task-0000..N-1")
    parser.add_argument("--prefix", default="swarm", help="key prefix for resp stores")
    parser.add_argument("--ttl-ms", type=int, default=1000)
    parser.add_argument("--work-ms", type=int, default=50, help="mean simulated work per task")
    parser.add_argument("--poll-ms", type=int, default=20, help="wait when nothing is claimable")
    parser.add_argument("--retries", type=int, default=10, help="attempts per store operation")
    parser.add_argument("--cpu", action="store_true", help="spin instead of sleeping")
    parser.add_argument("--reckless", action="store_true",
                        help="keep working after losing the lease, so the store's fencing is tested")
    parser.add_argument("--clock-offset-file", help="file holding a clock offset in ms")
    parser.add_argument("--log", help="append events here instead of stdout")
    return parser.parse_args(argv)


def main(argv=None):
    opts = parse_args(argv)
    store = open_store(opts.store, offset_clock(opts.clock_offset_file), opts.prefix)
    out = open(opts.log, "a", encoding="utf-8") if opts.log else sys.stdout
    peer = Peer(store, opts, out)
    try:
        return finish(peer)
    finally:
        store.close()
        if opts.log:
            out.close()


def finish(peer):
    try:
        peer.run()
    except GiveUp as err:
        peer.log("exit", reason="store unreachable during %s" % err, misses=peer.idle_misses)
        return 2
    peer.log("exit", reason="no pending tasks", misses=peer.idle_misses)
    return 0


if __name__ == "__main__":
    sys.exit(main())
