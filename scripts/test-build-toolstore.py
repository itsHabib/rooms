#!/usr/bin/env python3
"""Publication/signal regressions; Linux, no Nix, root, or cloud required."""
import importlib.util
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch
from types import SimpleNamespace

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location('builder', Path(__file__).with_name('build-toolstore.py'))
builder = importlib.util.module_from_spec(spec)
spec.loader.exec_module(builder)


class PublicationTests(unittest.TestCase):
    def setUp(self):
        self.handlers = {s: signal.getsignal(s) for s in (signal.SIGTERM, signal.SIGINT)}
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.source = self.root / 'source'
        self.source.mkdir()
        (self.source / 'meta.json').write_text('committed')
        self.destination = self.root / 'output'

    def tearDown(self):
        for sig, handler in self.handlers.items():
            signal.signal(sig, handler)
        self.temporary.cleanup()

    def test_termination_immediately_after_real_rename_commits_success(self):
        original = builder.run
        def interrupted(argv):
            result = original(argv)
            os.kill(os.getpid(), signal.SIGTERM)
            return result
        with patch.object(builder, 'run', interrupted):
            builder.publish_directory(self.source, self.destination)
        self.assertEqual((self.destination / 'meta.json').read_text(), 'committed')
        self.assertFalse(self.source.exists())
        with self.assertRaises(SystemExit) as caught:
            os.kill(os.getpid(), signal.SIGTERM)
        self.assertEqual(caught.exception.code, 0)

    def test_mv_killed_after_rename_is_reconciled_by_inode(self):
        original = builder.run
        def killed(argv):
            original(argv)
            raise subprocess.CalledProcessError(-signal.SIGTERM, argv)
        with patch.object(builder, 'run', killed):
            builder.publish_directory(self.source, self.destination)
        self.assertEqual((self.destination / 'meta.json').read_text(), 'committed')

    def test_existing_destination_is_preserved(self):
        self.destination.mkdir()
        (self.destination / 'sentinel').write_text('untouched')
        with self.assertRaises((FileExistsError, subprocess.CalledProcessError)):
            builder.publish_directory(self.source, self.destination)
        self.assertEqual((self.destination / 'sentinel').read_text(), 'untouched')
        self.assertTrue(self.source.exists())
        self.assertEqual(signal.getsignal(signal.SIGTERM), self.handlers[signal.SIGTERM])

    def test_hidden_symlink_is_rejected_before_nix(self):
        flake = self.root / 'flake'
        flake.mkdir()
        (flake / 'flake.lock').write_text('{"nodes":{}}')
        (flake / '.hidden-link').symlink_to('/etc/passwd')
        with patch.object(builder, 'run') as run:
            with self.assertRaisesRegex(ValueError, 'symlinked local flake'):
                builder.build(SimpleNamespace(out=self.destination, preset='python'), flake)
            run.assert_not_called()

    def test_hidden_files_and_directories_are_included_by_pathlib(self):
        (self.source / '.hidden').write_text('hidden')
        (self.source / '.dir').mkdir()
        (self.source / '.dir/value').write_text('nested')
        files = {str(p.relative_to(self.source)): builder.digest(p)
                 for p in self.source.rglob('*') if p.is_file()}
        self.assertIn('.hidden', files)
        self.assertIn('.dir/value', files)


if __name__ == '__main__':
    unittest.main()
