"""Run a bounded real-KVM batch; imports the retained phase harness next to it."""
import concurrent.futures
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import threading
import time

spec = importlib.util.spec_from_file_location('phases', Path(__file__).with_name('remote.py'))
r = importlib.util.module_from_spec(spec)
spec.loader.exec_module(r)
stop = threading.Event()


def sample_memory():
    with (r.LAB / 'memory.ndjson').open('x') as log:
        while not stop.is_set():
            pss = 0
            pids = []
            for proc in Path('/proc').glob('[0-9]*'):
                try:
                    if proc.joinpath('comm').read_text().strip() != 'firecracker':
                        continue
                    rows = proc.joinpath('smaps_rollup').read_text().splitlines()
                    pss += sum(int(line.split()[1]) for line in rows if line.startswith('Pss:'))
                    pids.append(int(proc.name))
                except (OSError, ValueError):
                    continue
            info = dict(line.split(':', 1) for line in Path('/proc/meminfo').read_text().splitlines())
            log.write(json.dumps({'unix': time.time(), 'firecracker_pids': pids,
                                 'summed_pss_kib': pss,
                                 'mem_available_kib': int(info['MemAvailable'].split()[0]),
                                 'cached_kib': int(info['Cached'].split()[0])}) + '\n')
            log.flush()
            stop.wait(.5)


def check(name, count=1):
    record = json.loads((r.LAB / name / 'summary.json').read_text())
    if record['cli_exit'] != 0:
        raise RuntimeError(f'{name}: CLI exit {record["cli_exit"]}; inspect preserved evidence')
    outputs = [r.LAB / name / 'out']
    if count > 1:
        outputs = [p for p in outputs[0].iterdir() if p.is_dir()]
        if len(outputs) != count:
            raise RuntimeError(f'{name}: expected {count} outputs, found {len(outputs)}')
    identities = []
    for out in outputs:
        value = r.collected(out)
        if not value['patch_matches_input'] or value['result']['exit_code'] != 0 or value['result']['status'] != 'succeeded':
            raise RuntimeError(f'{name}: incomplete or non-equivalent result')
        if value['identity']:
            identities.append(value['identity'])
    if len(set(identities)) != len(identities):
        raise RuntimeError(f'{name}: duplicate identity')
    return {'run': name, 'count': count, 'elapsed_seconds': record['elapsed_seconds'],
            'completed_per_minute': 60 * count / record['elapsed_seconds']}


def main():
    r.phase_setup()
    thread = threading.Thread(target=sample_memory)
    thread.start()
    rows = []
    try:
        for i in range(3):
            name = f'cold-{i}'
            r.phase_cold(name)
            rows.append(check(name))
        r.phase_base('base')
        base = json.loads((r.LAB / 'base/summary.json').read_text())
        snapshot = base['snapshot']['result']['directory']
        for i in range(3):
            name = f'restore-{i}'
            r.phase_restore(name, snapshot, '--toolstore', str(r.STORE))
            rows.append(check(name))
        for count in [2, 4, 8]:
            for repeat in range(2):
                name = f'clone-{count}-{repeat}'
                r.phase_clone(name, snapshot, str(count))
                rows.append(check(name, count))
        (r.LAB / 'comparison.json').write_text(json.dumps(rows, indent=2))
        r.phase_audit()
    finally:
        (r.LAB / 'partial-comparison.json').write_text(json.dumps(rows, indent=2))
        stop.set()
        thread.join()
        (r.LAB / 'final-host.json').write_text(json.dumps(r.host_state(), indent=2))


if __name__ == '__main__':
    main()
