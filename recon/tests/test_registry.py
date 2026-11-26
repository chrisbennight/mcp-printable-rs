"""Geometry model registry and route selection, with fake entries and no downloads."""

import copy
import hashlib
import io
from pathlib import Path
import tempfile
import unittest
import urllib.error
import urllib.request

from printable_recon import model_cache
from printable_recon.model_cache import Manifest, ModelFile, PreparationError
from printable_recon.registry import Host, NoRoute, Registry, RegistryError, ROLES

MODELS = Path(__file__).resolve().parents[1] / "models"


def hub_file(directory="model", content=b"weights"):
    return {"repo": "example/model", "revision": "a" * 40, "directory": directory, "filename": "model.pt",
            "sha256": hashlib.sha256(content).hexdigest(), "bytes": len(content)}


def document():
    entry = {"model": "Example", "outputs": {"depth": "metric", "mask": "valid_pixels", "focal": "pixels",
                                            "normals": "camera"}, "files": [hub_file()]}
    small = copy.deepcopy(entry)
    small["files"] = [hub_file("small")]
    options = [
        {"entry": "large", "device": "cuda", "side": 2048, "seconds": 1.0, "gpu_peak_mib": 8000, "ram_peak_mib": 4000},
        {"entry": "small", "device": "cuda", "side": 1024, "seconds": 0.5, "gpu_peak_mib": 2000, "ram_peak_mib": 3000},
        {"entry": "small", "device": "cpu", "side": 1024, "seconds": 20.0, "ram_peak_mib": 6000},
    ]
    return {"format": "printable-geometry 1", "entries": {"large": entry, "small": small},
            "routes": {role: copy.deepcopy(options) for role in ROLES}}


class SelectionTests(unittest.TestCase):
    def setUp(self):
        self.registry = Registry.parse(document())

    def test_the_best_option_runs_when_the_budget_allows(self):
        selection = self.registry.select("depth.deep", Host(gpu_budget_mib=24000))
        self.assertEqual((selection.option.entry, selection.option.side), ("large", 2048))
        self.assertFalse(selection.fallback)

    def test_a_lower_budget_falls_back_and_says_why(self):
        selection = self.registry.select("mask", Host(gpu_budget_mib=3000))
        self.assertEqual((selection.option.entry, selection.option.device), ("small", "cuda"))
        self.assertTrue(selection.fallback)
        self.assertIn("needs 8000 MiB of GPU memory, the budget is 3000 MiB", selection.rejected[0])

    def test_without_a_gpu_the_cpu_option_runs(self):
        selection = self.registry.select("focal", Host(gpu_budget_mib=None))
        self.assertEqual(selection.option.device, "cpu")
        self.assertEqual(len(selection.rejected), 2)
        self.assertIn("no GPU is available", selection.rejected[0])

    def test_nothing_fitting_is_refused_with_every_reason(self):
        with self.assertRaises(NoRoute) as raised:
            self.registry.select("normals", Host(gpu_budget_mib=None, ram_budget_mib=1000))
        message = str(raised.exception)
        self.assertIn("no normals model fits this worker", message)
        self.assertIn("needs 6000 MiB of memory, the budget is 1000 MiB", message)
        self.assertEqual(message.count(";"), 2)

    def test_a_ram_budget_also_limits_gpu_options(self):
        selection = self.registry.select("depth.shallow", Host(gpu_budget_mib=24000, ram_budget_mib=3500))
        self.assertEqual((selection.option.entry, selection.option.device), ("small", "cuda"))

    def test_unknown_roles_are_refused(self):
        with self.assertRaises(NoRoute):
            self.registry.select("albedo", Host(gpu_budget_mib=24000))


class ValidationTests(unittest.TestCase):
    def assert_invalid(self, change, message):
        data = document()
        change(data)
        with self.assertRaisesRegex(RegistryError, message):
            Registry.parse(data)

    def test_every_role_needs_options(self):
        self.assert_invalid(lambda d: d["routes"].pop("focal"), "roles without options: focal")

    def test_options_name_known_entries_with_that_output(self):
        self.assert_invalid(lambda d: d["routes"]["mask"][0].update(entry="missing"), "unknown entry")
        self.assert_invalid(lambda d: d["routes"]["mask"][0].update(entry=["large"]), "unknown entry")
        self.assert_invalid(lambda d: d["entries"]["large"]["outputs"].pop("mask"), "has no mask output")

    def test_gpu_options_need_a_gpu_peak_and_cpu_options_none(self):
        self.assert_invalid(lambda d: d["routes"]["mask"][0].pop("gpu_peak_mib"), "GPU options")
        self.assert_invalid(lambda d: d["routes"]["mask"][2].update(gpu_peak_mib=10), "GPU options")

    def test_measurements_must_be_positive(self):
        self.assert_invalid(lambda d: d["routes"]["mask"][0].update(seconds=0), "measured seconds")
        self.assert_invalid(lambda d: d["routes"]["mask"][0].update(gpu_peak_mib=True), "GPU options")
        self.assert_invalid(lambda d: d["routes"]["mask"][1].pop("ram_peak_mib"), "measured RAM peak")

    def test_files_must_be_pinned(self):
        self.assert_invalid(lambda d: d["entries"]["large"]["files"][0].update(revision="main"), "full commit")
        self.assert_invalid(lambda d: d["entries"]["large"]["files"][0].pop("sha256"), "invalid file")

    def test_unknown_outputs_roles_and_formats_are_refused(self):
        self.assert_invalid(lambda d: d["entries"]["large"]["outputs"].update(depth="disparity"), "unknown depth output")
        self.assert_invalid(lambda d: d["routes"].update(albedo=d["routes"]["mask"]), "unknown role")
        self.assert_invalid(lambda d: d.update(format="printable-geometry 2"), "unsupported")


class PublishedRegistryTests(unittest.TestCase):
    def setUp(self):
        self.registry = Registry.read(MODELS / "geometry.json")

    def test_routes_match_the_benchmark_choices(self):
        def chosen(role, budget, ram=None):
            option = self.registry.select(role, Host(budget, ram)).option
            return option.entry, option.device, option.side

        self.assertEqual(chosen("depth.shallow", 24 * 1024), ("da3-mono-large", "cuda", 1024))
        self.assertEqual(chosen("depth.shallow", None), ("da3-mono-large", "cpu", 1024))
        self.assertEqual(chosen("depth.deep", 24 * 1024), ("dp2-h", "cuda", 2048))
        self.assertEqual(chosen("depth.deep", 8 * 1024), ("dp2-h", "cuda", 1024))
        self.assertEqual(chosen("depth.deep", 3 * 1024), ("dp2-l", "cuda", 1024))
        self.assertEqual(chosen("depth.deep", None), ("dp2-h", "cpu", 1024))
        self.assertEqual(chosen("mask", 24 * 1024), ("moge3-vitg", "cuda", 1024))
        self.assertEqual(chosen("mask", 3 * 1024), ("moge3-vitl", "cuda", 1024))
        self.assertEqual(chosen("mask", None), ("moge2-vitl-normal", "cpu", 1024))
        self.assertEqual(chosen("focal", 3 * 1024), ("dp2-l", "cuda", 1024))
        self.assertEqual(chosen("normals", 3 * 1024), ("marigold-v2", "cuda", 1024))

    def test_every_route_runs_without_a_gpu(self):
        for role in ROLES:
            self.assertEqual(self.registry.select(role, Host(gpu_budget_mib=None)).option.device, "cpu", role)

    def test_published_files_are_pinned_and_marigold_is_converted_from_its_inputs(self):
        for entry in self.registry.entries.values():
            self.assertTrue(entry.manifest.files)
        marigold = self.registry.entries["marigold-v2"]
        self.assertEqual(marigold.conversion, "marigold-v2")
        self.assertEqual({item.directory for item in marigold.manifest.files}, {"qwen-image-edit-2509", "marigold-v2-0"})
        urls = [item for item in self.registry.entries["dp2-h"].manifest.files if item.url]
        self.assertEqual(len(urls), 2)


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
