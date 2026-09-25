import base64
import hashlib
import importlib.util
import io
import json
from email.message import Message
from pathlib import Path
import tempfile
import unittest


SOURCE = Path(__file__).resolve().parents[1] / "printable_client.py"
SPEC = importlib.util.spec_from_file_location("printable_client", SOURCE)
client = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(client)


class Response(io.BytesIO):
    pass


class DownloadTests(unittest.TestCase):
    def test_selected_contracts_do_not_request_the_full_catalog(self):
        instance = object.__new__(client.Client)
        calls = []
        def rpc(method, params):
            calls.append((method, params))
            return {"contents": [{"uri": params["uri"], "text": json.dumps({"tool": "view", "action": "section"})}]}
        instance.rpc = rpc
        self.assertEqual(instance.contracts("view", "section"), {"tool": "view", "action": "section"})
        self.assertEqual(calls, [("resources/read", {"uri": "printable://contracts/view/section"})])
        for tool, action in [(None, "section"), ("../view", None), ("view", "section/other"), ("", None)]:
            with self.subTest(tool=tool, action=action), self.assertRaises(ValueError):
                instance.contracts(tool, action)
        self.assertEqual(len(calls), 1, "invalid names must not reach the server")
        instance.rpc = lambda *args: {"contents": [{"uri": "printable://contracts/other", "text": "{}"}]}
        with self.assertRaises(ValueError):
            instance.contracts("view", "section")

    def test_full_discovery_keeps_pages_and_rejects_a_repeated_cursor(self):
        instance = object.__new__(client.Client)
        calls = []
        def rpc(method, params):
            calls.append((method, params))
            if not params:
                return {"tools": [{"name": "view"}], "nextCursor": "next"}
            return {"tools": [{"name": "render"}]}
        instance.rpc = rpc
        self.assertEqual(instance.tools(), {"tools": [{"name": "view"}, {"name": "render"}]})
        self.assertEqual(calls, [("tools/list", {}), ("tools/list", {"cursor": "next"})])
        instance.rpc = lambda *args: {"tools": [], "nextCursor": "same"}
        with self.assertRaisesRegex(ValueError, "repeated"):
            instance.tools()

    def test_results_prefer_structured_content_and_support_json_text(self):
        instance = object.__new__(client.Client)
        for response in [
            {"structuredContent": {"ok": True}, "content": [{"type": "text", "text": "different"}]},
            {"content": [{"type": "text", "text": '{"ok":true}'}]},
        ]:
            instance.rpc = lambda *args: response
            self.assertEqual(instance.call("status", {}), {"ok": True})
        calls = []
        def rejected(*args):
            calls.append(args)
            return {"isError": True, "structuredContent": {"ok": True}}
        instance.rpc = rejected
        with self.assertRaises(ValueError):
            instance.call("edit", {})
        self.assertEqual(len(calls), 1, "rejected mutations must not be replayed")

    def test_sse_empty_events_and_notifications_before_response(self):
        instance = object.__new__(client.Client)
        instance.url = "http://127.0.0.1/mcp"
        instance.bearer = "0" * 64
        instance.session = None
        instance.sequence = 0
        instance.protocol = "2025-11-25"
        class Transport:
            def open(self, request, timeout):
                response = Response(b'data: \n\ndata: {"jsonrpc":"2.0","method":"notifications/test"}\n\n'
                                    b'data: {"jsonrpc":"2.0","id":1,"result":{"ok":true}}\n\n')
                response.headers = Message()
                response.headers["Content-Type"] = "text/event-stream"
                return response
        instance.http = Transport()
        self.assertEqual(instance.rpc("initialize"), {"ok": True})

    def make_client(self, body, expected=None):
        instance = object.__new__(client.Client)
        digest = base64.urlsafe_b64encode(hashlib.sha256(body).digest()).decode().rstrip("=")
        file = {"uri": "mcp-file://printable/test", "size": len(body),
                "digest": {"algorithm": "sha-256", "value": expected or digest}}
        instance.call = lambda *args: {"file": file}
        instance.rpc = lambda *args: {"file": file, "download": {
            "transport": "http", "method": "GET", "url": "https://example.test/files/a",
            "headers": {"Authorization": "Bearer test-grant"}}}
        class Transport:
            def open(self, request, timeout):
                self.request = request
                return Response(body)
        instance.http = Transport()
        return instance

    def test_verified_download_uses_only_grant_and_never_overwrites(self):
        instance = self.make_client(b"artifact bytes")
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "part.stl"
            result = instance.download("part.stl", output)
            self.assertEqual(result["bytes"], 14)
            self.assertEqual(output.read_bytes(), b"artifact bytes")
            self.assertEqual(instance.http.request.get_header("Authorization"), "Bearer test-grant")
            def unexpected_publication(*args):
                self.fail("Existing destinations must be rejected before artifact publication")
            instance.call = unexpected_publication
            with self.assertRaises(FileExistsError):
                instance.download("part.stl", output)
            self.assertEqual(list(Path(directory).iterdir()), [output])

    def test_corrupt_download_leaves_no_destination_or_temporary_file(self):
        instance = self.make_client(b"artifact bytes", expected="wrong")
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaises(ValueError):
                instance.download("part.stl", Path(directory) / "part.stl")
            self.assertEqual(list(Path(directory).iterdir()), [])

    def test_url_security_boundary(self):
        for url in ["https://example.test/prefix/mcp", "http://127.0.0.1:8000/mcp",
                    "http://[::1]/mcp", "http://localhost/mcp"]:
            self.assertEqual(client.validate_url(url), url)
        for url in ["http://example.test/mcp", "https://user:pass@example.test/mcp",
                    "file:///tmp/a", "https://example.test/mcp?token=a",
                    "https://example.test/mcp#fragment"]:
            with self.subTest(url=url), self.assertRaises(ValueError):
                client.validate_url(url)


if __name__ == "__main__":
    unittest.main()
