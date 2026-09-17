import http.client
import json
import os
import random
import tempfile
import time
import unittest
import urllib.parse

import chunks
import serve

KIB = 1024


class ServeTest(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.dir = tmp.name
        self.secret = os.path.join(self.dir, "secret.json")
        with open(self.secret, "w") as handle:
            handle.write("top secret")
        self.store = chunks.Store(os.path.join(self.dir, "store"))
        source = os.path.join(self.dir, "data.bin")
        with open(source, "wb") as handle:
            handle.write(random.Random(1).randbytes(200 * KIB))
        self.manifest = chunks.build_manifest("snap", [source], chunks.fixed_spec(64 * KIB), self.store)
        self.store.save_manifest(self.manifest)

    def get(self, url, path):
        parts = urllib.parse.urlsplit(url)
        conn = http.client.HTTPConnection(parts.hostname, parts.port, timeout=10)
        self.addCleanup(conn.close)
        conn.request("GET", path)
        response = conn.getresponse()
        return response.status, response.read()

    def test_serves_manifest_chunks_and_have(self):
        sha = self.manifest["files"][0]["chunks"][0][2]
        with serve.serving(self.store.root) as url:
            status, body = self.get(url, "/manifest/snap")
            self.assertEqual((status, json.loads(body)), (200, self.manifest))
            status, body = self.get(url, f"/chunk/{sha}")
            self.assertEqual((status, chunks.sha256_hex(body)), (200, sha))
            status, body = self.get(url, "/have")
            self.assertEqual((status, set(json.loads(body))), (200, self.store.hashes()))
            self.assertEqual(self.get(url, "/chunk/" + "0" * 64)[0], 404)
            self.assertEqual(self.get(url, "/manifest/absent")[0], 404)

    def test_never_serves_a_path_outside_the_store(self):
        paths = [
            "/manifest/../../secret",
            "/manifest/..%2f..%2fsecret",
            "/manifest/%2e%2e/%2e%2e/secret",
            "/chunk/../../secret.json",
            "/chunk/../manifests/snap.json",
            "/objects/../../secret.json",
            "/../secret.json",
            "//etc/passwd",
            "/manifest/snap?x=../../secret",
            "/have/../../secret.json",
        ]
        with serve.serving(self.store.root) as url:
            for path in paths:
                status, body = self.get(url, path)
                self.assertIn(status, (400, 404), path)
                self.assertNotIn(b"top secret", body, path)

    def test_symlink_out_of_the_store_is_refused(self):
        os.symlink(self.secret, os.path.join(self.store.manifests, "leak.json"))
        with serve.serving(self.store.root) as url:
            status, body = self.get(url, "/manifest/leak")
        self.assertEqual(status, 400)
        self.assertNotIn(b"top secret", body)

    def test_corrupt_flag_changes_chunk_bytes_only(self):
        sha = self.manifest["files"][0]["chunks"][0][2]
        with serve.serving(self.store.root, corrupt=True) as url:
            self.assertNotEqual(chunks.sha256_hex(self.get(url, f"/chunk/{sha}")[1]), sha)
            self.assertEqual(json.loads(self.get(url, "/manifest/snap")[1]), self.manifest)

    def test_token_bucket_limits_throughput(self):
        bucket = serve.TokenBucket(rate=1_000_000)
        started = time.monotonic()
        for _ in range(10):
            bucket.take(50_000)
        elapsed = time.monotonic() - started
        self.assertGreater(elapsed, 0.4)
        self.assertLess(elapsed, 2.0)


if __name__ == "__main__":
    unittest.main()
