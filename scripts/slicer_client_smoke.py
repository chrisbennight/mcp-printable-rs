"""Exercise CAD-to-slice preparation and verified downloads through public MCP."""

import base64
import hashlib
import json
import time
import uuid
import zipfile


def run(client, output):
    output.mkdir(mode=0o700)

    def call(tool, action, params):
        return client.call(tool, {"action": action, "params": params})

    def load_retained(reference, name):
        destination = output / name
        client.download(reference["path"], destination)
        data = destination.read_bytes()
        if hashlib.sha256(data).hexdigest() != reference["sha256"]:
            raise ValueError("Retained delivery evidence changed")
        return json.loads(data)

    project_id = "slice-smoke-" + uuid.uuid4().hex
    project = call("project", "create", {"project_id": project_id, "name": "Slice integration"})
    source = b"result = cq.Workplane().box(10, 10, 3)\n"
    call("artifact", "write", {"path": project["root"] + "/part.py",
                               "data_base64": base64.b64encode(source).decode("ascii")})
    revision = call("project", "revise", {"project_id": project_id, "expected_parent": None,
        "source": "part.py", "expected_source_sha256": hashlib.sha256(source).hexdigest(),
        "manifest": {"format_version": 1, "units": "mm", "requirements": {
            "envelope": {"kind": "build_envelope", "size_mm": [256, 256, 256]}}}})
    cad = call("cad_build", "model", {"project_id": project_id, "source": "part.py", "output_dir": "cad",
        "revision": revision["identity"], "qualification": {
            "policy": "printable_part", "dimensions_mm": [10, 10, 3], "solid_count": 1}})
    measurement = load_retained(cad["measurement"], "cad-measurement.json")
    if (measurement["revision"] != revision["identity"] or measurement["qualification"]["status"] != "passed"
            or measurement["requirements"]["criteria"]["envelope"]["status"] != "passed"):
        raise ValueError("Native CAD measurement did not retain its revision and scoped requirements")
    printer = "Bambu Lab X1 Carbon 0.4 nozzle"
    profiles = call("slice", "profiles", {"category": "process", "printer": printer,
                                           "query": "0.20mm Standard"})
    if profiles["total"] < 1:
        raise ValueError("Native process profiles are unavailable through MCP")
    handle = {"project_id": project_id, "output_dir": "slice"}
    state = call("slice", "prepare", {**handle, "source": "cad/model.stl",
        "design_record": cad["measurement"],
        "build_plate": "Textured PEI Plate", "printer": {"name": printer},
        "process": {"name": "0.20mm Standard @BBL X1C"},
        "filaments": [{"name": "Bambu PLA Basic @BBL X1C"}],
        "auto_arrange": True, "timeout_seconds": 300})
    deadline = time.monotonic() + 360
    while state["status"] == "running" and time.monotonic() < deadline:
        time.sleep(0.5)
        state = call("slice", "status", handle)
    if state["status"] != "completed":
        raise ValueError("Public MCP slice did not complete")
    if state["setup"]["build_plate"] != "Textured PEI Plate":
        raise ValueError("Slice did not retain the selected build surface")
    manifest = load_retained(state["provenance"], "slice-provenance.json")
    design = manifest["data"]["design_evidence"]
    if (design["status"] != "matched_cad_artifact" or design["record"] != cad["measurement"]
            or design["revision"] != revision["identity"] or design["qualification"] != cad["qualification"]
            or design["requirements"] != cad["requirements"]):
        raise ValueError("Native slice lost the selected CAD measurement evidence")
    (output / "state.json").write_text(json.dumps(state))
    (output / "handle.json").write_text(json.dumps(handle))
    verify_restored(client, output, "initial")
    toolpath = next(name for name in state["artifacts"] if name.endswith(".gcode"))
    review = call("slice", "review", {"slice": handle, "toolpath": toolpath,
                                       "first_layer": 1, "last_layer": 1})
    if (review["kind"] != "actual_toolpath" or review["segments"] <= 0
            or review["source_sha256"] != state["artifacts"][toolpath]["sha256"]):
        raise ValueError("Review does not identify the completed toolpath")
    image = output / "toolpath.png"
    client.download(review["image"]["path"], image)
    if not image.read_bytes().startswith(b"\x89PNG\r\n\x1a\n"):
        raise ValueError("Toolpath download is not a PNG")
    client.download(review["metadata_path"], output / "review.json")
    retained = json.loads((output / "review.json").read_text())
    if retained != {key: value for key, value in review.items() if key != "metadata_path"}:
        raise ValueError("Retained review differs from the public response")
    reviewed = load_retained(review["provenance"], "review-provenance.json")["data"]
    if (reviewed["slice_provenance"] != state["provenance"]
            or reviewed["source_sha256"] != state["artifacts"][toolpath]["sha256"]):
        raise ValueError("Toolpath review lost the native slice identity")
    retained_image = output / "retained-toolpath.png"
    client.download(reviewed["image"]["artifact"]["path"], retained_image)
    if (hashlib.sha256(retained_image.read_bytes()).hexdigest() != reviewed["image"]["sha256"]
            or retained_image.read_bytes() != image.read_bytes()):
        raise ValueError("Retained toolpath preview does not match the reviewed image")
    print("SLICER_CLIENT_OK: native preparation, hashed downloads, toolpath review", flush=True)


def verify_restored(client, output, phase="restored"):
    handle = json.loads((output / "handle.json").read_text())
    original = json.loads((output / "state.json").read_text())
    state = client.call("slice", {"action": "status", "params": handle})
    if (state["status"] != "completed" or state["artifacts"] != original["artifacts"]
            or state["provenance"] != original["provenance"]):
        raise ValueError("Completed slice artifacts changed after recovery")
    downloads = output / phase
    downloads.mkdir(mode=0o700)
    for index, (name, item) in enumerate(state["artifacts"].items()):
        destination = downloads / f"artifact-{index}"
        client.download(item["artifact"]["path"], destination)
        with destination.open("rb") as stream:
            if hashlib.file_digest(stream, "sha256").hexdigest() != item["sha256"]:
                raise ValueError("Downloaded slice artifact differs from its recorded digest")
        if name == "model.gcode.3mf":
            with zipfile.ZipFile(destination) as package:
                if package.testzip() is not None or not any(path.endswith(".gcode") for path in package.namelist()):
                    raise ValueError("Printer-ready package is corrupt or lacks G-code")
    print("SLICER_ARTIFACTS_OK: completed status and retained artifact digests", flush=True)
