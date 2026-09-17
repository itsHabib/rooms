"""A minimal threaded RESP2 server: just the commands RespStore sends.

It stands in for Redis/Valkey so the tests run with nothing installed. It is
not Redis. There is no Lua interpreter: EVAL recognises the four scripts in
store.py by their exact text and runs a Python twin of each. Commands run one
at a time under a lock, which mirrors Redis's single command thread.

With --journal PATH every write is appended with its timestamp and replayed on
start, so a killed and restarted server keeps its leases, fence counters and
log, in the way an append-only file does for the real thing.

    python3 fake_resp.py --port 6390 [--journal PATH]
"""

import argparse
import json
import socketserver
import sys
import threading
import time

from store import ACQUIRE_LUA, COMPLETE_LUA, EXTEND_LUA, RELEASE_LUA

WRITES = {"SET", "INCR", "SADD", "XADD", "EVAL"}


class CommandError(Exception):
    """Sent to the client as a RESP error reply."""


class Simple(str):
    """A reply sent as a simple string (+OK) rather than a bulk string."""


def read_command(rfile):
    """One client command as a list of str; None when the client has gone."""
    header = rfile.readline()
    if not header:
        return None
    if not header.startswith(b"*") or not header.endswith(b"\r\n"):
        raise CommandError("protocol error: expected an array of bulk strings")
    return [_read_bulk(rfile) for _ in range(int(header[1:-2]))]


def _read_bulk(rfile):
    header = rfile.readline()
    if not header.startswith(b"$") or not header.endswith(b"\r\n"):
        raise CommandError("protocol error: expected a bulk string")
    size = int(header[1:-2])
    data = rfile.read(size + 2)
    if len(data) != size + 2:
        raise CommandError("protocol error: short bulk string")
    return data[:-2].decode()


def encode_reply(value):
    if isinstance(value, CommandError):
        return b"-ERR %s\r\n" % str(value).encode()
    if isinstance(value, Simple):
        return b"+%s\r\n" % value.encode()
    if value is None:
        return b"$-1\r\n"
    if isinstance(value, int):
        return b":%d\r\n" % value
    if isinstance(value, list):
        return b"*%d\r\n" % len(value) + b"".join(encode_reply(v) for v in value)
    data = str(value).encode()
    return b"$%d\r\n%s\r\n" % (len(data), data)


class Engine:
    """The keyspace. Expiry is lazy: a key past its deadline is dropped on access."""

    def __init__(self, journal_path=None):
        self.data = {}
        self.deadline = {}
        self.last_id = (0, 0)
        self.lock = threading.Lock()
        self.journal = None
        if journal_path:
            self._replay(journal_path)
            self.journal = open(journal_path, "a", encoding="utf-8")

    def execute(self, args, now_ms=None):
        name = args[0].upper() if args else ""
        handler = getattr(self, "cmd_" + name.lower(), None)
        if handler is None:
            raise CommandError("unknown command '%s'" % name)
        with self.lock:
            now = int(time.time() * 1000) if now_ms is None else now_ms
            reply = handler(now, *args[1:])
            if self.journal and now_ms is None and name in WRITES:
                self.journal.write(json.dumps([now, args]) + "\n")
                self.journal.flush()
        return reply

    def close(self):
        if self.journal:
            self.journal.close()

    def _replay(self, path):
        try:
            with open(path, encoding="utf-8") as fh:
                lines = [line for line in fh if line.endswith("\n")]
        except FileNotFoundError:
            return
        for number, line in enumerate(lines, 1):
            self._replay_line(path, number, line)

    def _replay_line(self, path, number, line):
        """A corrupt line is skipped with a note: losing one write beats refusing to start."""
        try:
            now, args = json.loads(line)
            self.execute(args, now)
        except (ValueError, TypeError, CommandError) as err:
            print("%s:%d: skipped corrupt journal line: %s" % (path, number, err), file=sys.stderr)

    def _get(self, now, key, default=None):
        if self.deadline.get(key, now + 1) <= now:
            self.data.pop(key, None)
            self.deadline.pop(key, None)
        return self.data.get(key, default)

    def cmd_ping(self, _now):
        return Simple("PONG")

    def cmd_get(self, now, key):
        return self._get(now, key)

    def cmd_set(self, now, key, value, *options):
        flags = [o.upper() for o in options]
        if "NX" in flags and self._get(now, key) is not None:
            return None
        self.data[key] = value
        self.deadline.pop(key, None)
        if "PX" in flags:
            self.deadline[key] = now + int(options[flags.index("PX") + 1])
        return Simple("OK")

    def cmd_del(self, now, key):
        existed = self._get(now, key) is not None
        self.data.pop(key, None)
        self.deadline.pop(key, None)
        return int(existed)

    def cmd_pexpire(self, now, key, ttl_ms):
        if self._get(now, key) is None:
            return 0
        self.deadline[key] = now + int(ttl_ms)
        return 1

    def cmd_incr(self, now, key):
        value = int(self._get(now, key, "0")) + 1
        self.data[key] = str(value)
        return value

    def cmd_sadd(self, now, key, *members):
        current = self.data.setdefault(key, set())
        added = set(members) - current
        current.update(added)
        return len(added)

    def cmd_smembers(self, now, key):
        return sorted(self._get(now, key, set()))

    def cmd_hsetnx(self, now, key, field, value):
        fields = self.data.setdefault(key, {})
        if field in fields:
            return 0
        fields[field] = value
        return 1

    def cmd_hkeys(self, now, key):
        return list(self._get(now, key, {}))

    def cmd_xadd(self, now, key, entry_id, *fields):
        if entry_id != "*" or not fields or len(fields) % 2:
            raise CommandError("XADD supports only '*' ids with field/value pairs")
        last_ms, last_seq = self.last_id
        self.last_id = (last_ms, last_seq + 1) if now <= last_ms else (now, 0)
        entry_id = "%d-%d" % self.last_id
        self.data.setdefault(key, []).append([entry_id, list(fields)])
        return entry_id

    def cmd_xrange(self, now, key, start, end):
        if (start, end) != ("-", "+"):
            raise CommandError("XRANGE supports only the full range - +")
        return [list(entry) for entry in self._get(now, key, [])]

    def cmd_eval(self, now, script, numkeys, *rest):
        twin = SCRIPTS.get(script)
        if twin is None:
            raise CommandError("unknown script: this server only knows store.py's scripts")
        count = int(numkeys)
        return twin(self, now, rest[:count], rest[count:])


def _acquire(engine, now, keys, argv):
    lease, fence, log = keys
    task, holder, ttl_ms = argv
    if engine.cmd_set(now, lease, holder, "NX", "PX", ttl_ms) is None:
        return None
    token = engine.cmd_incr(now, fence)
    engine.cmd_xadd(now, log, "*", "kind", "grant", "task", task, "holder", holder,
                    "token", str(token))
    return token


def _extend(engine, now, keys, argv):
    lease, fence = keys
    holder, token, ttl_ms = argv
    if engine.cmd_get(now, lease) != holder or engine.cmd_get(now, fence) != token:
        return 0
    return engine.cmd_pexpire(now, lease, ttl_ms)


def _release(engine, now, keys, argv):
    lease, fence = keys
    holder, token = argv
    if engine.cmd_get(now, lease) != holder or engine.cmd_get(now, fence) != token:
        return 0
    return engine.cmd_del(now, lease)


def _complete(engine, now, keys, argv):
    lease, fence, done, log = keys
    task, holder, token, result = argv
    accepted = int(engine.cmd_get(now, fence) == token
                   and engine.cmd_hsetnx(now, done, task, token) == 1)
    if accepted:
        engine.cmd_set(now, lease, "~done~")
    engine.cmd_xadd(now, log, "*", "kind", "complete", "task", task, "holder", holder,
                    "token", token, "result", result, "accepted", str(accepted))
    return accepted


SCRIPTS = {ACQUIRE_LUA: _acquire, EXTEND_LUA: _extend, RELEASE_LUA: _release,
           COMPLETE_LUA: _complete}


class _Handler(socketserver.StreamRequestHandler):
    def handle(self):
        try:
            while self._serve_one():
                pass
        except OSError:
            return

    def _serve_one(self):
        """Answer one command; False ends the connection (client gone or stream unparseable)."""
        try:
            args = read_command(self.rfile)
        except (CommandError, ValueError) as err:
            self.wfile.write(encode_reply(CommandError(str(err))))
            return False
        if args is None:
            return False
        self.wfile.write(encode_reply(self._run(args)))
        return True

    def _run(self, args):
        try:
            return self.server.engine.execute(args)
        except CommandError as err:
            return err
        except (TypeError, ValueError) as err:
            return CommandError("bad arguments: %s" % err)


class FakeRespServer(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True
    request_queue_size = 256

    def __init__(self, host="127.0.0.1", port=0, journal_path=None):
        super().__init__((host, port), _Handler)
        self.engine = Engine(journal_path)
        self.port = self.server_address[1]

    def start(self):
        threading.Thread(target=self.serve_forever, args=(0.02,), daemon=True).start()
        return self

    def stop(self):
        self.shutdown()
        self.server_close()
        self.engine.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--journal")
    opts = parser.parse_args()
    FakeRespServer(opts.host, opts.port, opts.journal).serve_forever()


if __name__ == "__main__":
    main()
