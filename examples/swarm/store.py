"""Lease and receipt stores for a swarm of peers: a shared directory or Redis/Valkey.

Both backends expose the same surface:

    seed(tasks)                              register task ids (idempotent)
    tasks() / pending()                      all task ids / ids with no accepted completion
    acquire(task, holder, ttl_ms)            fencing token, or None if held or done
    extend(task, holder, token, ttl_ms)      True while the caller still owns the lease
    release(task, holder, token)             True if the caller's lease was dropped
    complete(task, holder, token, result)    True if accepted; a stale token is rejected
    history() / receipts()                   ordered grant + completion events / completions

Fencing tokens increase per task. A completion is accepted only when it carries
the newest token granted for that task and the task is not already complete.
Backend failures raise OSError so callers need a single retry path.
"""

import json
import os
import socket
import tempfile
import time


class StoreError(OSError):
    """The store answered with an error, or the connection to it broke."""


def wall_ms():
    return int(time.time() * 1000)


class Store:
    def close(self):
        """Nothing to release by default."""

    def receipts(self):
        return [e for e in self.history() if e["kind"] == "complete"]

    def pending(self):
        return sorted(set(self.tasks()) - self._done())


class FileStore(Store):
    """Coordination on a shared directory.

    Each task owns a directory of numbered slot files. Slot n is either the
    lease grant carrying token n, or the completion of token n - 1. A slot is
    created by writing a temp file and hard-linking it into place: the link
    fails if the slot exists, so exactly one writer wins each slot and the
    content is never seen half-written. Completing with token n means winning
    slot n + 1, which fails once anyone has been granted token n + 1; that is
    the fencing check. Expiry is judged by the caller's clock, so clock skew
    between peers matters here in a way it does not for a networked store.
    """

    def __init__(self, root, clock=wall_ms):
        self.root = str(root)
        self.clock = clock

    def seed(self, tasks):
        for name in ("tmp", "done", "tasks"):
            os.makedirs(os.path.join(self.root, name), exist_ok=True)
        for task in tasks:
            os.makedirs(self._task_dir(task), exist_ok=True)

    def tasks(self):
        return sorted(os.listdir(os.path.join(self.root, "tasks")))

    def acquire(self, task, holder, ttl_ms):
        slots = self._slots(task)
        last = self._read(task, slots[-1]) if slots else None
        if last and last["kind"] == "complete":
            self._mark_done(task)
            return None
        if last and not last.get("released") and last["expiry"] > self.clock():
            return None
        token = (slots[-1] if slots else 0) + 1
        grant = {"kind": "grant", "task": task, "holder": holder, "token": token,
                 "expiry": self.clock() + ttl_ms}
        return token if self._link(task, token, grant) else None

    def extend(self, task, holder, token, ttl_ms):
        return self._rewrite(task, holder, token, {"expiry": self.clock() + ttl_ms})

    def release(self, task, holder, token):
        return self._rewrite(task, holder, token, {"released": True})

    def complete(self, task, holder, token, result):
        receipt = {"kind": "complete", "task": task, "holder": holder, "token": token,
                   "result": result, "accepted": False}
        accepted = self._owns(task, holder, token) and self._link(
            task, token + 1, dict(receipt, accepted=True))
        receipt["accepted"] = accepted
        line = (json.dumps(receipt, sort_keys=True) + "\n").encode()
        fd = os.open(os.path.join(self.root, "receipts.ndjson"),
                     os.O_WRONLY | os.O_APPEND | os.O_CREAT, 0o644)
        try:
            os.write(fd, line)
        finally:
            os.close(fd)
        if accepted:
            self._mark_done(task)
        return accepted

    def history(self):
        """Slot files are the authority for grants and accepted completions; a
        peer killed between winning a slot and appending its receipt line loses
        only the line. Rejected completions exist only in the receipt log."""
        events = [self._read(task, n) for task in self.tasks() for n in self._slots(task)]
        return [e for e in events if e] + [r for r in self._receipt_lines() if not r["accepted"]]

    def _done(self):
        return set(os.listdir(os.path.join(self.root, "done")))

    def _receipt_lines(self):
        try:
            with open(os.path.join(self.root, "receipts.ndjson"), encoding="utf-8") as fh:
                return [json.loads(line) for line in fh if line.endswith("\n")]
        except FileNotFoundError:
            return []

    def _task_dir(self, task):
        return os.path.join(self.root, "tasks", task)

    def _slot_path(self, task, n):
        return os.path.join(self._task_dir(task), "%08d.json" % n)

    def _slots(self, task):
        return sorted(int(name[:8]) for name in os.listdir(self._task_dir(task)))

    def _read(self, task, n):
        try:
            with open(self._slot_path(task, n), encoding="utf-8") as fh:
                return json.load(fh)
        except FileNotFoundError:
            return None

    def _owns(self, task, holder, token):
        slot = self._read(task, token)
        return bool(slot) and slot["kind"] == "grant" and slot["holder"] == holder

    def _temp(self, record):
        fd, path = tempfile.mkstemp(dir=os.path.join(self.root, "tmp"))
        with os.fdopen(fd, "w", encoding="utf-8") as fh:
            json.dump(record, fh, sort_keys=True)
        return path

    def _link(self, task, n, record):
        temp = self._temp(record)
        try:
            os.link(temp, self._slot_path(task, n))
            return True
        except FileExistsError:
            return False
        finally:
            os.unlink(temp)

    def _rewrite(self, task, holder, token, change):
        """Only the holder rewrites its own grant slot, so a rename is enough.
        A takeover can still land between the checks; fencing covers that gap."""
        slot = self._read(task, token)
        if not self._owns(task, holder, token) or slot.get("released"):
            return False
        if self._slots(task)[-1] != token:
            return False
        os.replace(self._temp(dict(slot, **change)), self._slot_path(task, token))
        return self._slots(task)[-1] == token

    def _mark_done(self, task):
        """A hint so pending() is one listdir; the completion slot stays the authority."""
        with open(os.path.join(self.root, "done", task), "w", encoding="utf-8"):
            pass


# Lua run by the server, so each check and its write are one atomic step.
# KEYS: lease, fence.  ARGV: holder, token, ttl_ms.
EXTEND_LUA = """
if redis.call('GET', KEYS[1]) == ARGV[1] and redis.call('GET', KEYS[2]) == ARGV[2] then
  return redis.call('PEXPIRE', KEYS[1], ARGV[3])
end
return 0
"""

# KEYS: lease, fence.  ARGV: holder, token.
RELEASE_LUA = """
if redis.call('GET', KEYS[1]) == ARGV[1] and redis.call('GET', KEYS[2]) == ARGV[2] then
  return redis.call('DEL', KEYS[1])
end
return 0
"""

# On acceptance the lease key becomes a tombstone with no expiry, so SET NX can
# never grant a finished task again, however stale the caller's view of pending.
# KEYS: lease, fence, done, log.  ARGV: task, holder, token, result.
COMPLETE_LUA = """
local ok = 0
if redis.call('GET', KEYS[2]) == ARGV[3] and redis.call('HSETNX', KEYS[3], ARGV[1], ARGV[3]) == 1 then
  ok = 1
  redis.call('SET', KEYS[1], '~done~')
end
redis.call('XADD', KEYS[4], '*', 'kind', 'complete', 'task', ARGV[1], 'holder', ARGV[2],
           'token', ARGV[3], 'result', ARGV[4], 'accepted', ok)
return ok
"""


def encode_command(args):
    parts = [b"*%d\r\n" % len(args)]
    for arg in args:
        data = arg if isinstance(arg, bytes) else str(arg).encode()
        parts.append(b"$%d\r\n%s\r\n" % (len(data), data))
    return b"".join(parts)


def read_reply(rfile):
    """Parse one RESP2 reply: str, int, None, a list, or StoreError for '-'."""
    line = rfile.readline()
    if not line.endswith(b"\r\n"):
        raise StoreError("connection closed mid-reply")
    kind, rest = line[:1], line[1:-2]
    if kind == b"+":
        return rest.decode()
    if kind == b"-":
        raise StoreError(rest.decode())
    if kind == b":":
        return int(rest)
    if kind == b"*":
        return None if rest == b"-1" else [read_reply(rfile) for _ in range(int(rest))]
    if kind != b"$":
        raise StoreError("unexpected reply type %r" % kind)
    if rest == b"-1":
        return None
    data = rfile.read(int(rest) + 2)
    if len(data) != int(rest) + 2:
        raise StoreError("connection closed mid-reply")
    return data[:-2].decode()


class RespStore(Store):
    """Coordination through Redis or Valkey, speaking RESP2 over one TCP socket.

    Grants and completions share one stream (XADD) rather than a list: stream
    ids give the checker a server-side total order, and completions are
    appended inside the same script that accepts or rejects them.

    acquire is SET NX PX followed by INCR, which are two steps. A peer stalled
    between them for longer than the ttl ends up with the newest token but not
    the lease; extend and release then fail for both parties and the lease
    lapses. Safety holds because complete compares against the fence counter.
    """

    def __init__(self, host, port, prefix="swarm", timeout_s=2.0):
        self.addr = (host, int(port))
        self.prefix = prefix
        self.timeout_s = timeout_s
        self._sock = None
        self._rfile = None

    def call(self, *args):
        try:
            if self._sock is None:
                self._sock = socket.create_connection(self.addr, timeout=self.timeout_s)
                self._rfile = self._sock.makefile("rb")
            self._sock.sendall(encode_command(args))
            return read_reply(self._rfile)
        except OSError:
            self.close()
            raise

    def close(self):
        if self._sock is None:
            return
        self._rfile.close()
        self._sock.close()
        self._sock = None

    def seed(self, tasks):
        if tasks:
            self.call("SADD", self._key("tasks"), *tasks)

    def tasks(self):
        return sorted(self.call("SMEMBERS", self._key("tasks")))

    def acquire(self, task, holder, ttl_ms):
        if self.call("SET", self._key("lease", task), holder, "NX", "PX", ttl_ms) is None:
            return None
        token = self.call("INCR", self._key("fence", task))
        self.call("XADD", self._key("log"), "*", "kind", "grant", "task", task,
                  "holder", holder, "token", token)
        return token

    def extend(self, task, holder, token, ttl_ms):
        keys = (self._key("lease", task), self._key("fence", task))
        return self.call("EVAL", EXTEND_LUA, 2, *keys, holder, token, ttl_ms) == 1

    def release(self, task, holder, token):
        keys = (self._key("lease", task), self._key("fence", task))
        return self.call("EVAL", RELEASE_LUA, 2, *keys, holder, token) == 1

    def complete(self, task, holder, token, result):
        keys = (self._key("lease", task), self._key("fence", task),
                self._key("done"), self._key("log"))
        return self.call("EVAL", COMPLETE_LUA, 4, *keys, task, holder, token, result) == 1

    def history(self):
        entries = self.call("XRANGE", self._key("log"), "-", "+")
        return [self._event(fields) for _id, fields in entries]

    def _done(self):
        return set(self.call("HKEYS", self._key("done")))

    def _key(self, *parts):
        return ":".join((self.prefix,) + parts)

    @staticmethod
    def _event(fields):
        event = dict(zip(fields[::2], fields[1::2]))
        event["token"] = int(event["token"])
        if "accepted" in event:
            event["accepted"] = event["accepted"] == "1"
        return event


def open_store(spec, clock=wall_ms, prefix="swarm"):
    """file:DIR or resp:HOST:PORT."""
    kind, _, rest = spec.partition(":")
    if kind == "file" and rest:
        return FileStore(rest, clock)
    host, _, port = rest.rpartition(":")
    if kind == "resp" and host and port.isdigit():
        return RespStore(host, int(port), prefix)
    raise ValueError("store must be file:DIR or resp:HOST:PORT, got %r" % spec)
