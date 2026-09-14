#!/usr/bin/env python3
"""Summarize completed Rooms experiment artifacts without running workloads."""
import argparse
from datetime import datetime
import hashlib
import json
import math
from pathlib import Path
import statistics
import sys


def number(value, name, minimum=0):
    if type(value) not in (int, float) or not math.isfinite(value) or value < minimum:
        raise ValueError(f'{name}: expected finite number >= {minimum}')
    return value


def read_json(path):
    return json.loads(path.read_text())


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def inspect_attempt(directory):
    """Read collector exit separately from guest exit; preserve missing evidence."""
    summary_path = directory / 'summary.json'
    result_path = directory / 'out/result.json'
    lifecycle_path = directory / 'lifecycle.ndjson'
    summary = read_json(summary_path)
    elapsed = number(summary['elapsed_seconds'], 'elapsed_seconds', 1e-12)
    problems = []
    hashes = {'summary.json': digest(summary_path)}
    result = {}
    events = []
    for path in (result_path, lifecycle_path):
        if not path.is_file():
            problems.append(f'missing {path.relative_to(directory)}')
            continue
        hashes[str(path.relative_to(directory))] = digest(path)
    if result_path.is_file():
        result = read_json(result_path)
    if lifecycle_path.is_file():
        events = [json.loads(line) for line in lifecycle_path.read_text().splitlines() if line.strip()]
    if type(summary.get('cli_exit')) is not int or summary['cli_exit'] != 0:
        problems.append('collector CLI did not succeed')
    if (type(result.get('schema_version')) is not int or result['schema_version'] != 1
            or result.get('status') != 'succeeded'
            or type(result.get('exit_code')) is not int or result['exit_code'] != 0):
        problems.append('guest result did not succeed')
    names = [event['event'] for event in events]
    if names.count('collection_done') != 1 or names.count('cleanup_done') != 1:
        problems.append('collection/cleanup completion missing or duplicated')
    if not names or names[-1] != 'cleanup_done':
        problems.append('cleanup is not final')
    if any(name.endswith('_failed') for name in names):
        problems.append('lifecycle records a failure')
    if len({event['room_id'] for event in events}) != 1:
        problems.append('lifecycle room identity missing or mixed')
    times = [datetime.fromisoformat(event['ts'].replace('Z', '+00:00')) for event in events]
    if any(t.tzinfo is None for t in times) or any(b < a for a, b in zip(times, times[1:])):
        raise ValueError('lifecycle timestamps must be ordered and timezone-aware')
    if any(type(e.get('seq')) is not int for e in events) or any(
            b['seq'] <= a['seq'] for a, b in zip(events, events[1:])):
        raise ValueError('lifecycle sequence must increase')
    stages = {}
    for start, end, label in [('slot_allocated', 'workload_started', 'admission_to_workload_seconds'),
                              ('workload_started', 'workload_exited', 'workload_seconds')]:
        if names.count(start) == 1 and names.count(end) == 1:
            duration = (times[names.index(end)] - times[names.index(start)]).total_seconds()
            stages[label] = number(duration, label)
    return {'path': str(directory), 'elapsed_seconds': elapsed,
            'execution_complete': not problems, 'problems': problems,
            'artifact_sha256': hashes, **stages}


def report(manifest, base):
    if manifest.get('schema_version') != 1:
        raise ValueError('unsupported manifest schema_version')
    preparation = number(manifest.get('preparation_seconds', 0), 'preparation_seconds')
    batches = manifest['batches']
    if not isinstance(batches, list) or not batches:
        raise ValueError('batches must be a nonempty list')
    seen = set()
    rows = []
    for batch in batches:
        concurrency = batch['concurrency']
        if type(concurrency) is not int or not 1 <= concurrency <= 63:
            raise ValueError('concurrency must be 1..63 for the current network allocator')
        wall = number(batch['wall_seconds'], 'wall_seconds', 1e-12)
        paths = batch['attempts']
        if not isinstance(paths, list) or not paths:
            raise ValueError('each batch needs attempt directories')
        attempts = []
        for path in paths:
            directory = (base / path).resolve()
            if directory in seen:
                raise ValueError(f'duplicate attempt: {directory}')
            seen.add(directory)
            attempt = inspect_attempt(directory)
            if attempt['elapsed_seconds'] > wall + 0.001:
                raise ValueError('batch wall time cannot be shorter than an attempt')
            attempts.append(attempt)
        complete = sum(a['execution_complete'] for a in attempts)
        durations = [a['elapsed_seconds'] for a in attempts]
        rows.append({'name': batch['name'], 'concurrency': concurrency,
                     'wall_seconds': wall, 'attempt_count': len(attempts),
                     'execution_complete_count': complete,
                     'completed_per_minute': 60 * complete / wall,
                     'median_attempt_seconds': statistics.median(durations),
                     'max_attempt_seconds': max(durations), 'attempts': attempts})
    total_wall = preparation + sum(row['wall_seconds'] for row in rows)
    completed = sum(row['execution_complete_count'] for row in rows)
    cost = manifest.get('estimated_total_cost_usd')
    if cost is not None:
        number(cost, 'estimated_total_cost_usd')
    return {'schema_version': 1, 'label': manifest['label'],
            'interpretation': 'Execution evidence only; no independent semantic verdict inferred. '
                              'Batch timings and cost are supplied observations, not remeasured.',
            'preparation_seconds': preparation, 'batches': rows,
            'total_wall_seconds_including_preparation': total_wall,
            'completed_per_minute_including_preparation': 60 * completed / total_wall,
            'estimated_cost_per_execution_complete_usd': cost / completed
            if cost is not None and completed else None}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('manifest', type=Path)
    args = parser.parse_args()
    try:
        value = report(read_json(args.manifest), args.manifest.resolve().parent)
        print(json.dumps(value, indent=2, allow_nan=False))
    except (OSError, ValueError, KeyError, TypeError) as error:
        print(f'experiment-report: {error}', file=sys.stderr)
        return 2
    return 0


if __name__ == '__main__':
    sys.exit(main())
