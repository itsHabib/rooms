#!/usr/bin/env python3
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

SPEC = importlib.util.spec_from_file_location('report', Path(__file__).with_name('experiment-report.py'))
REPORT = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(REPORT)


class ReportTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.base = Path(self.tmp.name)
        for name in ['a', 'b']:
            root = self.base / name
            (root / 'out').mkdir(parents=True)
            (root / 'summary.json').write_text(json.dumps({'cli_exit': 0, 'elapsed_seconds': 10}))
            (root / 'out/result.json').write_text(json.dumps(
                {'schema_version': 1, 'status': 'succeeded', 'exit_code': 0}))
            events = [{'seq': i + 1, 'room_id': name, 'ts': f'2026-09-14T00:00:0{i}Z', 'event': event}
                      for i, event in enumerate(['slot_allocated', 'workload_started',
                                                'workload_exited', 'collection_done', 'cleanup_done'])]
            (root / 'lifecycle.ndjson').write_text('\n'.join(json.dumps(e) for e in events))
        self.manifest = {'schema_version': 1, 'label': 'fixture', 'preparation_seconds': 20,
                         'estimated_total_cost_usd': 3,
                         'batches': [{'name': 'two', 'concurrency': 2, 'wall_seconds': 10,
                                      'attempts': ['a', 'b']}]}

    def test_parallel_throughput_uses_batch_wall_not_sum(self):
        value = REPORT.report(self.manifest, self.base)
        self.assertEqual(value['batches'][0]['completed_per_minute'], 12)
        self.assertEqual(value['completed_per_minute_including_preparation'], 4)
        self.assertEqual(value['estimated_cost_per_execution_complete_usd'], 1.5)

    def test_guest_pass_with_failed_collection_is_not_complete(self):
        (self.base / 'a/summary.json').write_text('{"cli_exit": 1, "elapsed_seconds": 10}')
        row = REPORT.report(self.manifest, self.base)['batches'][0]
        self.assertEqual(row['execution_complete_count'], 1)
        self.assertEqual(row['attempt_count'], 2)

    def test_missing_result_is_incomplete_not_dropped(self):
        (self.base / 'a/out/result.json').unlink()
        self.assertEqual(REPORT.report(self.manifest, self.base)['batches'][0]['execution_complete_count'], 1)

    def test_cleanup_failure_is_not_pass(self):
        path = self.base / 'a/lifecycle.ndjson'
        path.write_text(path.read_text().replace('cleanup_done', 'cleanup_failed'))
        self.assertFalse(REPORT.inspect_attempt(self.base / 'a')['execution_complete'])

    def test_duplicate_attempts_cannot_inflate_throughput(self):
        self.manifest['batches'][0]['attempts'] = ['a', './a']
        with self.assertRaisesRegex(ValueError, 'duplicate attempt'):
            REPORT.report(self.manifest, self.base)

    def test_invalid_density_and_timing_rejected(self):
        for field, value in [('concurrency', 64), ('wall_seconds', float('nan')), ('wall_seconds', 1)]:
            manifest = json.loads(json.dumps(self.manifest))
            manifest['batches'][0][field] = value
            with self.subTest(field=field), self.assertRaises(ValueError):
                REPORT.report(manifest, self.base)

    def test_zero_success_has_no_cost_per_success(self):
        for name in ['a', 'b']:
            (self.base / name / 'out/result.json').unlink()
        self.assertIsNone(REPORT.report(self.manifest, self.base)['estimated_cost_per_execution_complete_usd'])


if __name__ == '__main__':
    unittest.main()
