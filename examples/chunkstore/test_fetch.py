import contextlib
import os
import random
import tempfile
import unittest

import chunks
import fetch
import serve

KIB = 1024


class FetchTest(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.dir = tmp.name
        rng = random.Random(11)
        self.data = rng.randbytes(704 * KIB) + bytes(256 * KIB) + rng.randbytes(300 * KIB + 5)
        self.paths = [self.write("snapshot.mem", self.data), self.write("snapshot.vmstate", rng.randbytes(9 * KIB))]
        self.source = chunks.Store(os.path.join(self.dir, "source"))
        self.spec = chunks.fixed_spec(32 * KIB)
        manifest = chunks.build_manifest("snap", self.paths, self.spec, self.source)
        self.manifest_sha = self.source.save_manifest(manifest)

    def write(self, name, data):
        path = os.path.join(self.dir, name)
        with open(path, "wb") as handle:
            handle.write(data)
        return path

    def restored(self, out):
        with open(os.path.join(out, "snapshot.mem"), "rb") as handle:
            return handle.read()

    @contextlib.contextmanager
    def seeds(self, *corrupt_flags):
        with contextlib.ExitStack() as stack:
            yield [stack.enter_context(serve.serving(self.source.root, corrupt=flag)) for flag in corrupt_flags]

    def test_spreads_chunks_across_seeds(self):
        local = chunks.Store(os.path.join(self.dir, "local"))
        out = os.path.join(self.dir, "out")
        with self.seeds(False, False, False) as urls:
            stats = fetch.fetch("snap", urls, local, out, workers=6)
        self.assertEqual(self.restored(out), self.data)
        self.assertEqual(stats["retries"], 0)
        self.assertEqual(sum(seed["chunks"] for seed in stats["seeds"]), stats["chunks_total"])
        self.assertTrue(all(seed["chunks"] > 0 for seed in stats["seeds"]))
        self.assertEqual(stats["bytes_fetched"], 1004 * KIB + 5 + 9 * KIB)

    def test_corrupt_seed_is_dropped_and_routed_around(self):
        local = chunks.Store(os.path.join(self.dir, "local"))
        out = os.path.join(self.dir, "out")
        with self.seeds(True, False, False) as urls:
            stats = fetch.fetch("snap", urls, local, out, workers=6)
        self.assertEqual(self.restored(out), self.data)
        bad, *good = stats["seeds"]
        self.assertIsNotNone(bad["dropped"])
        self.assertGreater(bad["bad_chunks"], 0)
        self.assertEqual((bad["chunks"], bad["bytes"]), (0, 0))
        self.assertEqual(stats["retries"], bad["bad_chunks"])
        self.assertEqual(sum(seed["chunks"] for seed in good), stats["chunks_total"])
        for sha in local.hashes():
            self.assertEqual(chunks.sha256_hex(local.get(sha)), sha)

    def test_only_corrupt_seeds_fails_and_stores_nothing(self):
        local = chunks.Store(os.path.join(self.dir, "local"))
        out = os.path.join(self.dir, "out")
        with self.seeds(True, True) as urls:
            with self.assertRaises(fetch.FetchError):
                fetch.fetch("snap", urls, local, out)
        self.assertEqual(local.hashes(), set())
        self.assertFalse(os.path.exists(out))

    def test_local_chunks_are_not_fetched_again(self):
        local = chunks.Store(os.path.join(self.dir, "local"))
        changed = bytearray(self.data)
        changed[40 * KIB:41 * KIB] = bytes(KIB)
        chunks.build_manifest("older", [self.write("older.mem", bytes(changed))], self.spec, local)
        with self.seeds(False) as urls:
            stats = fetch.fetch("snap", urls, local, os.path.join(self.dir, "out"))
        self.assertEqual(stats["chunks_fetched"], 2)
        self.assertEqual(stats["bytes_fetched"], 32 * KIB + 9 * KIB)
        self.assertEqual(self.restored(os.path.join(self.dir, "out")), self.data)

    def test_fetched_host_can_seed_the_next_one(self):
        second = chunks.Store(os.path.join(self.dir, "second"))
        third = chunks.Store(os.path.join(self.dir, "third"))
        with self.seeds(False) as urls:
            fetch.fetch("snap", urls, second)
        with serve.serving(second.root) as url:
            fetch.fetch("snap", [url], third, os.path.join(self.dir, "out"))
        self.assertEqual(self.restored(os.path.join(self.dir, "out")), self.data)

    def test_seed_lacking_chunks_is_only_asked_for_what_it_has(self):
        partial = chunks.Store(os.path.join(self.dir, "partial"))
        chunks.build_manifest("part", [self.paths[1]], self.spec, partial)
        local = chunks.Store(os.path.join(self.dir, "local"))
        with self.seeds(False) as urls, serve.serving(partial.root) as partial_url:
            stats = fetch.fetch("snap", [partial_url] + urls, local, os.path.join(self.dir, "out"))
        self.assertEqual(stats["retries"], 0)
        self.assertLessEqual(stats["seeds"][0]["chunks"], 1)
        self.assertEqual(self.restored(os.path.join(self.dir, "out")), self.data)

    def test_pinned_manifest_hash_is_enforced(self):
        local = chunks.Store(os.path.join(self.dir, "local"))
        with self.seeds(False) as urls:
            with self.assertRaises(fetch.FetchError):
                fetch.fetch("snap", urls, local, manifest_sha="0" * 64)
            stats = fetch.fetch("snap", urls, local, manifest_sha=self.manifest_sha)
        self.assertEqual(stats["chunks_local"], 0)

    def test_unreachable_seed_is_survived(self):
        local = chunks.Store(os.path.join(self.dir, "local"))
        with serve.serving(self.source.root) as dead:
            pass
        with self.seeds(False) as urls:
            stats = fetch.fetch("snap", [dead] + urls, local, os.path.join(self.dir, "out"))
        self.assertIsNotNone(stats["seeds"][0]["dropped"])
        self.assertEqual(self.restored(os.path.join(self.dir, "out")), self.data)


if __name__ == "__main__":
    unittest.main()
