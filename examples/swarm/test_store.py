"""One contract, every backend: file, the fake RESP server, and a real server if installed."""

import os
import subprocess
import tempfile
import threading
import time
import unittest

import bench
import faults
from fake_resp import FakeRespServer
from store import FileStore, RespStore, open_store


class StoreContract:
    """Mixed into a TestCase that provides client(): a new handle on the shared backend."""

    def setUp(self):
        self.store = self.client()
        self.store.seed(["a", "b"])

    def test_seed_is_idempotent_and_lists_tasks(self):
        self.client().seed(["a", "b"])
        self.assertEqual(self.store.tasks(), ["a", "b"])
        self.assertEqual(self.store.pending(), ["a", "b"])

    def test_a_held_lease_cannot_be_acquired(self):
        self.assertEqual(self.store.acquire("a", "p1", 5000), 1)
        self.assertIsNone(self.store.acquire("a", "p2", 5000))
        self.assertEqual(self.store.acquire("b", "p2", 5000), 1)

    def test_release_frees_the_lease_and_the_next_token_is_larger(self):
        token = self.store.acquire("a", "p1", 5000)
        self.assertFalse(self.store.release("a", "p2", token))
        self.assertTrue(self.store.release("a", "p1", token))
        self.assertGreater(self.store.acquire("a", "p2", 5000), token)

    def test_extend_is_only_for_the_current_holder(self):
        token = self.store.acquire("a", "p1", 5000)
        self.assertTrue(self.store.extend("a", "p1", token, 5000))
        self.assertFalse(self.store.extend("a", "p2", token, 5000))
        self.assertFalse(self.store.extend("a", "p1", token + 1, 5000))

    def test_extend_keeps_a_short_lease_alive(self):
        token = self.store.acquire("a", "p1", 150)
        for _ in range(4):
            time.sleep(0.06)
            self.assertTrue(self.store.extend("a", "p1", token, 150))
        self.assertIsNone(self.store.acquire("a", "p2", 150))

    def test_complete_is_accepted_once(self):
        token = self.store.acquire("a", "p1", 5000)
        self.assertTrue(self.store.complete("a", "p1", token, "ok"))
        self.assertFalse(self.store.complete("a", "p1", token, "again"))
        self.assertIsNone(self.store.acquire("a", "p2", 5000))
        self.assertEqual(self.store.pending(), ["b"])

    def test_a_token_that_was_never_granted_is_rejected(self):
        self.store.acquire("a", "p1", 5000)
        self.assertFalse(self.store.complete("a", "p1", 7, "forged"))
        self.assertEqual(self.store.pending(), ["a", "b"])

    def test_expiry_takeover_and_stale_token_rejection(self):
        """The model's counterexample, replayed: the paused holder must lose."""
        stale = self.store.acquire("a", "p1", 60)
        time.sleep(0.12)
        fresh = self.store.acquire("a", "p2", 5000)
        self.assertGreater(fresh, stale)
        self.assertFalse(self.store.extend("a", "p1", stale, 5000))
        self.assertFalse(self.store.complete("a", "p1", stale, "late"))
        self.assertTrue(self.store.complete("a", "p2", fresh, "ok"))
        receipts = [(r["holder"], r["token"], r["accepted"]) for r in self.store.receipts()]
        self.assertCountEqual(receipts, [("p1", stale, False), ("p2", fresh, True)])
        verdict = faults.check(["a"], self.store.history(), live_peers=1)
        self.assertTrue(verdict["ok"], verdict)
        self.assertEqual(verdict["rejected_completions"], 1)

    def test_history_orders_grants_before_their_completion(self):
        token = self.store.acquire("a", "p1", 5000)
        self.store.complete("a", "p1", token, "ok")
        kinds = [(e["kind"], e["token"]) for e in self.store.history() if e["task"] == "a"]
        self.assertEqual(kinds, [("grant", 1), ("complete", 1)])

    def test_racing_acquires_have_one_winner(self):
        tokens = []

        def race(name):
            tokens.append(self.client().acquire("a", name, 5000))

        threads = [threading.Thread(target=race, args=("p%d" % n,)) for n in range(8)]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join()
        self.assertEqual(sorted(t for t in tokens if t is not None), [1])


class FileStoreTest(StoreContract, unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        super().setUp()

    def client(self, clock=None):
        return FileStore(self.tmp.name, clock) if clock else FileStore(self.tmp.name)

    def test_a_fast_clock_takes_over_but_cannot_break_fencing(self):
        held = self.store.acquire("a", "p1", 5000)
        skewed = self.client(clock=lambda: int(time.time() * 1000) + 60000)
        taken = skewed.acquire("a", "p2", 5000)
        self.assertEqual(taken, held + 1)
        self.assertFalse(self.store.complete("a", "p1", held, "late"))
        self.assertTrue(skewed.complete("a", "p2", taken, "ok"))

    def test_an_accepted_completion_survives_a_lost_receipt_line(self):
        token = self.store.acquire("a", "p1", 5000)
        self.store.complete("a", "p1", token, "ok")
        os.unlink(os.path.join(self.tmp.name, "receipts.ndjson"))
        self.assertEqual([r["accepted"] for r in self.store.receipts()], [True])

    def test_open_store_parses_specs(self):
        self.assertIsInstance(open_store("file:" + self.tmp.name), FileStore)
        self.assertEqual(open_store("resp:127.0.0.1:6379").addr, ("127.0.0.1", 6379))
        for bad in ("file:", "resp:nohost", "nats:x:1"):
            self.assertRaises(ValueError, open_store, bad)


class FakeRespStoreTest(StoreContract, unittest.TestCase):
    def setUp(self):
        self.server = FakeRespServer().start()
        self.addCleanup(self.server.stop)
        self.clients = []
        self.addCleanup(lambda: [c.close() for c in self.clients])
        super().setUp()

    def client(self):
        self.clients.append(RespStore("127.0.0.1", self.server.port))
        return self.clients[-1]

    def test_prefixes_isolate_runs_on_one_server(self):
        other = RespStore("127.0.0.1", self.server.port, prefix="other")
        self.clients.append(other)
        self.store.acquire("a", "p1", 5000)
        other.seed(["a"])
        self.assertEqual(other.acquire("a", "p2", 5000), 1)

    def test_a_restarted_journaled_server_keeps_state_and_the_client_reconnects(self):
        with tempfile.TemporaryDirectory() as tmp:
            journal = os.path.join(tmp, "journal.ndjson")
            first = FakeRespServer(journal_path=journal).start()
            store = RespStore("127.0.0.1", first.port)
            self.clients.append(store)
            store.seed(["a"])
            token = store.acquire("a", "p1", 5000)
            first.stop()
            store.close()
            self.assertRaises(OSError, store.pending)
            second = FakeRespServer(port=first.port, journal_path=journal).start()
            self.addCleanup(second.stop)
            self.assertIsNone(store.acquire("a", "p2", 5000))
            self.assertTrue(store.complete("a", "p1", token, "ok"))


@unittest.skipUnless(bench.real_server(), "no valkey-server or redis-server on PATH")
class RealRespStoreTest(StoreContract, unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.port = bench.free_port()
        self.proc = subprocess.Popen(bench.server_command("real", self.port, self.tmp.name),
                                     stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        self.addCleanup(self.proc.wait)
        self.addCleanup(self.proc.kill)
        self.clients = []
        self.addCleanup(lambda: [c.close() for c in self.clients])
        self._await_server()
        super().setUp()

    def _await_server(self):
        for _ in range(250):
            try:
                return RespStore("127.0.0.1", self.port).call("PING")
            except OSError:
                time.sleep(0.02)
        return self.fail("server did not start")

    def client(self):
        self.clients.append(RespStore("127.0.0.1", self.port))
        return self.clients[-1]


if __name__ == "__main__":
    unittest.main()
