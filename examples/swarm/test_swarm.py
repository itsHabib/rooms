"""Peers and the bench end to end, small enough for CI: real processes, both backends."""

import io
import json
import os
import tempfile
import unittest

import bench
import peer
from store import FileStore


def run_peer(args):
    out = io.StringIO()
    opts = peer.parse_args(args)
    store = peer.open_store(opts.store, peer.offset_clock(opts.clock_offset_file))
    worker = peer.Peer(store, opts, out)
    worker.run()
    return [json.loads(line) for line in out.getvalue().splitlines()]


class PeerTest(unittest.TestCase):
    def test_one_peer_completes_every_task_once(self):
        with tempfile.TemporaryDirectory() as tmp:
            events = run_peer(["--store", "file:" + tmp, "--peer-id", "solo", "--tasks", "5",
                               "--work-ms", "2", "--ttl-ms", "500"])
            receipts = FileStore(tmp).receipts()
        self.assertEqual(sorted(r["task"] for r in receipts if r["accepted"]), peer.task_ids(5))
        self.assertEqual(len([e for e in events if e["ev"] == "complete" and e["accepted"]]), 5)

    def test_an_unreachable_store_ends_the_peer_with_exit_2_not_a_crash(self):
        args = ["--store", "resp:127.0.0.1:%d" % bench.free_port(), "--peer-id", "lonely",
                "--tasks", "1", "--retries", "2"]
        with tempfile.TemporaryDirectory() as tmp:
            log = os.path.join(tmp, "log.ndjson")
            self.assertEqual(peer.main(args + ["--log", log]), 2)
            with open(log, encoding="utf-8") as fh:
                events = [json.loads(line) for line in fh]
        self.assertEqual([e["ev"] for e in events], ["retry", "retry", "exit"])

    def test_the_clock_offset_file_is_re_read(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "offset")
            clock = peer.offset_clock(path)
            before = clock()
            with open(path, "w", encoding="utf-8") as fh:
                fh.write("-90000")
            self.assertLess(clock(), before - 80000)


class BenchTest(unittest.TestCase):
    FLAGS = ["--ttl-ms", "300", "--work-ms", "20", "--reckless"]

    def test_a_small_swarm_keeps_the_invariants_on_both_backends(self):
        for backend in ("file", "fake"):
            with tempfile.TemporaryDirectory() as tmp:
                result = bench.run_once(tmp, "run", backend, 3, 12, self.FLAGS, timeout_s=30)
                self.assertTrue(os.path.exists(os.path.join(tmp, "run", "history.ndjson")))
            self.assertTrue(result["finished"], result)
            self.assertTrue(result["invariants"]["ok"], result)
            self.assertEqual(result["invariants"]["accepted"], 12)
            self.assertEqual(result["claim_ms"]["n"], 12)

    def test_a_pause_longer_than_the_ttl_and_a_store_outage_are_survived(self):
        events = [{"at_ms": 100, "action": "pause", "peer": 0},
                  {"at_ms": 150, "action": "store_kill", "peer": None},
                  {"at_ms": 450, "action": "store_restart", "peer": None},
                  {"at_ms": 900, "action": "resume", "peer": 0}]
        for backend in ("file", "fake"):
            with tempfile.TemporaryDirectory() as tmp:
                result = bench.run_once(tmp, "run", backend, 3, 40, self.FLAGS, events, 30)
            self.assertTrue(result["invariants"]["ok"], result)
            self.assertGreater(result["store_retries"], 0, result)

    def test_an_existing_output_directory_is_refused(self):
        with tempfile.TemporaryDirectory() as tmp:
            self.assertEqual(bench.main(["--out", tmp]), 2)

    def test_percentiles(self):
        self.assertEqual(bench.percentiles([]), {"n": 0})
        self.assertEqual(bench.percentiles(list(range(1, 101))),
                         {"n": 100, "p50": 51, "p95": 96, "max": 100})


if __name__ == "__main__":
    unittest.main()
