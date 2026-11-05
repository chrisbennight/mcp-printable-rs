"""Model preparation contracts with an isolated fake Hub, never live downloads."""

from dataclasses import replace
import hashlib
import json
import os
from pathlib import Path
import tempfile
import threading
import unittest
from unittest.mock import patch

from printable_recon.model_cache import Manifest, ModelFile, PreparationError, prepare


class ModelCacheTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name) / "models"
        self.content = b"model data from a pinned revision"
        self.item = ModelFile("example/shape", "a" * 40, "shape", "ckpts/weights.safetensors",
                              hashlib.sha256(self.content).hexdigest(), len(self.content))
        self.manifest = Manifest((self.item,))
        self.downloads = []
        self.addCleanup(patch.stopall)
        patch.dict(os.environ, {"HF_ENDPOINT": "https://models.example.test/hub"}).start()

    def download(self, item, directory, endpoint):
        self.downloads.append((item, endpoint))
        path = directory / item.filename
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(self.content)
        return path

    def test_first_use_publishes_verified_files_and_offline_reuse_needs_no_network(self):
        progress = []
        snapshot = prepare(self.root, self.manifest, download=self.download, progress=progress.append)
        self.assertEqual((snapshot / self.item.relative_path).read_bytes(), self.content)
        self.assertEqual(json.loads((snapshot / "manifest.json").read_text()), self.manifest.document())
        self.assertEqual(progress[-1]["completed_bytes"], len(self.content))
        self.assertEqual(self.downloads[0][1], "https://models.example.test/hub")
        def offline(*_):
            self.fail("prepared model attempted a download")
        self.assertEqual(prepare(self.root, self.manifest, download=offline), snapshot)

    def test_interruption_keeps_verified_partial_files_and_retry_completes(self):
        second = replace(self.item, filename="ckpts/second.safetensors")
        manifest = Manifest((self.item, second))
        def interrupted(item, directory, endpoint):
            if item == second:
                raise PreparationError("interrupted")
            return self.download(item, directory, endpoint)
        with self.assertRaisesRegex(PreparationError, "interrupted"):
            prepare(self.root, manifest, download=interrupted)
        self.assertFalse((self.root / "snapshots" / manifest.identifier).exists())
        snapshot = prepare(self.root, manifest, download=self.download)
        self.assertTrue(snapshot.is_dir())
        self.assertEqual([item for item, _ in self.downloads], [self.item, second])

    def test_corruption_never_becomes_a_ready_snapshot(self):
        def corrupt(item, directory, endpoint):
            path = self.download(item, directory, endpoint)
            path.write_bytes(b"x" * item.bytes)
            return path
        with self.assertRaisesRegex(PreparationError, "failed download verification"):
            prepare(self.root, self.manifest, download=corrupt)
        snapshot = prepare(self.root, self.manifest, download=self.download)
        path = snapshot / self.item.relative_path
        path.write_bytes(b"x" * self.item.bytes)
        with self.assertRaisesRegex(PreparationError, "missing or corrupt"):
            prepare(self.root, self.manifest, download=self.download)

    def test_concurrent_requests_share_one_preparation(self):
        results, errors = [], []
        entered, release = threading.Event(), threading.Event()
        def download(item, directory, endpoint):
            entered.set()
            if not release.wait(5):
                raise RuntimeError("test downloader was not released")
            return self.download(item, directory, endpoint)
        def request():
            try:
                results.append(prepare(self.root, self.manifest, download=download))
            except BaseException as error:
                errors.append(error)
        first, second = threading.Thread(target=request), threading.Thread(target=request)
        first.start()
        self.assertTrue(entered.wait(5))
        second.start()
        release.set()
        first.join(5)
        second.join(5)
        self.assertFalse(first.is_alive() or second.is_alive())
        self.assertEqual(errors, [])
        self.assertEqual(len(results), 2)
        self.assertEqual(results[0], results[1])
        self.assertEqual(len(self.downloads), 1)

    def test_paths_revisions_and_endpoint_are_checked_before_download(self):
        for change in ({"filename": "../outside"}, {"directory": "/tmp"}, {"revision": "main"}):
            with self.subTest(change=change), self.assertRaises(ValueError):
                replace(self.item, **change)
        with patch.dict(os.environ, {"HF_ENDPOINT": "https://user:password@models.example.test"}):
            with self.assertRaisesRegex(PreparationError, "without credentials"):
                prepare(self.root, self.manifest, download=self.download)
        self.assertEqual(self.downloads, [])

    def test_existing_symlink_cannot_redirect_model_storage(self):
        self.root.mkdir()
        outside = Path(self.temp.name) / "outside"
        outside.mkdir()
        (self.root / "downloads").symlink_to(outside, target_is_directory=True)
        with self.assertRaisesRegex(PreparationError, "escapes"):
            prepare(self.root, self.manifest, download=self.download)
        self.assertEqual(list(outside.iterdir()), [])

    def test_retained_metadata_symlinks_cannot_overwrite_other_files(self):
        for target_kind in ("outside", "checkpoint"):
            with self.subTest(target=target_kind):
                root = self.root / target_kind
                pending = root / "downloads" / self.manifest.identifier
                pending.mkdir(parents=True)
                outside = Path(self.temp.name) / "unrelated.txt"
                outside.write_bytes(b"keep unrelated data")
                target = outside if target_kind == "outside" else pending / self.item.relative_path
                (pending / "manifest.json").symlink_to(target)
                snapshot = prepare(root, self.manifest, download=self.download)
                self.assertEqual(outside.read_bytes(), b"keep unrelated data")
                self.assertEqual((snapshot / self.item.relative_path).read_bytes(), self.content)
                self.assertFalse((snapshot / "manifest.json").is_symlink())
                self.assertEqual(json.loads((snapshot / "manifest.json").read_text()), self.manifest.document())

    def test_retained_model_aliases_are_refused_before_snapshot_promotion(self):
        for alias_kind in ("file", "directory"):
            with self.subTest(alias=alias_kind):
                root = self.root / alias_kind
                pending = root / "downloads" / self.manifest.identifier
                backing = pending / "backing" / "ckpts" / "weights.safetensors"
                backing.parent.mkdir(parents=True)
                backing.write_bytes(self.content)
                if alias_kind == "directory":
                    (pending / self.item.directory).symlink_to(pending / "backing", target_is_directory=True)
                else:
                    path = pending / self.item.relative_path
                    path.parent.mkdir(parents=True)
                    path.symlink_to(backing)
                with self.assertRaisesRegex(PreparationError, "symbolic links"):
                    prepare(root, self.manifest, download=self.download)
                self.assertFalse((root / "snapshots" / self.manifest.identifier).exists())
        self.assertEqual(self.downloads, [])


if __name__ == "__main__":
    unittest.main()
