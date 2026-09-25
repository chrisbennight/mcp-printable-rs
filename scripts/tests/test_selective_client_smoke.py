import json
from pathlib import Path
import tempfile
import unittest

from printable_client import Client
from selective_client_smoke import run


class DiscoveryEvidenceTests(unittest.TestCase):
    def test_failed_response_is_retained_without_replay_or_exception_details(self):
        client = object.__new__(Client)
        client.server_info = {"name": "fixture", "version": "test"}
        client.protocol = "2025-11-25"
        calls = []
        def rpc(*args, **kwargs):
            calls.append(args)
            raise TimeoutError("private diagnostic")
        client.rpc = rpc
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "failed"
            with self.assertRaises(TimeoutError):
                run(client, output)
            self.assertIs(client.rpc, rpc)
            evidence = json.loads((output / "evidence.json").read_text())
            self.assertEqual(evidence["results"][0]["outcome"], "failed")
            retained = (output / "full-calls.json").read_text()
            self.assertNotIn("private diagnostic", retained)
            self.assertEqual(json.loads(retained)[0]["outcome"], "response_unavailable")
            self.assertEqual(len(calls), 1)

    def test_evidence_keeps_actual_contracts_calls_and_unavailable_usage(self):
        client = object.__new__(Client)
        client.server_info = {"name": "fixture", "version": "test"}
        client.protocol = "2025-11-25"

        def rpc(method, params, notification=False):
            if method == "tools/list":
                return {"tools": [{"name": name, "inputSchema": {"fixture": name}}
                                  for name in ("scad_build", "validate_mesh", "printer")]}
            if method == "resources/read":
                uri = params["uri"]
                if uri == "printable://contracts":
                    value = {"tools": [{"tool": "scad_build"}, {"tool": "validate_mesh"}]}
                else:
                    value = {"selected": uri}
                return {"contents": [{"uri": uri, "text": json.dumps(value)}]}
            self.assertEqual(method, "tools/call")
            if params["name"] == "scad_build":
                value = {"artifact": {"path": params["arguments"]["params"]["path"]}}
            else:
                self.assertEqual(params["name"], "validate_mesh")
                value = {"report": {"solid_geometry": True, "bounds": {"dimensions_mm": [10, 20, 30]}}}
            return {"structuredContent": value, "content": [{"type": "text", "text": json.dumps(value)}]}

        client.rpc = rpc
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "evidence"
            evidence = run(client, output, {"server": {"image_id": "fixture"}})
            self.assertIs(client.rpc, rpc)
            self.assertEqual(evidence["server"], client.server_info)
            self.assertIsNone(evidence["model_usage"])
            self.assertIsNone(evidence["model"])
            self.assertEqual([item["outcome"] for item in evidence["results"]], ["passed", "passed"])
            selected = json.loads((output / "selective-calls.json").read_text())
            self.assertEqual([item["method"] for item in selected],
                             ["resources/read"] * 3 + ["tools/call"] * 2)
            self.assertIn("printer", (output / "full-contracts.json").read_text())
            self.assertNotIn("printer", (output / "selective-contracts.json").read_text())
            self.assertEqual(json.loads((output / "evidence.json").read_text()), evidence)


if __name__ == "__main__":
    unittest.main()
