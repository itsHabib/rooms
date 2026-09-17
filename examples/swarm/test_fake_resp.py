"""The fake server's wire protocol and the few Redis semantics the store leans on."""

import io
import os
import socket
import tempfile
import unittest

from fake_resp import CommandError, Engine, FakeRespServer, Simple, encode_reply, read_command
from store import COMPLETE_LUA, StoreError, encode_command, read_reply


class CommandParsingTest(unittest.TestCase):
    def test_reads_an_array_of_bulk_strings(self):
        wire = io.BytesIO(b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$5\r\nv\r\nv!\r\n")
        self.assertEqual(read_command(wire), ["SET", "k", "v\r\nv!"])

    def test_reads_back_to_back_commands_then_end_of_stream(self):
        wire = io.BytesIO(encode_command(["PING"]) + encode_command(["INCR", "n"]))
        self.assertEqual(read_command(wire), ["PING"])
        self.assertEqual(read_command(wire), ["INCR", "n"])
        self.assertIsNone(read_command(wire))

    def test_client_encoding_handles_ints_and_utf8(self):
        self.assertEqual(encode_command(["SET", "k", 12]),
                         b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$2\r\n12\r\n")
        wire = io.BytesIO(encode_command(["SET", "k", "hé"]))
        self.assertEqual(read_command(wire), ["SET", "k", "hé"])

    def test_rejects_malformed_input(self):
        for wire in (b"PING\r\n", b"*1\r\n+PING\r\n", b"*1\r\n$4\r\nPI", b"*1\r\n$4\r\nPING"):
            with self.assertRaises(CommandError, msg=wire):
                read_command(io.BytesIO(wire))


class ReplyEncodingTest(unittest.TestCase):
    def test_round_trips_every_reply_shape(self):
        for value in (Simple("OK"), "bulk", 7, None, [], ["1-0", ["kind", "grant"]], [1, None]):
            self.assertEqual(read_reply(io.BytesIO(encode_reply(value))), value)

    def test_wire_forms(self):
        self.assertEqual(encode_reply(Simple("OK")), b"+OK\r\n")
        self.assertEqual(encode_reply(None), b"$-1\r\n")
        self.assertEqual(encode_reply(["a", 1]), b"*2\r\n$1\r\na\r\n:1\r\n")

    def test_errors_and_truncation_raise_store_error(self):
        for wire in (encode_reply(CommandError("nope")), b"$5\r\nab", b"", b"?what\r\n"):
            with self.assertRaises(StoreError, msg=wire):
                read_reply(io.BytesIO(wire))


class EngineTest(unittest.TestCase):
    def setUp(self):
        self.engine = Engine()

    def test_set_nx_px_expires_on_the_engine_clock(self):
        self.assertEqual(self.engine.execute(["SET", "k", "a", "NX", "PX", "100"], 1000), "OK")
        self.assertIsNone(self.engine.execute(["SET", "k", "b", "NX", "PX", "100"], 1099))
        self.assertEqual(self.engine.execute(["GET", "k"], 1099), "a")
        self.assertEqual(self.engine.execute(["SET", "k", "b", "NX", "PX", "100"], 1100), "OK")

    def test_incr_counts_from_one(self):
        self.assertEqual([self.engine.execute(["INCR", "n"]) for _ in range(3)], [1, 2, 3])

    def test_stream_ids_increase_even_within_one_millisecond(self):
        ids = [self.engine.execute(["XADD", "s", "*", "k", "v"], now) for now in (5, 5, 4, 6)]
        self.assertEqual(ids, ["5-0", "5-1", "5-2", "6-0"])
        self.assertEqual(self.engine.execute(["XRANGE", "s", "-", "+"])[0], ["5-0", ["k", "v"]])

    def test_unknown_commands_and_scripts_are_errors(self):
        self.assertRaises(CommandError, self.engine.execute, ["FLUSHALL"])
        self.assertRaises(CommandError, self.engine.execute, ["EVAL", "return 1", "0"])

    def test_scripts_are_matched_by_exact_text(self):
        self.engine.execute(["INCR", "fence"])
        args = ["4", "lease", "fence", "done", "log", "t", "p1", "1", "ok"]
        self.assertEqual(self.engine.execute(["EVAL", COMPLETE_LUA] + args), 1)
        self.assertRaises(CommandError, self.engine.execute, ["EVAL", COMPLETE_LUA + " "] + args)

    def test_journal_replay_rebuilds_state_with_original_timestamps(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "journal")
            first = Engine(path)
            first.execute(["SET", "lease", "p1", "NX", "PX", "60000"])
            first.execute(["INCR", "fence"])
            entry = first.execute(["XADD", "log", "*", "kind", "grant"])
            first.close()
            second = Engine(path)
            self.addCleanup(second.close)
            self.assertEqual(second.execute(["GET", "lease"]), "p1")
            self.assertEqual(second.execute(["INCR", "fence"]), 2)
            self.assertEqual(second.execute(["XRANGE", "log", "-", "+"]),
                             [[entry, ["kind", "grant"]]])


class ServerTest(unittest.TestCase):
    def test_a_bad_command_gets_an_error_reply_and_the_connection_survives(self):
        server = FakeRespServer().start()
        self.addCleanup(server.stop)
        with socket.create_connection(("127.0.0.1", server.port), timeout=2) as sock:
            sock.sendall(encode_command(["NOPE"]) + encode_command(["INCR"])
                         + encode_command(["PING"]))
            replies = sock.makefile("rb")
            self.assertRaisesRegex(StoreError, "unknown command", read_reply, replies)
            self.assertRaisesRegex(StoreError, "bad arguments", read_reply, replies)
            self.assertEqual(read_reply(replies), "PONG")
            replies.close()

    def test_an_unparseable_stream_gets_an_error_and_is_closed(self):
        server = FakeRespServer().start()
        self.addCleanup(server.stop)
        with socket.create_connection(("127.0.0.1", server.port), timeout=2) as sock:
            sock.sendall(b"GET / HTTP/1.1\r\n\r\n")
            replies = sock.makefile("rb")
            self.assertRaisesRegex(StoreError, "protocol error", read_reply, replies)
            self.assertEqual(replies.read(), b"")
            replies.close()


if __name__ == "__main__":
    unittest.main()
