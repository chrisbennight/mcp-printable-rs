"""Reproducible CAD workflow evidence using a public, generated board fixture.

Run with the pinned CAD environment: python cad/benchmark.py OUTPUT.json
Measures adapter time within one process, not MCP transport or model tokens.
"""

import json
import platform
from pathlib import Path
import sys
import tempfile
import time

from build import build


SOURCE = '''result = cq.Assembly(name="board")
result.add(cq.Workplane().box(100, 70, 1.6), name="PCB", color=cq.Color("green"))
connector = cq.Workplane().box(12, 16, 6).faces(">Z").workplane().hole(3)
result.add(connector, name="USB_left", loc=cq.Location((30, 28, 4), (0,0,1), 90))
result.add(connector, name="USB_right", loc=cq.Location((-30, 28, 4)))
for i in range(24):
    result.add(cq.Workplane().box(5, 5, 2), name=f"chip_{i}", loc=cq.Location((-35+(i%8)*10, -20+(i//8)*12, 2)))
'''


def measure(root, name, request):
    directory = root / name
    directory.mkdir()
    started = time.perf_counter()
    build(request, directory)
    elapsed = time.perf_counter() - started
    report = json.loads((directory / "output/report.json").read_text())
    return {"seconds": elapsed, "report_json_bytes": len(json.dumps(report).encode()),
            "output_bytes": sum(path.stat().st_size for path in (directory / "output").iterdir())}, report, directory


def run():
    import cadquery as cq
    import OCP
    results = {"environment":{"system":platform.system(), "architecture":platform.machine(), "python":platform.python_version(), "cadquery":cq.__version__, "ocp":OCP.__version__}, "boundary":"native adapter in one warm Python process; excludes worker startup, MCP and download time",
               "model_tokens":None, "peak_memory_bytes":None,
               "fixture":"generated board, placed repeated connectors and discrete components; no private vendor data",
               "sources":{}}
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        source = root / "board.py"
        source.write_text(SOURCE)
        common = {"parameters":{}, "linear_tolerance_mm":0.05, "angular_tolerance_rad":0.1}
        baseline, _, _ = measure(root, "full", {**common, "action":"model", "source":str(source)})
        # Re-import an exchange assembly declaring inches to cover native unit conversion.
        namespace = {"cq":cq}
        # Execute this fixed, repository-owned fixture through the same script path.
        import runpy
        assembly = runpy.run_path(str(source), init_globals=namespace)["result"]
        step = root / "vendor.step"
        assembly.export(str(step), exportType="STEP", unit="MM", outputUnit="INCH")
        step_baseline, _, _ = measure(root, "full-step", {**common, "action":"import_step", "source":str(step)})
        for kind, path, original in (("generated", source, baseline), ("step", step, step_baseline)):
            no_retention, _, _ = measure(root, f"inspect-only-{kind}", {**common, "action":"inspect", "source":str(path),
                "inspection":{"selection":{"name_contains":"USB"}, "limit":1, "retain_model":False}})
            initial, report, directory = measure(root, f"inspect-{kind}", {**common, "action":"inspect", "source":str(path),
                "inspection":{"selection":{"name_contains":"USB"}, "limit":1}})
            identity = report["components"][0]["id"]
            model = directory / "output/model.cadmodel"
            followup, measured, _ = measure(root, f"followup-{kind}", {"action":"inspect", "source":str(model), "reuse_model":True,
                "inspection":{"selection":{"component_ids":[identity]}, "questions":[
                    {"id":"features", "kind":"features", "component_id":identity},
                    {"id":"valid", "kind":"validity", "component_id":identity}]}})
            exported, report, _ = measure(root, f"export-{kind}", {"action":"export", "source":str(model), "reuse_model":True,
                "inspection":{"selection":{"component_ids":[identity]}, "exports":["step"]}})
            assert measured["answers"][1]["evidence"]["valid"]
            assert report["exports"][0]["status"] == "completed"
            results["sources"][kind] = {"full_build":original, "first_inspection_with_retention":initial,
                "first_inspection_without_retention":no_retention,
                "reused_measurements":followup, "reused_selected_step":exported,
                "native_model_bytes":model.stat().st_size,
                "cad_calls_for_inspect_followup_export":3, "task_specific_scripts_required":0}
    return results


if __name__ == "__main__":
    destination = Path(sys.argv[1])
    with destination.open("x") as stream:
        json.dump(run(), stream, indent=2)
        stream.write("\n")
