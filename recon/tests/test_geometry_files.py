"""The geometry model list and URL-pinned model files, with fake files and no downloads."""

import hashlib
import io
from pathlib import Path
import tempfile
import unittest
import urllib.error
import urllib.request

from printable_recon import geometry_files, model_cache
from printable_recon.geometry_files import GeometryFilesError
from printable_recon.model_cache import Manifest, ModelFile, PreparationError

MODELS = Path(__file__).resolve().parents[1] / "models"


def document():
    content = b"weights"
    entry = {"model": "Example", "outputs": {"depth": "metric", "mask": "valid_pixels"},
             "files": [{"repo": "example/model", "revision": "a" * 40, "directory": "model", "filename": "model.pt",
                        "sha256": hashlib.sha256(content).hexdigest(), "bytes": len(content)}]}
    return {"format": "printable-geometry-models 1", "models": {"example": entry}}


class ModelListTests(unittest.TestCase):
    def assert_invalid(self, change, message):
        data = document()
        change(data)
        with self.assertRaisesRegex(GeometryFilesError, message):
            geometry_files.parse(data)

    def test_a_valid_list_reads(self):
        model = geometry_files.parse(document())["example"]
        self.assertEqual((model.model, model.outputs["depth"], len(model.manifest.files)), ("Example", "metric", 1))

    def test_files_must_be_pinned_and_outputs_known(self):
        self.assert_invalid(lambda d: d["models"]["example"]["files"][0].update(revision="main"), "full commit")
        self.assert_invalid(lambda d: d["models"]["example"]["files"][0].pop("sha256"), "invalid file")
        self.assert_invalid(lambda d: d["models"]["example"]["outputs"].update(depth="disparity"), "unknown depth")
        self.assert_invalid(lambda d: d.update(format="printable-geometry-models 2"), "unsupported")

    def test_published_models_are_pinned_and_marigold_is_converted_from_its_inputs(self):
        models = geometry_files.read(MODELS / "geometry.json")
        for model in models.values():
            self.assertTrue(model.manifest.files)
        self.assertEqual(models["marigold-v2"].conversion, "marigold-v2")
        self.assertEqual({item.directory for item in models["marigold-v2"].manifest.files},
                         {"qwen-image-edit-2509", "marigold-v2-0"})
        self.assertEqual(len([item for item in models["dp2-h"].manifest.files if item.url]), 2)


class UrlFileTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.directory = Path(self.temp.name)
        self.content = b"weights hosted outside the Hub"
        self.item = ModelFile(None, None, "weights", "model.pt", hashlib.sha256(self.content).hexdigest(),
                              len(self.content), url="https://models.example.test/files/model.pt")

    def test_url_files_must_be_plain_https_and_not_also_from_the_hub(self):
        for url in ("http://models.example.test/model.pt", "https://user:secret@models.example.test/model.pt",
                    "https://models.example.test/model.pt?token=x", "https://models.example.test/"):
            with self.assertRaises(ValueError, msg=url):
                ModelFile(None, None, "weights", "model.pt", "a" * 64, 1, url=url)
        with self.assertRaisesRegex(ValueError, "not both"):
            ModelFile("example/model", "a" * 40, "weights", "model.pt", "a" * 64, 1, url=self.item.url)
        with self.assertRaisesRegex(ValueError, "must not end in .partial"):
            ModelFile("example/model", "a" * 40, "weights", "model.pt.partial", "a" * 64, 1)
        with self.assertRaisesRegex(ValueError, "must not end in .partial"):
            ModelFile("example/model", "a" * 40, "weights", "model.pt.partial/inner.pt", "a" * 64, 1)

    def test_hub_manifest_identifiers_are_unchanged(self):
        published = Manifest.read(MODELS / "trellis2.json")
        self.assertEqual(published.identifier, "0a0f4ad1e25d70bee6a0ac07d44a6ce62927d3d72750bf64b97261a4e9bbc962")
        self.assertNotIn("url", published.document()["files"][0])
        self.assertNotIn("repo", self.item.document())

    def test_a_download_resumes_from_a_partial_file(self):
        partial = self.directory / "model.pt.partial"
        partial.write_bytes(self.content[:10])
        requests = []

        class Response(io.BytesIO):
            status = 206

        def opener(request, timeout):
            requests.append(request.headers.get("Range"))
            return Response(self.content[10:])

        path = model_cache._download_url(self.item, self.directory, opener)
        self.assertEqual(path.read_bytes(), self.content)
        self.assertEqual(requests, ["bytes=10-"])
        self.assertFalse(partial.exists())

    def test_a_server_that_ignores_ranges_restarts_the_file(self):
        (self.directory / "model.pt.partial").write_bytes(b"stale")

        class Response(io.BytesIO):
            status = 200

        path = model_cache._download_url(self.item, self.directory, lambda request, timeout: Response(self.content))
        self.assertEqual(path.read_bytes(), self.content)

    def test_oversized_and_failed_downloads_are_refused(self):
        class Response(io.BytesIO):
            status = 200

        with self.assertRaisesRegex(PreparationError, "larger than pinned"):
            model_cache._download_url(self.item, self.directory, lambda r, timeout: Response(self.content + b"extra"))

        def failing(request, timeout):
            raise urllib.error.HTTPError(self.item.url, 404, "missing", {}, None)

        with self.assertRaisesRegex(PreparationError, "HTTP 404"):
            model_cache._download_url(self.item, self.directory, failing)

    def test_a_short_response_keeps_its_bytes_for_the_next_attempt(self):
        class Response(io.BytesIO):
            status = 200

        with self.assertRaisesRegex(PreparationError, "ended early"):
            model_cache._download_url(self.item, self.directory, lambda r, timeout: Response(self.content[:7]))
        self.assertEqual((self.directory / "model.pt.partial").read_bytes(), self.content[:7])
        self.assertFalse((self.directory / "model.pt").exists())

    def test_a_complete_partial_file_is_finished_without_a_request(self):
        (self.directory / "model.pt.partial").write_bytes(self.content)

        def unused(request, timeout):
            raise AssertionError("no request is needed")

        path = model_cache._download_url(self.item, self.directory, unused)
        self.assertEqual(path.read_bytes(), self.content)

    def test_a_symbolic_link_in_place_of_the_partial_file_is_refused(self):
        elsewhere = self.directory / "verified.pt"
        elsewhere.write_bytes(b"keep")
        (self.directory / "model.pt.partial").symlink_to(elsewhere)
        with self.assertRaisesRegex(PreparationError, "symbolic links"):
            model_cache._download_url(self.item, self.directory, lambda r, timeout: io.BytesIO(self.content))
        self.assertEqual(elsewhere.read_bytes(), b"keep")

    def test_a_refused_range_starts_the_next_attempt_again(self):
        (self.directory / "model.pt.partial").write_bytes(self.content[:5])

        def refused(request, timeout):
            raise urllib.error.HTTPError(self.item.url, 416, "range not satisfiable", {}, None)

        with self.assertRaisesRegex(PreparationError, "HTTP 416"):
            model_cache._download_url(self.item, self.directory, refused)
        self.assertFalse((self.directory / "model.pt.partial").exists())

    def test_a_hard_linked_partial_file_is_not_written_through(self):
        class Response(io.BytesIO):
            status = 200

        for existing in (self.content[:5], b""):
            other = self.directory / "other.pt"
            other.write_bytes(existing)
            (self.directory / "model.pt.partial").hardlink_to(other)
            path = model_cache._download_url(self.item, self.directory, lambda r, timeout: Response(self.content))
            self.assertEqual(path.read_bytes(), self.content)
            self.assertEqual(other.read_bytes(), existing)
            path.unlink()
            other.unlink()

    def test_a_response_that_breaks_off_keeps_its_bytes_and_reports_a_download_failure(self):
        import http.client

        class Response(io.BytesIO):
            status = 200

            def read(self, size=-1):
                if self.tell():
                    raise http.client.IncompleteRead(b"")
                return super().read(5)

        with self.assertRaisesRegex(PreparationError, "check network access and retry"):
            model_cache._download_url(self.item, self.directory, lambda r, timeout: Response(self.content))
        self.assertEqual((self.directory / "model.pt.partial").read_bytes(), self.content[:5])

    def test_redirects_away_from_https_are_refused(self):
        handler = model_cache._HttpsRedirects()
        request = urllib.request.Request(self.item.url)
        with self.assertRaisesRegex(PreparationError, "away from HTTPS"):
            handler.redirect_request(request, None, 302, "Found", {}, "http://models.example.test/model.pt")
        followed = handler.redirect_request(request, None, 302, "Found", {}, "https://cdn.example.test/model.pt")
        self.assertEqual(followed.full_url, "https://cdn.example.test/model.pt")

    def test_prepare_verifies_url_files_like_hub_files(self):
        root = self.directory / "cache"
        manifest = Manifest((self.item,))

        def download(item, directory, endpoint):
            path = directory / item.filename
            path.write_bytes(self.content)
            return path

        snapshot = model_cache.prepare(root, manifest, download=download)
        self.assertEqual((snapshot / self.item.relative_path).read_bytes(), self.content)


if __name__ == "__main__":
    unittest.main()
