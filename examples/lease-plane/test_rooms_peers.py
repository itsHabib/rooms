"""The pure parts of the in-Room runner. No VM, no Redis: the end-to-end path needs a KVM host."""

import argparse
import base64
import io
import os
import tarfile
import tempfile
import threading
import time
import unittest

import bench
import rooms_peers


def row(ok=True, accepted=120, peers_reporting=4):
    return {"invariants": {"ok": ok, "accepted": accepted}, "peers_reporting": peers_reporting}


class RunOkTest(unittest.TestCase):
    def test_a_run_that_did_all_the_work_passes(self):
        self.assertTrue(rooms_peers.run_ok(row(), 120))

    def test_broken_invariants_fail(self):
        self.assertFalse(rooms_peers.run_ok(row(ok=False), 120))

    def test_missing_work_fails_even_if_the_checker_was_happy(self):
        self.assertFalse(rooms_peers.run_ok(row(accepted=119), 120))

    def test_a_run_no_peer_reported_on_fails(self):
        self.assertFalse(rooms_peers.run_ok(row(peers_reporting=0), 120))
        self.assertFalse(rooms_peers.run_ok(row(accepted=0, peers_reporting=0), 0))


class NdjsonTest(unittest.TestCase):
    def read(self, text):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "peer.ndjson")
            with open(path, "w", encoding="utf-8") as fh:
                fh.write(text)
            return bench.read_ndjson(path)

    def test_a_whole_last_line_without_a_newline_is_kept(self):
        self.assertEqual(self.read('{"ev": "claim"}\n{"ev": "complete"}'),
                         [{"ev": "claim"}, {"ev": "complete"}])

    def test_a_torn_last_line_is_skipped(self):
        self.assertEqual(self.read('{"ev": "claim"}\n{"ev": "comp'), [{"ev": "claim"}])

    def test_blank_lines_and_an_empty_file_are_fine(self):
        self.assertEqual(self.read('\n{"ev": "exit"}\n\n'), [{"ev": "exit"}])
        self.assertEqual(self.read(""), [])

    def test_peer_events_collects_every_clone_directory(self):
        with tempfile.TemporaryDirectory() as tmp:
            for clone, text in (("clone-0", '{"peer": "a"}\n'), ("clone-1", '{"peer": "b"}')):
                os.makedirs(os.path.join(tmp, clone, "out"))
                with open(os.path.join(tmp, clone, "out", "peer.ndjson"), "w", encoding="utf-8") as fh:
                    fh.write(text)
            os.makedirs(os.path.join(tmp, "clone-2"))
            peers = sorted(e["peer"] for e in rooms_peers.peer_events(tmp))
        self.assertEqual(peers, ["a", "b"])


class InjectorTest(unittest.TestCase):
    def test_running_rooms_picks_ids_of_running_rows_only(self):
        listing = "ID STATE\nroom-a running 12s\nroom-b exited 0\nroom-c running 3s\n\n"
        self.assertEqual(rooms_peers.running_rooms(listing), ["room-a", "room-c"])
        self.assertEqual(rooms_peers.running_rooms(""), [])

    def test_quarter_done_treats_a_dead_store_as_not_there_yet(self):
        class Probe:
            def __init__(self, reply):
                self.reply = reply

            def call(self, *_args):
                if isinstance(self.reply, Exception):
                    raise self.reply
                return self.reply

        self.assertFalse(rooms_peers.quarter_done(Probe(29), 120))
        self.assertTrue(rooms_peers.quarter_done(Probe(30), 120))
        self.assertFalse(rooms_peers.quarter_done(Probe(ConnectionRefusedError()), 120))

    def test_await_progress_returns_promptly_once_stopped_with_no_store_at_all(self):
        opts = argparse.Namespace(host_ip="127.0.0.1", wall_s=30, tasks=120)
        stop = threading.Event()
        stop.set()
        started = time.monotonic()
        rooms_peers.await_progress(opts, bench.free_port(), stop)
        self.assertLess(time.monotonic() - started, 5)

    def test_await_progress_gives_up_at_the_deadline(self):
        opts = argparse.Namespace(host_ip="127.0.0.1", wall_s=0, tasks=120)
        rooms_peers.await_progress(opts, bench.free_port(), threading.Event())

    def test_inject_does_nothing_when_the_run_already_ended(self):
        opts = argparse.Namespace(host_ip="127.0.0.1", wall_s=30, tasks=120, rooms="/nonexistent")
        stop = threading.Event()
        stop.set()
        log = []
        rooms_peers.inject(opts, bench.free_port(), "/nonexistent", {"server": None}, log, stop)
        self.assertEqual(log, [])

    def test_finish_injector_is_safe_on_an_unstarted_thread_and_twice(self):
        stop = threading.Event()
        idle = threading.Thread(target=stop.wait)
        rooms_peers.finish_injector(idle, stop)
        worker = threading.Thread(target=stop.wait)
        stop.clear()
        worker.start()
        rooms_peers.finish_injector(worker, stop)
        rooms_peers.finish_injector(worker, stop)
        self.assertFalse(worker.is_alive())


class BundleTest(unittest.TestCase):
    def test_the_bundle_carries_exactly_what_the_guest_imports(self):
        raw = io.BytesIO(base64.b64decode(rooms_peers.bundle()))
        with tarfile.open(fileobj=raw, mode="r:gz") as tar:
            self.assertEqual(sorted(tar.getnames()), ["peer.py", "store.py"])

    def test_the_guest_command_formats(self):
        command = rooms_peers.GUEST.format(bundle="QQ==", host="172.16.0.1", port=6390, tasks=120,
                                           work_ms=200, ttl_ms=2000)
        self.assertIn("--store resp:172.16.0.1:6390", command)
        self.assertIn("--tasks 120 --work-ms 200 --ttl-ms 2000", command)


if __name__ == "__main__":
    unittest.main()
