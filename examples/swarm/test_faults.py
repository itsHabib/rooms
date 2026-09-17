"""Schedules are a pure function of their arguments; the checker catches each broken rule."""

import os
import subprocess
import sys
import tempfile
import unittest

import faults
from store import FileStore

HERE = os.path.dirname(os.path.abspath(__file__))


def grant(task, token, holder="p1"):
    return {"kind": "grant", "task": task, "holder": holder, "token": token}


def complete(task, token, accepted, holder="p1"):
    return {"kind": "complete", "task": task, "holder": holder, "token": token,
            "accepted": accepted}


class ScheduleTest(unittest.TestCase):
    def test_same_seed_same_bytes(self):
        for seed in range(20):
            self.assertEqual(faults.dumps(faults.schedule(seed, 8)),
                             faults.dumps(faults.schedule(seed, 8)))

    def test_same_seed_same_bytes_across_processes_and_hash_seeds(self):
        outputs = set()
        for hash_seed in ("0", "1", "random"):
            env = dict(os.environ, PYTHONHASHSEED=hash_seed)
            command = [sys.executable, os.path.join(HERE, "faults.py"), "--seed", "7", "--peers", "8"]
            outputs.add(subprocess.check_output(command, env=env))
        self.assertEqual(outputs, {faults.dumps(faults.schedule(7, 8))})

    def test_seed_7_is_pinned(self):
        """Guards against an accidental change to how the generator draws."""
        first = faults.schedule(7, 8)[:2]
        self.assertEqual(first, [{"action": "skew", "at_ms": 139, "peer": 6, "offset_ms": 489},
                                 {"action": "kill", "at_ms": 173, "peer": 4}])

    def test_different_seeds_differ(self):
        encoded = {faults.dumps(faults.schedule(seed, 8)) for seed in range(20)}
        self.assertEqual(len(encoded), 20)

    def test_events_are_time_ordered_and_every_disruption_ends(self):
        for seed in range(50):
            events = faults.schedule(seed, 4, ttl_ms=400, duration_ms=3000, faults=10)
            times = [e["at_ms"] for e in events]
            self.assertEqual(times, sorted(times))
            for begin, end in faults.PAIRS.values():
                begun = sorted(str(e["peer"]) for e in events if e["action"] == begin)
                ended = sorted(str(e["peer"]) for e in events if e["action"] == end)
                self.assertEqual(begun, ended)

    def test_some_pauses_outlive_the_ttl_and_every_kind_appears(self):
        events = [e for seed in range(30) for e in faults.schedule(seed, 4, ttl_ms=400)]
        self.assertEqual({e["action"] for e in events},
                         {"pause", "resume", "kill", "restart", "store_kill", "store_restart", "skew"})
        self.assertTrue(any(abs(e["offset_ms"]) > 400 for e in events if e["action"] == "skew"))

    def test_replay_dispatches_each_event_in_order(self):
        class Recorder:
            def __init__(self):
                self.seen = []

            def __getattr__(self, action):
                return lambda event: self.seen.append((action, event["peer"]))

        events = faults.schedule(3, 2, ttl_ms=4, duration_ms=20, faults=4)
        recorder = Recorder()
        faults.replay(events, recorder)
        self.assertEqual(recorder.seen, [(e["action"], e["peer"]) for e in events])


class CheckTest(unittest.TestCase):
    def test_a_clean_history_passes(self):
        history = [grant("a", 1), grant("a", 2, "p2"), complete("a", 1, False),
                   complete("a", 2, True, "p2"), grant("b", 1), complete("b", 1, True)]
        verdict = faults.check(["a", "b"], history, live_peers=2)
        self.assertTrue(verdict["ok"], verdict)
        self.assertEqual((verdict["accepted"], verdict["rejected_completions"]), (2, 1))

    def test_a_double_acceptance_is_reported(self):
        history = [grant("a", 1), complete("a", 1, True), complete("a", 1, True)]
        verdict = faults.check(["a"], history, live_peers=1)
        self.assertEqual([v["rule"] for v in verdict["violations"]], ["exactly-once"])
        self.assertEqual(verdict["accepted_duplicates"], 1)

    def test_an_accepted_stale_token_is_reported(self):
        history = [grant("a", 1), grant("a", 2, "p2"), complete("a", 1, True)]
        verdict = faults.check(["a"], history, live_peers=1)
        self.assertEqual([v["rule"] for v in verdict["violations"]], ["stale-token"])

    def test_a_grant_after_the_acceptance_does_not_make_it_stale(self):
        history = [grant("a", 1), complete("a", 1, True), grant("a", 2, "p2")]
        self.assertTrue(faults.check(["a"], history, live_peers=1)["ok"])

    def test_an_unfinished_task_is_lost_only_if_a_peer_lives(self):
        rules = [v["rule"] for v in faults.check(["a"], [grant("a", 1)], 1)["violations"]]
        self.assertEqual(rules, ["exactly-once", "lost-task"])
        rules = [v["rule"] for v in faults.check(["a"], [grant("a", 1)], 0)["violations"]]
        self.assertEqual(rules, ["exactly-once"])

    def test_the_checker_catches_a_store_without_fencing(self):
        """A control: if the store accepted a stale token, the run would be flagged."""
        class Unfenced(FileStore):
            def complete(self, task, holder, token, result):
                self._link(task, max(self._slots(task)) + 1,
                           {"kind": "complete", "task": task, "holder": holder,
                            "token": token, "result": result, "accepted": True})
                return True

        with tempfile.TemporaryDirectory() as tmp:
            store = Unfenced(tmp, clock=lambda: 0)
            store.seed(["a"])
            stale = store.acquire("a", "p1", 10)
            store.clock = lambda: 100
            fresh = store.acquire("a", "p2", 10)
            store.complete("a", "p2", fresh, "ok")
            store.complete("a", "p1", stale, "late")
            rules = {v["rule"] for v in faults.check(["a"], store.history(), 1)["violations"]}
        self.assertEqual(rules, {"exactly-once", "stale-token"})


if __name__ == "__main__":
    unittest.main()
