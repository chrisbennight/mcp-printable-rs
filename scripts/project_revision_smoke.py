"""Verify retained design requirements against native CAD through public MCP."""

import base64
import copy
import hashlib
import json


def run(client, project, output):
    def call(tool, action, params):
        return client.call(tool, {"action": action, "params": params})

    source = b'''points = [(-12 + parameters["shift"], -8), (-12, 8), (12, -8), (12, 8)]
result = cq.Workplane("XY").box(parameters["width"], 30, 5).faces(">Z").workplane().pushPoints(points).hole(4)
'''
    path = project["root"] + "/revision-part.py"
    call("artifact", "write", {"path": path, "data_base64": base64.b64encode(source).decode()})
    manifest = {"format_version": 1, "units": "mm", "parameters": {
        "width": {"value": 40, "unit": "mm", "minimum": 30, "maximum": 80, "description": "Enclosure width"},
        "shift": {"value": 0, "unit": "mm", "minimum": -5, "maximum": 5, "description": "First hole offset"}},
        "requirements": {
            "mounts": {"kind": "hole_pattern", "centers_mm": [[-12, -8], [-12, 8], [12, -8], [12, 8]], "radius_mm": 2, "tolerance_mm": 0.05},
            "envelope": {"kind": "build_envelope", "size_mm": [80, 40, 20]},
            "fit": {"kind": "clearance", "minimum_mm": 0.3, "description": "Lid clearance requires assembly evidence"}},
        "assumptions": ["Prototype geometry; physical fit remains untested"]}
    parent = None
    retained = []
    for label, width, shift, expected in [("initial", 40, 0, "passed"), ("wider", 60, 0, "passed"), ("moved-hole", 60, 2, "failed")]:
        manifest = copy.deepcopy(manifest)
        manifest["parameters"]["width"]["value"] = width
        manifest["parameters"]["shift"]["value"] = shift
        revision = call("project", "revise", {"project_id": project["project_id"], "expected_parent": parent,
            "source": "revision-part.py", "expected_source_sha256": hashlib.sha256(source).hexdigest(), "manifest": manifest})
        assert revision["revision"]["parent"] == parent
        parent = revision["identity"]
        if label == "moved-hole":
            call("artifact", "write", {"path": path, "data_base64": base64.b64encode(b"raise RuntimeError('replaced source')").decode(), "overwrite": True})
        result = call("cad_build", "model", {"project_id": project["project_id"], "source": "revision-part.py",
            "parameters": {"width": width, "shift": shift}, "output_dir": "builds/revision-" + label, "revision": parent})
        assert result["report"]["valid"]
        assert result["requirements"]["criteria"]["mounts"]["status"] == expected, result["requirements"]
        assert result["requirements"]["criteria"]["fit"]["status"] == "unmeasured"
        assert result["revision"] == parent
        measured = output / ("revision-" + label + ".json")
        client.download(result["measurement"]["path"], measured)
        assert hashlib.sha256(measured.read_bytes()).hexdigest() == result["measurement"]["sha256"]
        assert json.loads(measured.read_text())["revision"] == parent
        reopened = call("project", "revision", {"project_id": project["project_id"], "revision": parent})
        assert reopened["revision"] == revision["revision"]
        retained.append({"identity": parent, "measurement": result["measurement"], "requirements": result["requirements"]})
    (output / "revision-evidence.json").write_text(json.dumps(retained, indent=2) + "\n")
    print("PROJECT_REVISION_OK: retained source, wider enclosure, preserved grid, detected moved hole")
