"""Process-mode benchmark: P peer processes against each backend, then seeded fault runs.

    python3 bench.py --out NEW_DIR [--peers 4,16,64] [--tasks 200] [--fault-seeds 5]

Backends: `file` (shared directory), `fake` (fake_resp.py over TCP) and `real`
(valkey-server or redis-server, only when one is on PATH). The output directory
must not exist. It receives summary.json and, per run, peers.ndjson (what each
peer saw), history.ndjson (the store's grants and completions) and, for fault
runs, schedule.json.
"""

import argparse
import json
import os
import platform
import shutil
import signal
import socket
import subprocess
import sys
import time

import faults
from peer import task_ids
from store import FileStore, RespStore

HERE = os.path.dirname(os.path.abspath(__file__))


def real_server():
    return shutil.which("valkey-server") or shutil.which("redis-server")


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def server_command(backend, port, scratch):
    """Both servers persist every write, so a store kill loses no accepted state."""
    if backend == "fake":
        return [sys.executable, os.path.join(HERE, "fake_resp.py"), "--port", str(port),
                "--journal", os.path.join(scratch, "journal.ndjson")]
    return [real_server(), "--port", str(port), "--bind", "127.0.0.1", "--dir", scratch,
            "--save", "", "--appendonly", "yes", "--appendfsync", "always"]


class Swarm:
    """One store plus P peer processes, with the handles a fault schedule needs."""

    def __init__(self, backend, scratch, peers, tasks, peer_flags):
        self.backend = backend
        self.scratch = scratch
        self.tasks = tasks
        self.peer_flags = peer_flags
        self.procs = [None] * peers
        self.server = None
        self.root = os.path.join(scratch, "store")
        self.port = free_port()
        os.makedirs(os.path.join(scratch, "peers"))

    def store(self):
        if self.backend == "file":
            return FileStore(self.root)
        return RespStore("127.0.0.1", self.port)

    def start(self):
        self.store_restart()
        store = self.store()
        store.seed(task_ids(self.tasks))
        store.close()
        for index in range(len(self.procs)):
            self.restart({"peer": index})

    def store_restart(self, _event=None):
        if self.backend == "file":
            os.makedirs(self.root, exist_ok=True)
            os.chmod(self.root, 0o755)
            return
        if self.server and self.server.poll() is None:
            return
        self.server = subprocess.Popen(server_command(self.backend, self.port, self.scratch),
                                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        self._await_server()

    def _await_server(self):
        probe = RespStore("127.0.0.1", self.port)
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            try:
                probe.call("PING")
                return probe.close()
            except OSError:
                time.sleep(0.02)
        raise RuntimeError("%s server did not come up on port %d" % (self.backend, self.port))

    def store_kill(self, _event=None):
        """A directory has no process to kill; revoking access is the nearest outage."""
        if self.backend == "file":
            os.chmod(self.root, 0)
            return
        self.server.kill()
        self.server.wait()

    def restart(self, event):
        index = event["peer"]
        if self.procs[index] and self.procs[index].poll() is None:
            return
        spec = "file:" + self.root if self.backend == "file" else "resp:127.0.0.1:%d" % self.port
        name = "peer-%02d" % index
        command = [sys.executable, os.path.join(HERE, "peer.py"), "--store", spec,
                   "--peer-id", name, "--tasks", str(self.tasks),
                   "--log", os.path.join(self.scratch, "peers", name + ".ndjson"),
                   "--clock-offset-file", self._offset_path(index)] + self.peer_flags
        self.procs[index] = subprocess.Popen(command, stdout=subprocess.DEVNULL)

    def _offset_path(self, index):
        return os.path.join(self.scratch, "peers", "clock-%02d" % index)

    def _signal(self, event, signum):
        proc = self.procs[event["peer"]]
        if proc and proc.poll() is None:
            proc.send_signal(signum)

    def pause(self, event):
        self._signal(event, signal.SIGSTOP)

    def resume(self, event):
        self._signal(event, signal.SIGCONT)

    def kill(self, event):
        self._signal(event, signal.SIGKILL)
        if self.procs[event["peer"]]:
            self.procs[event["peer"]].wait()

    def skew(self, event):
        temp = self._offset_path(event["peer"]) + ".tmp"
        with open(temp, "w", encoding="utf-8") as fh:
            fh.write(str(event["offset_ms"]))
        os.replace(temp, self._offset_path(event["peer"]))

    def wait(self, timeout_s):
        """True if every peer exited by itself; stragglers are killed."""
        deadline = time.monotonic() + timeout_s
        while time.monotonic() < deadline and self._running():
            time.sleep(0.02)
        finished = not self._running()
        for index in range(len(self.procs)):
            self.kill({"peer": index})
        return finished

    def _running(self):
        return [proc for proc in self.procs if proc and proc.poll() is None]

    def stop(self):
        if self.server:
            self.store_kill()
        if os.path.isdir(self.root):
            os.chmod(self.root, 0o755)

    def peer_events(self):
        events = []
        folder = os.path.join(self.scratch, "peers")
        for name in sorted(n for n in os.listdir(folder) if n.endswith(".ndjson")):
            with open(os.path.join(folder, name), encoding="utf-8") as fh:
                events.extend(json.loads(line) for line in fh if line.endswith("\n"))
        return events


def percentiles(values):
    ordered = sorted(values)
    if not ordered:
        return {"n": 0}
    def rank(q):
        return round(ordered[min(len(ordered) - 1, int(q * len(ordered)))], 3)
    return {"n": len(ordered), "p50": rank(0.50), "p95": rank(0.95), "max": round(ordered[-1], 3)}


def write_ndjson(path, records):
    with open(path, "w", encoding="utf-8") as fh:
        fh.writelines(json.dumps(record, sort_keys=True) + "\n" for record in records)


def run_once(out, name, backend, peers, tasks, peer_flags, events=None, timeout_s=120):
    """Run one swarm to completion (replaying `events` if given) and summarise it."""
    run_dir = os.path.join(out, name)
    scratch = os.path.join(run_dir, "scratch")
    os.makedirs(scratch)
    swarm = Swarm(backend, scratch, peers, tasks, peer_flags)
    started = time.monotonic()
    try:
        swarm.start()
        faults.replay(events or [], swarm)
        finished = swarm.wait(timeout_s)
        wall_s = time.monotonic() - started
        live = sum(1 for proc in swarm.procs if proc.returncode == 0) if finished else peers
        store = swarm.store()
        history = store.history()
        store.close()
    finally:
        swarm.wait(0)
        swarm.stop()
    seen = swarm.peer_events()
    write_ndjson(os.path.join(run_dir, "peers.ndjson"), seen)
    write_ndjson(os.path.join(run_dir, "history.ndjson"), history)
    shutil.rmtree(scratch)
    verdict = faults.check(task_ids(tasks), history, live)
    claims = [e for e in seen if e["ev"] == "claim"]
    exits = [e for e in seen if e["ev"] == "exit"]
    return {
        "name": name, "backend": backend, "peers": peers, "tasks": tasks,
        "finished": finished, "wall_s": round(wall_s, 3),
        "throughput_tasks_per_s": round(verdict["accepted"] / wall_s, 2),
        "claim_ms": percentiles([e["claim_ms"] for e in claims]),
        "acquire_ms": percentiles([e["acquire_ms"] for e in claims]),
        "claim_attempts_rejected": sum(e["misses"] for e in claims + exits),
        "lost_leases": sum(1 for e in seen if e["ev"] == "lost"),
        "store_retries": sum(1 for e in seen if e["ev"] == "retry"),
        "peers_gave_up": sum(1 for e in exits if e["reason"] != "no pending tasks"),
        "invariants": verdict,
    }


def fault_run(out, backend, seed, opts):
    events = faults.schedule(seed, opts.fault_peers, opts.fault_ttl_ms)
    name = "fault-%s-seed%d" % (backend, seed)
    flags = ["--ttl-ms", str(opts.fault_ttl_ms), "--work-ms", str(opts.fault_work_ms),
             "--reckless"]
    result = run_once(out, name, backend, opts.fault_peers, opts.fault_tasks, flags, events, 60)
    with open(os.path.join(out, name, "schedule.json"), "wb") as fh:
        fh.write(faults.dumps(events))
    return dict(result, seed=seed, schedule_events=len(events))


def parse_args(argv):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--out", required=True, help="output directory; must not exist")
    parser.add_argument("--peers", default="4,16,64")
    parser.add_argument("--tasks", type=int, default=200)
    parser.add_argument("--work-ms", type=int, default=20)
    parser.add_argument("--ttl-ms", type=int, default=2000)
    parser.add_argument("--backends", default="file,fake,real")
    parser.add_argument("--fault-seeds", type=int, default=5, help="run seeds 1..K")
    parser.add_argument("--fault-peers", type=int, default=8)
    parser.add_argument("--fault-tasks", type=int, default=48)
    parser.add_argument("--fault-ttl-ms", type=int, default=400)
    parser.add_argument("--fault-work-ms", type=int, default=500,
                        help="longer than the ttl, so a paused holder is overtaken mid-task")
    return parser.parse_args(argv)


def main(argv=None):
    opts = parse_args(argv)
    if os.path.exists(opts.out):
        print("refusing to reuse output directory %s" % opts.out, file=sys.stderr)
        return 2
    os.makedirs(opts.out)
    backends = [b for b in opts.backends.split(",") if b != "real" or real_server()]
    flags = ["--ttl-ms", str(opts.ttl_ms), "--work-ms", str(opts.work_ms)]
    runs, fault_runs = [], []
    for backend in backends:
        for peers in (int(p) for p in opts.peers.split(",")):
            name = "bench-%s-p%d" % (backend, peers)
            runs.append(run_once(opts.out, name, backend, peers, opts.tasks, flags))
            print(json.dumps(runs[-1], sort_keys=True))
        for seed in range(1, opts.fault_seeds + 1):
            fault_runs.append(fault_run(opts.out, backend, seed, opts))
            print(json.dumps(fault_runs[-1], sort_keys=True))
    summary = {
        "machine": {"platform": platform.platform(), "machine": platform.machine(),
                    "cpus": os.cpu_count(), "python": platform.python_version(),
                    "real_server": real_server()},
        "options": vars(opts), "runs": runs, "fault_runs": fault_runs,
    }
    with open(os.path.join(opts.out, "summary.json"), "w", encoding="utf-8") as fh:
        json.dump(summary, fh, indent=2, sort_keys=True)
    bad = [r["name"] for r in runs + fault_runs if not r["invariants"]["ok"]]
    print("invariant failures: %s" % (", ".join(bad) or "none"))
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
