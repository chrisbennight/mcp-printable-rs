"""Compare direct-client discovery on an isolated modeling fixture, without a model."""

import argparse
import hashlib
import json
from pathlib import Path
import platform
import time
import uuid

from printable_client import CLIENT_INFO, Client


def encoded_size(value):
    return len(json.dumps(value, separators=(",", ":"), ensure_ascii=False).encode("utf-8"))


def run(client, output, runtime=None):
    output.mkdir(mode=0o700)
    results = []
    evidence = {
        "client": CLIENT_INFO, "python": platform.python_version(),
        "client_source_sha256": hashlib.sha256(Path(__file__).with_name("printable_client.py").read_bytes()).hexdigest(),
        "server": client.server_info, "protocol": client.protocol,
        "runtime": runtime, "model": None, "model_usage": None,
        "usage_unavailable_reason": "Direct scripted client; no model is invoked and no provider usage is reported.",
        "measurement_scope": "Compact UTF-8 JSON of decoded results; excludes JSON-RPC envelopes, HTTP framing, and server instructions. Bytes are not tokens.",
        "latency_scope": "Single ordered full/selective samples including local client work; uncontrolled cache effects, not a performance comparison.",
        "results": results,
    }
    root = "acceptance/discovery-" + uuid.uuid4().hex
    for mode in ("full", "selective"):
        calls = []
        rpc = client.rpc
        exposed = None
        passed = False

        def observed(method, params=None, notification=False):
            started = time.perf_counter()
            record = {"method": method, "params": params, "outcome": "response_unavailable"}
            calls.append(record)
            try:
                result = rpc(method, params, notification=notification)
                record.update(outcome="received", result=result, result_json_bytes=encoded_size(result))
                return result
            finally:
                record["elapsed_ms"] = (time.perf_counter() - started) * 1000

        client.rpc = observed
        try:
            if mode == "full":
                exposed = client.tools()
                if not {"scad_build", "validate_mesh"}.issubset(
                        {tool["name"] for tool in exposed["tools"]}):
                    raise ValueError("Full discovery omitted the fixture tools")
            else:
                index = client.contracts()
                if not {"scad_build", "validate_mesh"}.issubset(
                        {tool["tool"] for tool in index["tools"]}):
                    raise ValueError("Contract index omitted the fixture tools")
                exposed = {"index": index, "contracts": [
                    client.contracts("scad_build", "mesh"), client.contracts("validate_mesh")]}
            path = root + "/" + mode + ".stl"
            compiled = client.call("scad_build", {"action": "mesh", "params": {
                "source": "cube([10,20,30]);", "path": path}})
            measured = client.call("validate_mesh", {"path": path})
            if (compiled["artifact"]["path"] != path
                    or not measured["report"]["solid_geometry"]
                    or measured["report"]["bounds"]["dimensions_mm"] != [10, 20, 30]):
                raise ValueError("Discovery fixture did not produce the requested solid")
            if mode == "selective" and any(call["method"] == "tools/list" for call in calls):
                raise ValueError("Selective discovery unexpectedly loaded the full catalog")
            passed = True
        finally:
            client.rpc = rpc
            (output / (mode + "-contracts.json")).write_text(json.dumps(exposed, indent=2))
            (output / (mode + "-calls.json")).write_text(json.dumps(calls, indent=2))
            results.append({"mode": mode, "outcome": "passed" if passed else "failed", "rpc_calls": len(calls),
                            "exposed_contract_json_bytes": encoded_size(exposed) if exposed is not None else None,
                            "received_result_json_bytes": sum(call.get("result_json_bytes", 0) for call in calls),
                            "elapsed_ms": sum(call["elapsed_ms"] for call in calls),
                            "inline_image_bytes": 0})
            (output / "evidence.json").write_text(json.dumps(evidence, indent=2))
    print("SELECTIVE_CLIENT_OK: full and selected discovery produced the requested solid; model usage unavailable")
    return evidence


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--url", default="http://127.0.0.1:8000/mcp")
    parser.add_argument("--bearer-file", type=Path, default=Path(".dev/mcp-bearer"))
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    with Client(args.url, args.bearer_file) as client:
        run(client, args.output)


if __name__ == "__main__":
    main()
