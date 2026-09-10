"""External protocol smoke for a running headless Blender bridge."""

from __future__ import annotations

import argparse
import json
import math
import socket
import struct
import sys
import time
import uuid
from typing import Any


def receive_exact(connection: socket.socket, length: int) -> bytes:
    chunks: list[bytes] = []
    while length:
        chunk = connection.recv(length)
        if not chunk:
            raise RuntimeError("bridge closed before returning a response")
        chunks.append(chunk)
        length -= len(chunk)
    return b"".join(chunks)


def command(
    host: str,
    port: int,
    name: str,
    params: dict[str, Any],
) -> Any:
    request_id = str(uuid.uuid4())
    payload = json.dumps(
        {"id": request_id, "command": name, "params": params},
        separators=(",", ":"),
    ).encode("utf-8")
    with socket.create_connection((host, port), timeout=10) as connection:
        work_budget = params.get("timeout_seconds", 0)
        if isinstance(work_budget, bool) or not isinstance(work_budget, (int, float)):
            work_budget = 0
        connection.settimeout(150 + max(0, work_budget))
        connection.sendall(struct.pack(">I", len(payload)) + payload)
        (length,) = struct.unpack(">I", receive_exact(connection, 4))
        if length > 64 * 1024 * 1024:
            raise RuntimeError("bridge returned an oversized response")
        response = json.loads(receive_exact(connection, length).decode("utf-8"))
    if response.get("id") != request_id:
        raise RuntimeError("bridge returned a mismatched correlation id")
    if response.get("status") == "error":
        raise RuntimeError(response.get("error", "bridge command failed"))
    if response.get("status") != "success" or "result" not in response:
        raise RuntimeError("bridge returned a malformed response")
    return response["result"]


def wait_until_ready(host: str, port: int) -> None:
    deadline = time.monotonic() + 120
    while time.monotonic() < deadline:
        try:
            command(host, port, "bridge_status", {})
            return
        except (OSError, RuntimeError):
            time.sleep(0.5)
    raise RuntimeError("bridge did not become ready")


def clear_scene(host: str, port: int, minimum_removed: int) -> None:
    cleared = command(host, port, "clear_scene", {})
    removed = cleared.get("removed_objects")
    if isinstance(removed, bool) or not isinstance(removed, int):
        raise RuntimeError("scene clear returned an invalid removal count")
    if removed < minimum_removed:
        raise RuntimeError("scene clear did not remove the expected objects")
    scene = command(host, port, "get_scene_info", {})
    if scene.get("objects") != [] or scene.get("active_object") is not None:
        raise RuntimeError("scene clear left objects behind")


def mechanical_export_bounds(host: str, port: int) -> dict[str, Any]:
    inspected = command(
        host,
        port,
        "execute_code",
        {
            "code": (
                "from pathlib import Path\n"
                "import math\n"
                "import struct\n"
                "artifacts = {}\n"
                "for label, relative in (\n"
                "    ('fixed', '.printable/jobs/smoke/analysis/fixed.stl'),\n"
                "    ('moving', '.printable/jobs/smoke/analysis/moving.stl'),\n"
                "):\n"
                "    data = Path(workspace_root, relative).read_bytes()\n"
                "    if len(data) < 84:\n"
                "        raise RuntimeError(f'{label} export is not binary STL')\n"
                "    triangles = struct.unpack_from('<I', data, 80)[0]\n"
                "    if triangles == 0 or len(data) != 84 + triangles * 50:\n"
                "        raise RuntimeError(f'{label} export has invalid binary STL framing')\n"
                "    vertices = [\n"
                "        struct.unpack_from('<3f', data, 84 + triangle * 50 + 12 + vertex * 12)\n"
                "        for triangle in range(triangles)\n"
                "        for vertex in range(3)\n"
                "    ]\n"
                "    if not all(math.isfinite(value) for vertex in vertices for value in vertex):\n"
                "        raise RuntimeError(f'{label} export contains non-finite coordinates')\n"
                "    artifacts[label] = {\n"
                "        'triangles': triangles,\n"
                "        'minimum': [min(vertex[axis] for vertex in vertices) for axis in range(3)],\n"
                "        'maximum': [max(vertex[axis] for vertex in vertices) for axis in range(3)],\n"
                "    }\n"
                "result = artifacts\n"
            )
        },
    ).get("result")
    if not isinstance(inspected, dict):
        raise RuntimeError("mechanical STL inspection returned no artifact bounds")
    return inspected


def assert_pid_file_process_absent(
    host: str,
    port: int,
    pid_file: str,
) -> None:
    for _ in range(50):
        descendant = command(
            host,
            port,
            "execute_code",
            {
                "code": (
                    "from pathlib import Path\n"
                    f"pid = Path(workspace_root, 'smoke/{pid_file}').read_text(encoding='utf-8')\n"
                    "result = not Path('/proc', pid).exists()\n"
                ),
                "timeout_seconds": 10.0,
            },
        )
        if descendant.get("result") is True:
            return
        time.sleep(0.1)
    raise RuntimeError("caller background process survived execution")


def assert_timeout_cleans_descendant(
    host: str,
    port: int,
    pid_file: str,
    timeout_seconds: float,
    disable_in_process_guard: bool,
) -> None:
    guard_bypass = ""
    if disable_in_process_guard:
        guard_bypass = (
            "os._exit = lambda _code: None\n"
            "sys.settrace(None)\n"
            "sys.setswitchinterval(600.0)\n"
        )
    try:
        command(
            host,
            port,
            "execute_code",
            {
                "code": (
                    "import os\n"
                    "import subprocess\n"
                    "import sys\n"
                    "from pathlib import Path\n"
                    "descendant = subprocess.Popen(\n"
                    "    ['/usr/bin/python3', '-c', 'while True: pass'],\n"
                    "    close_fds=True,\n"
                    "    start_new_session=True,\n"
                    ")\n"
                    f"Path(workspace_root, 'smoke/{pid_file}').write_text(\n"
                    "    str(descendant.pid), encoding='utf-8'\n"
                    ")\n"
                    f"{guard_bypass}"
                    "try:\n"
                    "    while True:\n"
                    "        pass\n"
                    "except BaseException:\n"
                    "    while True:\n"
                    "        pass\n"
                ),
                "timeout_seconds": timeout_seconds,
            },
        )
    except (OSError, RuntimeError):
        pass
    else:
        raise RuntimeError("caller code consumed the execution watchdog")
    wait_until_ready(host, port)
    assert_pid_file_process_absent(host, port, pid_file)


def assert_background_work_is_contained(host: str, port: int) -> None:
    try:
        command(
            host,
            port,
            "execute_code",
            {
                "code": (
                    "import os\n"
                    "import threading\n"
                    "import time\n"
                    "from pathlib import Path\n"
                    "Path(workspace_root, 'smoke/thread-blender.pid').write_text(\n"
                    "    str(os.getpid()), encoding='utf-8'\n"
                    ")\n"
                    "threading.Thread(\n"
                    "    target=lambda: time.sleep(600), daemon=True\n"
                    ").start()\n"
                    "result = 'returned'\n"
                ),
                "timeout_seconds": 5.0,
            },
        )
    except (OSError, RuntimeError):
        pass
    else:
        raise RuntimeError("background Python thread returned a success response")
    wait_until_ready(host, port)
    restarted = command(
        host,
        port,
        "execute_code",
        {
            "code": (
                "import os\n"
                "from pathlib import Path\n"
                "previous = Path(\n"
                "    workspace_root, 'smoke/thread-blender.pid'\n"
                ").read_text(encoding='utf-8')\n"
                "result = os.getpid() != int(previous)\n"
            ),
            "timeout_seconds": 10.0,
        },
    )
    if restarted.get("result") is not True:
        raise RuntimeError("background Python thread did not restart Blender")

    try:
        command(
            host,
            port,
            "execute_code",
            {
                "code": (
                    "import subprocess\n"
                    "import sys\n"
                    "from pathlib import Path\n"
                    "child = subprocess.Popen(\n"
                    "    [sys.executable, '-c', 'while True: pass'],\n"
                    "    close_fds=True,\n"
                    "    start_new_session=True,\n"
                    ")\n"
                    "Path(workspace_root, 'smoke/background-process.pid').write_text(\n"
                    "    str(child.pid), encoding='utf-8'\n"
                    ")\n"
                    "result = 'returned'\n"
                ),
                "timeout_seconds": 5.0,
            },
        )
    except RuntimeError as error:
        if "background processes" not in str(error):
            raise
    else:
        raise RuntimeError("background process returned a success response")
    assert_pid_file_process_absent(host, port, "background-process.pid")


def scene_state_smoke(host: str, port: int) -> None:
    before = command(host, port, "get_scene_info", {}).get("scene_state")
    if not isinstance(before, dict):
        raise RuntimeError("Blender observation omitted scene state")
    command(host, port, "save_blend", {"path": "smoke/state-before.blend", "expected_scene": before})
    created = command(host, port, "create_primitive", {
        "primitive": "cube", "name": "StateGuardProbe", "expected_scene": before,
    })
    after = created.get("scene_state")
    if not isinstance(after, dict) or after == before:
        raise RuntimeError("Blender mutation did not advance scene state")
    try:
        command(host, port, "rename_object", {
            "name": "StateGuardProbe", "new_name": "UnexpectedStaleEdit",
            "expected_scene": before,
        })
    except RuntimeError as error:
        if "expected_scene is stale" not in str(error):
            raise
    else:
        raise RuntimeError("stale scene mutation was accepted")
    command(host, port, "get_object_info", {"name": "StateGuardProbe", "expected_scene": after})
    restored = command(host, port, "restore_checkpoint", {
        "path": "smoke/state-before.blend", "expected_scene": after,
    }).get("scene_state")
    if not isinstance(restored, dict) or restored["generation"] == after["generation"]:
        raise RuntimeError("checkpoint restore reused the old scene generation")


def native_observation_smoke(host: str, port: int) -> None:
    command(host, port, "save_blend", {"path": "smoke/before-native.blend"})
    try:
        command(host, port, "execute_code", {"code": """
bpy.ops.mesh.primitive_cube_add(size=2)
obj = bpy.context.object
obj.name = 'NativeProbe'
obj.scale = (1, 2, 3)
material = bpy.data.materials.new('NativeProbeMaterial')
material.use_nodes = True
obj.data.materials.append(material)
result = True
"""})
        scene_state = command(host, port, "get_scene_info", {})["scene_state"]
        captures = []
        for axis in ("FRONT", "RIGHT"):
            observed = command(host, port, "capture_native_view", {
                "path": f"smoke/native-{axis.lower()}.png", "max_size": 640,
                "expected_scene": scene_state,
                "view": {"axis": axis, "location": [0, 0, 0], "distance": 12,
                         "perspective": "ORTHO", "shading": "SOLID", "overlays": True},
            })
            if observed["method"] != "viewport" or observed["scene_state"] != scene_state:
                raise RuntimeError("native view changed model state or substituted capture method")
            if max(observed["width"], observed["height"]) > 640:
                raise RuntimeError("native view exceeded its image budget")
            captures.append(observed)
        if captures[0]["sha256"] == captures[1]["sha256"]:
            raise RuntimeError("asymmetric model did not produce different native views")
        command(host, port, "execute_code", {
            "context": {"area_type": "VIEW_3D"},
            "code": "bpy.ops.object.mode_set(mode='EDIT'); result = bpy.context.mode",
        })
        editing = command(host, port, "capture_native_view", {
            "path": "smoke/native-edit.png", "method": "editor", "max_size": 640,
        })
        if editing["mode"] != "EDIT_MESH" or editing["method"] != "editor":
            raise RuntimeError("editor capture omitted actual mesh editing state")
        command(host, port, "execute_code", {"code": """
bpy.ops.object.mode_set(mode='OBJECT')
area = next(a for a in bpy.context.window_manager.windows[0].screen.areas if a.type == 'VIEW_3D')
bpy.app.driver_namespace['printable_native_probe_editor'] = (area, area.type, area.ui_type)
area.type = 'NODE_EDITOR'
area.ui_type = 'ShaderNodeTree'
result = True
"""})
        command(host, port, "execute_code", {
            "context": {"area_type": "NODE_EDITOR"},
            "code": "bpy.ops.node.view_all(); result = bpy.context.space_data.edit_tree.name",
        })
        nodes = command(host, port, "capture_native_view", {
            "path": "smoke/native-nodes.png", "method": "editor", "max_size": 640,
            "context": {"area_type": "NODE_EDITOR"},
        })
        if nodes["area_type"] != "NODE_EDITOR" or not nodes["view_configuration"]["node_tree"]:
            raise RuntimeError("native node editor capture did not identify its displayed tree")
        pixels = command(host, port, "execute_code", {"code": """
from array import array
from pathlib import Path
observations = []
for path in ('smoke/native-edit.png', 'smoke/native-nodes.png'):
    image = bpy.data.images.load(str(Path(workspace_root) / path), check_existing=False)
    try:
        pixels = array('f', [0.0]) * len(image.pixels)
        image.pixels.foreach_get(pixels)
        distinct = len({tuple(pixels[index:index + 3]) for index in range(0, len(pixels), 148)})
        if distinct < 8:
            raise RuntimeError('native editor fixture returned blank or missing UI pixels')
        observations.append({'path': path, 'distinct_rgb_samples': distinct})
    finally:
        bpy.data.images.remove(image)
result = {'observations': observations, 'startup_splash_disabled': not bpy.context.preferences.view.show_splash}
"""})["result"]
        if not pixels["startup_splash_disabled"]:
            raise RuntimeError("native UI runtime leaves the startup splash enabled")
        print(json.dumps({"native_observation": {"views_differ": True, "edit_mode": editing["mode"],
                                                "node_editor": nodes["area_type"], "pixels": pixels}}))
    finally:
        command(host, port, "execute_code", {"code": """
saved = bpy.app.driver_namespace.pop('printable_native_probe_editor', None)
if saved is not None:
    area, area_type, ui_type = saved
    area.type = area_type
    area.ui_type = ui_type
result = True
"""})
        command(host, port, "restore_checkpoint", {"path": "smoke/before-native.blend"})


def capability_smoke(host: str, port: int) -> None:
    wait_until_ready(host, port)
    scene_state_smoke(host, port)
    editing = command(host, port, "get_editing_state", {"section": "editors", "limit": 2})
    if len(editing.get("items", [])) > 2 or not isinstance(editing.get("mode"), str):
        raise RuntimeError("editing-state inspection violated its bounded contract")
    ui = command(host, port, "execute_code", {"code": "result = not bpy.app.background"})["result"]
    if ui:
        contextual = command(host, port, "execute_code", {
            "context": {"area_type": "VIEW_3D"},
            "code": "result = {'area': bpy.context.area.type, 'region': bpy.context.region.type}",
        })
        if contextual["result"] != {"area": "VIEW_3D", "region": "WINDOW"}:
            raise RuntimeError("execution did not use its explicit Blender editor")
        if contextual["context"]["before"]["area_type"] != "VIEW_3D":
            raise RuntimeError("execution omitted its actual editor context")
        native_observation_smoke(host, port)
    status = command(host, port, "bridge_status", {})
    if not str(status["blender_version"]).startswith("5.2.0"):
        raise RuntimeError("bridge is not running Blender 5.2.0")
    if "execute_code" not in status.get("commands", []):
        raise RuntimeError("bridge does not advertise explicit code execution")
    if "animate_rotation" not in status.get("commands", []):
        raise RuntimeError("bridge does not advertise rigid rotation authoring")
    if "job_prepare_mechanical_rotation" not in status.get("commands", []):
        raise RuntimeError("bridge does not advertise mechanical job preparation")
    if "render_product" not in status.get("commands", []):
        raise RuntimeError("bridge does not advertise product presentation")
    limits = status.get("execution_limits", {})
    if limits.get("default_timeout_seconds") != 120:
        raise RuntimeError("bridge does not report the execution default")
    if (
        limits.get("timeout_policy")
        != "caller-selected positive runtime-representable seconds; "
        "no configured maximum"
    ):
        raise RuntimeError("bridge does not report caller-selected budgets")
    if limits.get("timeout_recovery") != (
        "watchdog restart; persistent unhealthy state requires container restart"
    ):
        raise RuntimeError("bridge does not report watchdog restart recovery")
    if limits.get("output_capture") != (
        "Python, native, and inherited subprocess stdout/stderr"
    ):
        raise RuntimeError("bridge does not report descriptor output capture")
    if (
        limits.get("background_work_policy")
        != "must finish before execute_code returns"
    ):
        raise RuntimeError("bridge does not report synchronous execution policy")
    if limits.get("timeout_outcome") != (
        "unknown after request delivery; never automatically retry"
    ):
        raise RuntimeError("bridge does not report ambiguous timeout outcomes")
    clear_scene(host, port, 0)
    command(
        host,
        port,
        "create_primitive",
        {"primitive": "cube", "name": "MechanicalBase", "size": 2.0},
    )
    command(
        host,
        port,
        "create_primitive",
        {"primitive": "cube", "name": "MechanicalLeaf", "size": 1.0},
    )
    command(
        host,
        port,
        "execute_code",
        {
            "code": (
                "bpy.data.objects['MechanicalLeaf'].location.x = 4.0\n"
                "result = 'positioned'\n"
            )
        },
    )
    prepared = command(
        host,
        port,
        "job_prepare_mechanical_rotation",
        {
            "fixed_objects": ["MechanicalBase"],
            "moving_objects": ["MechanicalLeaf"],
            "controller_name": "MechanicalPivot",
            "pivot": [3.0, 0.0, 0.0],
            "axis": [0.0, 0.0, 1.0],
            "angle_degrees": 90.0,
            "frame_start": 1,
            "frame_end": 3,
            "fixed_path": ".printable/jobs/smoke/analysis/fixed.stl",
            "moving_path": ".printable/jobs/smoke/analysis/moving.stl",
            "max_output_bytes": 1024 * 1024,
            "timeout_seconds": 120.0,
        },
    )
    if (
        prepared.get("fixed_path")
        != ".printable/jobs/smoke/analysis/fixed.stl"
        or prepared.get("moving_path")
        != ".printable/jobs/smoke/analysis/moving.stl"
        or prepared.get("motion", {}).get("interpolation") != "LINEAR"
        or prepared.get("fixed_size_bytes", 0) <= 0
        or prepared.get("moving_size_bytes", 0) <= 0
    ):
        raise RuntimeError("mechanical job preparation returned an invalid contract")
    exported = mechanical_export_bounds(host, port)
    expected_bounds = {
        "fixed": ([-1.0, -1.0, -1.0], [1.0, 1.0, 1.0]),
        "moving": ([3.5, -0.5, -0.5], [4.5, 0.5, 0.5]),
    }
    for label, (expected_minimum, expected_maximum) in expected_bounds.items():
        artifact = exported.get(label)
        if not isinstance(artifact, dict) or artifact.get("triangles") != 12:
            raise RuntimeError(f"{label} mechanical export has the wrong mesh")
        for field, expected in (
            ("minimum", expected_minimum),
            ("maximum", expected_maximum),
        ):
            actual = artifact.get(field)
            if (
                not isinstance(actual, list)
                or len(actual) != 3
                or any(
                    isinstance(value, bool)
                    or not isinstance(value, (int, float))
                    or abs(float(value) - target) > 1e-6
                    for value, target in zip(actual, expected)
                )
            ):
                raise RuntimeError(
                    f"{label} mechanical export is not in shared world coordinates: {exported}"
                )
    clear_scene(host, port, 3)
    created = command(
        host,
        port,
        "create_primitive",
        {"primitive": "cube", "name": "SmokeCube", "size": 2.0},
    )
    if created.get("vertices") != 8:
        raise RuntimeError("cube creation did not produce eight vertices")
    renamed = command(
        host,
        port,
        "rename_object",
        {"name": "SmokeCube", "new_name": "SmokeBase"},
    )
    if renamed.get("name") != "SmokeBase":
        raise RuntimeError("object rename did not return the renamed cube")
    animated = command(
        host,
        port,
        "animate_rotation",
        {
            "objects": ["SmokeBase"],
            "controller_name": "SmokeHingePivot",
            "pivot": [1.0, 0.0, 0.0],
            "axis": [0.0, 0.0, 1.0],
            "angle_degrees": 90.0,
            "frame_start": 1,
            "frame_end": 3,
        },
    )
    if (
        animated.get("controller") != "SmokeHingePivot"
        or animated.get("objects") != ["SmokeBase"]
        or animated.get("interpolation") != "LINEAR"
    ):
        raise RuntimeError("rigid rotation authoring returned an invalid contract")
    motion = command(
        host,
        port,
        "execute_code",
        {
            "code": (
                "scene = bpy.context.scene\n"
                "original_frame = scene.frame_current\n"
                "obj = bpy.data.objects['SmokeBase']\n"
                "positions = []\n"
                "for frame in (1, 2, 3):\n"
                "    scene.frame_set(frame)\n"
                "    positions.append([float(value) for value in obj.matrix_world.translation])\n"
                "scene.frame_set(original_frame)\n"
                "result = {'positions': positions}\n"
            )
        },
    ).get("result", {})
    positions = motion.get("positions") if isinstance(motion, dict) else None
    expected_positions = (
        (0.0, 0.0, 0.0),
        (1.0 - 2.0**-0.5, -(2.0**-0.5), 0.0),
        (1.0, -1.0, 0.0),
    )
    if not isinstance(positions, list) or len(positions) != 3:
        raise RuntimeError("rigid rotation smoke returned no evaluated positions")
    for actual, expected in zip(positions, expected_positions):
        if (
            not isinstance(actual, list)
            or len(actual) != 3
            or any(
                isinstance(observed, bool)
                or not isinstance(observed, (int, float))
                or abs(float(observed) - target) > 1e-5
                for observed, target in zip(actual, expected)
            )
        ):
            raise RuntimeError(
                f"rigid rotation did not follow the pivot arc: {positions}"
            )
    command(
        host,
        port,
        "create_primitive",
        {
            "primitive": "cylinder",
            "name": "SmokeCutter",
            "vertices": 32,
            "radius": 0.5,
            "depth": 3.0,
        },
    )
    modeled = command(
        host,
        port,
        "boolean",
        {
            "target": "SmokeBase",
            "operand": "SmokeCutter",
            "operation": "DIFFERENCE",
            "result_name": "SmokeBoolean",
            "delete_operand": True,
        },
    )
    if modeled.get("name") != "SmokeBoolean" or not modeled.get("polygons"):
        raise RuntimeError("boolean modeling did not produce the expected mesh")
    executed = command(
        host,
        port,
        "execute_code",
        {
            "code": (
                "import math\n"
                "import os\n"
                "import subprocess\n"
                "import sys\n"
                "os.write(1, b'native-code-output\\n')\n"
                "subprocess.run(\n"
                "    [sys.executable, '-c', \"print('child-code-output')\"],\n"
                "    check=True,\n"
                ")\n"
                "segments, rings, height = 256, 32, 40.0\n"
                "vertices, faces = [], []\n"
                "for ring in range(rings + 1):\n"
                "    z = -height / 2 + height * ring / rings\n"
                "    for segment in range(segments):\n"
                "        angle = 2 * math.pi * segment / segments\n"
                "        radius = 12.0 + 0.8 * math.sin(12 * angle + z * 0.45)\n"
                "        vertices.append((radius * math.cos(angle), radius * math.sin(angle), z))\n"
                "for ring in range(rings):\n"
                "    for segment in range(segments):\n"
                "        next_segment = (segment + 1) % segments\n"
                "        a = ring * segments + segment\n"
                "        b = ring * segments + next_segment\n"
                "        c = (ring + 1) * segments + next_segment\n"
                "        d = (ring + 1) * segments + segment\n"
                "        faces.append((a, b, c, d))\n"
                "faces.append(tuple(reversed(range(segments))))\n"
                "top = rings * segments\n"
                "faces.append(tuple(top + segment for segment in range(segments)))\n"
                "mesh = bpy.data.meshes.new('SmokeCouplerMesh')\n"
                "mesh.from_pydata(vertices, [], faces)\n"
                "mesh.update()\n"
                "obj = bpy.data.objects.new('SmokeCodeCoupler', mesh)\n"
                "bpy.context.collection.objects.link(obj)\n"
                "print(f'code-created:{obj.name}')\n"
                "result = {'name': obj.name, 'vertices': len(mesh.vertices), "
                "'faces': len(mesh.polygons)}\n"
            ),
            "timeout_seconds": 600.0,
        },
    )
    execution_result = executed.get("result")
    if (
        not isinstance(execution_result, dict)
        or execution_result.get("name") != "SmokeCodeCoupler"
        or execution_result.get("vertices") != 8448
        or execution_result.get("faces") != 8194
        or "native-code-output\n" not in executed.get("stdout", "")
        or "child-code-output\n" not in executed.get("stdout", "")
        or "code-created:SmokeCodeCoupler\n" not in executed.get("stdout", "")
        or executed.get("stdout_truncated") is not False
    ):
        raise RuntimeError("explicit Blender code did not return bounded output")
    code_object = command(
        host, port, "get_object_info", {"name": "SmokeCodeCoupler"}
    )
    if code_object.get("type") != "MESH":
        raise RuntimeError("explicit Blender code did not mutate the scene")
    command(host, port, "save_blend", {"path": "smoke/checkpoint.blend"})
    command(
        host,
        port,
        "rename_object",
        {"name": "SmokeBoolean", "new_name": "ChangedAfterCheckpoint"},
    )
    restored = command(
        host,
        port,
        "restore_checkpoint",
        {"path": "smoke/checkpoint.blend"},
    )
    restored_names = {
        item.get("name")
        for item in restored.get("objects", [])
        if isinstance(item, dict)
    }
    if "SmokeBoolean" not in restored_names or "ChangedAfterCheckpoint" in restored_names:
        raise RuntimeError("checkpoint restore did not recover the saved scene")
    readback = command(host, port, "get_object_info", {"name": "SmokeBoolean"})
    if readback.get("name") != "SmokeBoolean":
        raise RuntimeError("object readback did not find the restored boolean mesh")
    scene = command(host, port, "get_scene_info", {})
    scene_objects = {
        item.get("name"): item
        for item in scene.get("objects", [])
        if isinstance(item, dict)
    }
    if (
        scene.get("active_object") != "SmokeBoolean"
        or not scene_objects.get("SmokeBoolean", {}).get("polygons")
    ):
        raise RuntimeError("scene read did not report the active boolean mesh")
    command(host, port, "export_stl", {"path": "smoke/cube.stl"})
    clear_scene(host, port, 1)
    imported = command(host, port, "import_stl", {"path": "smoke/cube.stl"})
    if not imported.get("objects"):
        raise RuntimeError("STL import did not create an object")
    command(host, port, "save_blend", {"path": "smoke/scene.blend"})
    rendered = command(
        host,
        port,
        "render_still",
        {"path": "smoke/render.png", "width": 256, "height": 256},
    )
    if rendered.get("engine") not in {"BLENDER_EEVEE", "BLENDER_EEVEE_NEXT"}:
        raise RuntimeError("still smoke did not use EEVEE")
    if (
        rendered.get("path") != "smoke/render.png"
        or rendered.get("media_type") != "image/png"
        or rendered.get("width") != 256
        or rendered.get("height") != 256
        or isinstance(rendered.get("size_bytes"), bool)
        or not isinstance(rendered.get("size_bytes"), int)
        or rendered["size_bytes"] <= 0
    ):
        raise RuntimeError("still smoke returned invalid artifact metadata")
    decoded = command(
        host,
        port,
        "execute_code",
        {
            "code": (
                "from pathlib import Path\n"
                "image = bpy.data.images.load(str(Path(workspace_root, 'smoke/render.png')), check_existing=False)\n"
                "try:\n"
                "    pixels = list(image.pixels)\n"
                "    first = tuple(pixels[:4])\n"
                "    varied = any(tuple(pixels[offset:offset + 4]) != first for offset in range(4, len(pixels), 4))\n"
                "    result = {'width': int(image.size[0]), 'height': int(image.size[1]), 'channels': int(image.channels), 'varied': varied}\n"
                "finally:\n"
                "    bpy.data.images.remove(image)\n"
            ),
            "timeout_seconds": 120.0,
        },
    )
    if decoded.get("result") != {
        "width": 256,
        "height": 256,
        "channels": 4,
        "varied": True,
    }:
        raise RuntimeError("still smoke did not decode to a useful 256x256 RGBA image")
    clear_scene(host, port, 1)
    hollow = command(
        host,
        port,
        "execute_code",
        {
            "code": (
                "import math\n"
                "segments, outer_radius, inner_radius, half_height = 256, 12.0, 5.0, 10.0\n"
                "vertices = []\n"
                "for z, radius in [(-half_height, outer_radius), (-half_height, inner_radius), (half_height, outer_radius), (half_height, inner_radius)]:\n"
                "    for segment in range(segments):\n"
                "        angle = 2.0 * math.pi * segment / segments\n"
                "        vertices.append((radius * math.cos(angle), radius * math.sin(angle), z))\n"
                "bottom_outer, bottom_inner, top_outer, top_inner = (0, segments, 2 * segments, 3 * segments)\n"
                "faces = []\n"
                "for segment in range(segments):\n"
                "    next_segment = (segment + 1) % segments\n"
                "    bo, bn = bottom_outer + segment, bottom_outer + next_segment\n"
                "    bi, binext = bottom_inner + segment, bottom_inner + next_segment\n"
                "    to, tn = top_outer + segment, top_outer + next_segment\n"
                "    ti, tinext = top_inner + segment, top_inner + next_segment\n"
                "    faces.extend([(bo, bn, tn, to), (bi, ti, tinext, binext), (to, tn, tinext, ti), (bo, bi, binext, bn)])\n"
                "mesh = bpy.data.meshes.new('SmokeHollowCouplerMesh')\n"
                "mesh.from_pydata(vertices, [], faces)\n"
                "mesh.update()\n"
                "obj = bpy.data.objects.new('SmokeHollowCoupler', mesh)\n"
                "bpy.context.collection.objects.link(obj)\n"
                "result = {'vertices': len(mesh.vertices), 'faces': len(mesh.polygons), 'segments': segments, 'outer_radius': outer_radius, 'inner_radius': inner_radius}\n"
            ),
            "timeout_seconds": 120.0,
        },
    ).get("result")
    if hollow != {
        "vertices": 1024,
        "faces": 1024,
        "segments": 256,
        "outer_radius": 12.0,
        "inner_radius": 5.0,
    }:
        raise RuntimeError("diagnostic smoke did not create the hollow reference part")
    instanced = command(
        host,
        port,
        "execute_code",
        {
            "code": (
                "mesh_object = next(obj for obj in bpy.context.scene.objects if obj.type == 'MESH')\n"
                "linked_collections = list(mesh_object.users_collection)\n"
                "source = bpy.data.collections.new('SmokeInstanceSource')\n"
                "source.objects.link(mesh_object)\n"
                "for collection in linked_collections:\n"
                "    collection.objects.unlink(mesh_object)\n"
                "instance = bpy.data.objects.new('SmokeCollectionInstance', None)\n"
                "instance.instance_type = 'COLLECTION'\n"
                "instance.instance_collection = source\n"
                "instance.location = (12.0, 0.0, 0.0)\n"
                "instance.scale = (-1.0, 1.0, 1.0)\n"
                "bpy.context.scene.collection.objects.link(instance)\n"
                "result = {'direct_geometry_count': sum(obj.type == 'MESH' for obj in bpy.context.scene.objects), 'instance_type': instance.instance_type, 'scale_x': instance.scale.x, 'source_objects': len(source.objects)}\n"
            ),
            "timeout_seconds": 120.0,
        },
    )
    if instanced.get("result") != {
        "direct_geometry_count": 0,
        "instance_type": "COLLECTION",
        "scale_x": -1.0,
        "source_objects": 1,
    }:
        raise RuntimeError("multi-view smoke did not create an instance-only scene")
    product_profiles = (
        ("engineering", "orthographic", "Khronos PBR Neutral", False, "preserve"),
        (
            "studio_neutral",
            "perspective",
            "Khronos PBR Neutral",
            True,
            "smooth_by_angle",
        ),
        ("studio_dark", "perspective", "AgX", True, "smooth_by_angle"),
    )
    product_paths = []
    empty_slot = command(
        host,
        port,
        "execute_code",
        {
            "code": (
                "obj = bpy.data.objects['SmokeHollowCoupler']\n"
                "obj.data.materials.append(None)\n"
                "result = {'materials': len(obj.data.materials), 'empty': sum(material is None for material in obj.data.materials)}\n"
            )
        },
    ).get("result")
    if empty_slot != {"materials": 1, "empty": 1}:
        raise RuntimeError("product smoke could not create an empty material slot")
    callback_guard = command(
        host,
        port,
        "execute_code",
        {
            "code": (
                "obj = bpy.data.objects['SmokeHollowCoupler']\n"
                "initial = tuple(obj.data.vertices[0].co)\n"
                "def printable_product_smoke_render_pre(_scene, *_args):\n"
                "    obj.data.vertices[0].co.x += 1000.0\n"
                "bpy.app.handlers.render_pre.append(printable_product_smoke_render_pre)\n"
                "bpy.app.driver_namespace['printable_product_smoke_initial'] = initial\n"
                "result = {'registered': printable_product_smoke_render_pre in bpy.app.handlers.render_pre}\n"
            )
        },
    ).get("result")
    if callback_guard != {"registered": True}:
        raise RuntimeError("product smoke could not install its mutation callback")
    for (
        profile,
        camera_type,
        view_transform,
        ground_enabled,
        shading,
    ) in product_profiles:
        relative = f"smoke/products/{profile}.png"
        if profile == "studio_neutral":
            assigned = command(
                host,
                port,
                "execute_code",
                {
                    "code": (
                        "obj = bpy.data.objects['SmokeHollowCoupler']\n"
                        "material = bpy.data.materials.new('SmokeSourceMaterial')\n"
                        "material.diffuse_color = (0.05, 0.2, 0.65, 1.0)\n"
                        "obj.data.materials.append(None)\n"
                        "obj.material_slots[1].link = 'OBJECT'\n"
                        "obj.material_slots[1].material = material\n"
                        "result = {'materials': len(obj.material_slots), 'empty': sum(slot.material is None for slot in obj.material_slots), 'links': [slot.link for slot in obj.material_slots]}\n"
                    )
                },
            ).get("result")
            if assigned != {
                "materials": 2,
                "empty": 1,
                "links": ["DATA", "OBJECT"],
            }:
                raise RuntimeError(
                    "product smoke could not assign an object-linked source material"
                )
        presentation: dict[str, Any] = {
            "profile": profile,
            "view": {"azimuth_degrees": 40.0, "elevation_degrees": 22.0},
        }
        if profile == "studio_neutral":
            presentation["materials"] = [
                {
                    "objects": ["SmokeHollowCoupler"],
                    "base_color_srgb": [0.74, 0.31, 0.12],
                    "metallic": 0.0,
                    "roughness": 0.34,
                }
            ]
        product = command(
            host,
            port,
            "render_product",
            {
                "path": relative,
                "objects": ["SmokeHollowCoupler"],
                "presentation": presentation,
                "width": 160,
                "height": 120,
                "timeout_seconds": 600.0,
            },
        )
        effective = product.get("presentation", {})
        if (
            product.get("path") != relative
            or product.get("width") != 160
            or product.get("height") != 120
            or product.get("media_type") != "image/png"
            or product.get("engine")
            not in {"BLENDER_EEVEE", "BLENDER_EEVEE_NEXT"}
            or product.get("source_state_verified") is not True
            or product.get("cleanup_verified") is not True
            or effective.get("profile") != profile
            or effective.get("camera", {}).get("type") != camera_type
            or effective.get("camera", {}).get("azimuth_degrees") != 40.0
            or effective.get("camera", {}).get("elevation_degrees") != 22.0
            or effective.get("color_management", {}).get("view_transform")
            != view_transform
            or effective.get("ground", {}).get("enabled") is not ground_enabled
            or effective.get("shading", {}).get("mode") != shading
            or effective.get("shading", {}).get("presentation_only") is not True
            or effective.get("framing", {}).get("margin_percent") != 15.0
            or effective.get("framing", {}).get("instance_count") != 1
            or effective.get("geometry", {}).get("instances") != 1
            or effective.get("geometry", {}).get("unique_evaluated_meshes") != 1
            or not isinstance(effective.get("geometry", {}).get("vertices"), int)
            or effective["geometry"]["vertices"] <= 0
            or not isinstance(product.get("size_bytes"), int)
            or product["size_bytes"] <= 0
            or not isinstance(product.get("sha256"), str)
            or len(product["sha256"]) != 64
        ):
            raise RuntimeError(
                f"{profile} product render returned an invalid contract: {product}"
            )
        if (
            profile == "studio_neutral"
            and effective.get("materials", {}).get("overrides", [{}])[0].get(
                "objects"
            )
            != ["SmokeHollowCoupler"]
        ):
            raise RuntimeError("product render did not apply the explicit material")
        if (
            profile == "engineering"
            and effective.get("materials", {}).get("fallback", {}).get("objects")
            != ["SmokeHollowCoupler"]
        ):
            raise RuntimeError("product render did not apply the fallback material")
        if (
            profile == "studio_dark"
            and effective.get("materials", {}).get("preserved_objects")
            != ["SmokeHollowCoupler"]
        ):
            raise RuntimeError("product render did not preserve the source material")
        if (
            profile == "studio_dark"
            and effective.get("materials", {}).get("fallback", {}).get("objects")
            != ["SmokeHollowCoupler"]
        ):
            raise RuntimeError("product render did not fill the empty material slot")
        product_paths.append(relative)
    callback_check = command(
        host,
        port,
        "execute_code",
        {
            "code": (
                "obj = bpy.data.objects['SmokeHollowCoupler']\n"
                "handlers = [handler for handler in bpy.app.handlers.render_pre if getattr(handler, '__name__', '') == 'printable_product_smoke_render_pre']\n"
                "initial = bpy.app.driver_namespace.pop('printable_product_smoke_initial')\n"
                "current = tuple(obj.data.vertices[0].co)\n"
                "for handler in handlers:\n"
                "    bpy.app.handlers.render_pre.remove(handler)\n"
                "result = {'restored': len(handlers) == 1, 'source_unchanged': current == initial}\n"
            )
        },
    ).get("result")
    if callback_check != {"restored": True, "source_unchanged": True}:
        raise RuntimeError(
            f"product render did not isolate source mutation callbacks: {callback_check}"
        )
    product_checks = command(
        host,
        port,
        "execute_code",
        {
            "code": (
                "import hashlib\n"
                "from pathlib import Path\n"
                f"paths = {product_paths!r}\n"
                "checks = []\n"
                "for relative in paths:\n"
                "    path = Path(workspace_root, relative)\n"
                "    image = bpy.data.images.load(str(path), check_existing=False)\n"
                "    try:\n"
                "        pixels = list(image.pixels)\n"
                "        first = tuple(pixels[:4])\n"
                "        varied = any(tuple(pixels[offset:offset + 4]) != first for offset in range(4, len(pixels), 4))\n"
                "        checks.append({'width': int(image.size[0]), 'height': int(image.size[1]), 'varied': varied, 'sha256': hashlib.sha256(path.read_bytes()).hexdigest()})\n"
                "    finally:\n"
                "        bpy.data.images.remove(image)\n"
                "result = {'images': checks, 'scene_objects': sorted(obj.name for obj in bpy.context.scene.objects), 'temporary_objects': sorted(obj.name for obj in bpy.data.objects if obj.name.startswith('PrintableProduct'))}\n"
            ),
            "timeout_seconds": 120.0,
        },
    ).get("result")
    product_images = (
        product_checks.get("images") if isinstance(product_checks, dict) else None
    )
    if (
        not isinstance(product_images, list)
        or len(product_images) != len(product_profiles)
        or any(
            check.get("width") != 160
            or check.get("height") != 120
            or check.get("varied") is not True
            for check in product_images
        )
        or len({check.get("sha256") for check in product_images}) != len(
            product_profiles
        )
        or product_checks.get("scene_objects") != ["SmokeCollectionInstance"]
        or product_checks.get("temporary_objects") != []
    ):
        raise RuntimeError(
            f"product profiles did not produce distinct clean presentation artifacts: {product_checks}"
        )
    view_specs = [
        {
            "path": "smoke/views/front.png",
            "label": "FRONT",
            "direction": [0.0, -1.0, 0.0],
        },
        {
            "path": "smoke/views/right.png",
            "label": "RIGHT",
            "direction": [1.0, 0.0, 0.0],
        },
        {
            "path": "smoke/views/back.png",
            "label": "BACK",
            "direction": [0.0, 1.0, 0.0],
        },
        {
            "path": "smoke/views/iso.png",
            "label": "ISOMETRIC",
            "direction": [1.0, -1.0, 1.0],
        },
    ]
    multi = command(
        host,
        port,
        "render_views",
        {
            "views": view_specs,
            "presentation": {"profile": "studio_dark"},
            "width": 128,
            "height": 128,
            "timeout_seconds": 600.0,
        },
    )
    if (
        multi.get("engine") not in {"BLENDER_EEVEE", "BLENDER_EEVEE_NEXT"}
        or multi.get("presentation", {}).get("profile") != "studio_dark"
        or len(multi.get("presentation", {}).get("views", []))
        != len(view_specs)
        or len(multi.get("views", [])) != len(view_specs)
        or any(
            rendered.get("path") != expected["path"]
            or rendered.get("label") != expected["label"]
            or rendered.get("width") != 128
            or rendered.get("height") != 128
            or rendered.get("media_type") != "image/png"
            or not isinstance(rendered.get("size_bytes"), int)
            or rendered["size_bytes"] <= 0
            for rendered, expected in zip(multi.get("views", []), view_specs)
        )
    ):
        raise RuntimeError("multi-view smoke returned invalid artifact metadata")
    multi_bounds = multi.get("bounds", {})
    dimensions = multi_bounds.get("dimensions", [])
    if (
        multi_bounds.get("coordinate_space") != "world"
        or len(dimensions) != 3
        or any(dimension <= 0 for dimension in dimensions)
    ):
        raise RuntimeError("multi-view smoke did not report useful evaluated bounds")
    cross_section = command(
        host,
        port,
        "render_diagnostic",
        {
            "path": "smoke/diagnostics/cross-section.png",
            "mode": "cross_section",
            "axis": "Z",
            "width": 128,
            "height": 128,
            "timeout_seconds": 600.0,
        },
    )
    if (
        cross_section.get("mode") != "cross_section"
        or cross_section.get("analysis", {}).get("section_faces", 0) <= 0
        or cross_section.get("analysis", {}).get("evaluated_edges", 0) <= 0
        or cross_section.get("analysis", {}).get("evaluated_loops", 0) <= 0
        or cross_section.get("media_type") != "image/png"
    ):
        raise RuntimeError("cross-section smoke did not produce a capped section")
    expected_section_area = (
        0.5
        * 256
        * math.sin(2.0 * math.pi / 256)
        * (12.0**2 - 5.0**2)
    )
    if not math.isclose(
        cross_section["analysis"].get("section_area", -1.0),
        expected_section_area,
        rel_tol=1e-5,
        abs_tol=1e-5,
    ):
        raise RuntimeError("cross-section smoke did not preserve the through-bore area")
    heatmap = command(
        host,
        port,
        "render_diagnostic",
        {
            "path": "smoke/diagnostics/heatmap.png",
            "mode": "overhang",
            "build_direction": [0.0, 0.0, 1.0],
            "overhang_angle_degrees": 45.0,
            "width": 128,
            "height": 128,
            "timeout_seconds": 600.0,
        },
    )
    heatmap_categories = heatmap.get("analysis", {}).get("categories", {})
    if (
        heatmap.get("mode") != "overhang"
        or heatmap_categories.get("supported", {}).get("faces", 0) <= 0
        or heatmap_categories.get("severe", {}).get("faces", 0) <= 0
        or heatmap.get("analysis", {}).get("evaluated_edges", 0) <= 0
        or heatmap.get("analysis", {}).get("evaluated_loops", 0) <= 0
        or heatmap.get("media_type") != "image/png"
    ):
        raise RuntimeError("overhang smoke did not classify useful face categories")
    decoded_views = command(
        host,
        port,
        "execute_code",
        {
            "code": (
                "import hashlib\n"
                "from pathlib import Path\n"
                "paths = ['smoke/views/front.png', 'smoke/views/right.png', 'smoke/views/back.png', 'smoke/views/iso.png']\n"
                "checks = []\n"
                "for relative in paths:\n"
                "    path = Path(workspace_root, relative)\n"
                "    image = bpy.data.images.load(str(path), check_existing=False)\n"
                "    try:\n"
                "        pixels = list(image.pixels)\n"
                "        first = tuple(pixels[:4])\n"
                "        checks.append({'width': int(image.size[0]), 'height': int(image.size[1]), 'varied': any(tuple(pixels[offset:offset + 4]) != first for offset in range(4, len(pixels), 4)), 'sha256': hashlib.sha256(path.read_bytes()).hexdigest()})\n"
                "    finally:\n"
                "        bpy.data.images.remove(image)\n"
                "result = checks\n"
            ),
            "timeout_seconds": 30.0,
        },
    )
    checks = decoded_views.get("result")
    if (
        not isinstance(checks, list)
        or len(checks) != len(view_specs)
        or any(
            check.get("width") != 128
            or check.get("height") != 128
            or check.get("varied") is not True
            for check in checks
        )
        or len({check.get("sha256") for check in checks}) < 2
    ):
        raise RuntimeError("multi-view smoke did not produce useful distinct PNG views")
    diagnostic_checks = command(
        host,
        port,
        "execute_code",
        {
            "code": (
                "from pathlib import Path\n"
                "checks = []\n"
                "for relative in ['smoke/diagnostics/cross-section.png', 'smoke/diagnostics/heatmap.png']:\n"
                "    image = bpy.data.images.load(str(Path(workspace_root, relative)), check_existing=False)\n"
                "    try:\n"
                "        pixels = list(image.pixels)\n"
                "        first = tuple(pixels[:4])\n"
                "        checks.append({'width': int(image.size[0]), 'height': int(image.size[1]), 'varied': any(tuple(pixels[offset:offset + 4]) != first for offset in range(4, len(pixels), 4))})\n"
                "    finally:\n"
                "        bpy.data.images.remove(image)\n"
                "result = checks\n"
            ),
            "timeout_seconds": 120.0,
        },
    ).get("result")
    if (
        not isinstance(diagnostic_checks, list)
        or len(diagnostic_checks) != 2
        or any(
            check.get("width") != 128
            or check.get("height") != 128
            or check.get("varied") is not True
            for check in diagnostic_checks
        )
    ):
        raise RuntimeError("diagnostic smoke did not produce useful PNG images")
    animation_setup = command(
        host,
        port,
        "execute_code",
        {
            "code": (
                "from mathutils import Vector\n"
                "scene = bpy.context.scene\n"
                "original_frame = scene.frame_current\n"
                "product = bpy.data.objects['SmokeCollectionInstance']\n"
                "scene.frame_set(1)\n"
                "product.location = (0.0, 0.0, 0.0)\n"
                "product.keyframe_insert(data_path='location', frame=1)\n"
                "product.location = (8.0, 0.0, 0.0)\n"
                "product.keyframe_insert(data_path='location', frame=3)\n"
                "camera_data = bpy.data.cameras.new('SmokeAuthoredCameraData')\n"
                "camera = bpy.data.objects.new('SmokeAuthoredCamera', camera_data)\n"
                "scene.collection.objects.link(camera)\n"
                "camera.location = (35.0, -35.0, 24.0)\n"
                "camera.rotation_euler = (Vector((4.0, 0.0, 0.0)) - camera.location).to_track_quat('-Z', 'Y').to_euler()\n"
                "camera_data.lens = 55.0\n"
                "scene.camera = camera\n"
                "scene.frame_set(2)\n"
                "result = {'original_frame': original_frame, 'measurement_frame': scene.frame_current}\n"
            )
        },
    ).get("result")
    if (
        not isinstance(animation_setup, dict)
        or animation_setup.get("measurement_frame") != 2
    ):
        raise RuntimeError("animation presentation smoke could not author its fixture")
    command(
        host,
        port,
        "job_save_checkpoint",
        {"path": ".printable/jobs/smoke/presentation-source.blend"},
    )
    command(
        host,
        port,
        "execute_code",
        {"code": "bpy.context.scene.frame_set(3); result = bpy.context.scene.frame_current"},
    )
    restored_checkpoint = command(
        host,
        port,
        "job_restore_checkpoint",
        {"path": ".printable/jobs/smoke/presentation-source.blend"},
    )
    if restored_checkpoint.get("frame_current") != 2:
        raise RuntimeError(
            "durable checkpoint restore did not report its authored current frame"
        )
    sequence = command(
        host,
        port,
        "job_measure_sequence_bounds",
        {
            "frame_start": 1,
            "frame_end": 3,
            "frame_step": 1,
            "timeout_seconds": 120.0,
        },
    )
    sequence_bounds = sequence.get("bounds")
    static_product_dimensions = product.get("bounds", {}).get("dimensions", [])
    if (
        sequence.get("frames_evaluated") != 3
        or sequence.get("frame_start") != 1
        or sequence.get("frame_end") != 3
        or sequence.get("frame_step") != 1
        or not isinstance(sequence_bounds, dict)
        or sequence_bounds.get("coordinate_space") != "world"
        or sequence_bounds.get("unit") != "blender_unit"
        or len(static_product_dimensions) != 3
        or sequence_bounds.get("dimensions", [0.0])[0]
        < static_product_dimensions[0] + 7.5
    ):
        raise RuntimeError(
            f"sequence presentation bounds were not complete: {sequence}"
        )
    restored_frame = command(
        host,
        port,
        "execute_code",
        {"code": "result = bpy.context.scene.frame_current"},
    ).get("result")
    if restored_frame != 2:
        raise RuntimeError("sequence bounds evaluation did not restore the authored frame")
    presented_frames = []
    for frame in (1, 3):
        presented_frames.append(
            command(
                host,
                port,
                "job_render_product",
                {
                    "path": f".printable/jobs/smoke/presented-{frame}.png",
                    "presentation": {"profile": "studio_neutral"},
                    "frame": frame,
                    "camera_behavior": "bounds",
                    "framing_bounds": sequence_bounds,
                    "allow_ground": False,
                    "width": 128,
                    "height": 96,
                    "timeout_seconds": 120.0,
                },
            )
        )
    first_presentation = presented_frames[0].get("presentation", {})
    last_presentation = presented_frames[-1].get("presentation", {})
    if (
        any(
            rendered.get("source_state_verified") is not True
            or rendered.get("cleanup_verified") is not True
            or rendered.get("width") != 128
            or rendered.get("height") != 96
            or rendered.get("size_bytes", 0) <= 0
            for rendered in presented_frames
        )
        or first_presentation.get("camera", {}).get("behavior") != "bounds"
        or first_presentation.get("camera", {}).get("position")
        != last_presentation.get("camera", {}).get("position")
        or first_presentation.get("lighting") != last_presentation.get("lighting")
        or first_presentation.get("framing", {}).get("bounds") != sequence_bounds
        or last_presentation.get("framing", {}).get("bounds") != sequence_bounds
        or first_presentation.get("ground", {}).get("enabled") is not False
    ):
        raise RuntimeError(
            "presented animation did not keep framing and lighting locked"
        )
    preserved = command(
        host,
        port,
        "job_render_product",
        {
            "path": ".printable/jobs/smoke/presented-preserve.png",
            "presentation": {"profile": "studio_dark"},
            "frame": 3,
            "camera_behavior": "preserve",
            "framing_bounds": product["bounds"],
            "width": 128,
            "height": 96,
            "timeout_seconds": 120.0,
        },
    )
    if (
        preserved.get("presentation", {}).get("camera", {}).get("behavior")
        != "preserve"
        or preserved.get("presentation", {}).get("camera", {}).get(
            "source_object"
        )
        != "SmokeAuthoredCamera"
        or preserved.get("presentation", {})
        .get("framing", {})
        .get("bounds")
        != product["bounds"]
        or preserved.get("presentation", {})
        .get("ground", {})
        .get("enabled")
        is not True
        or preserved.get("source_state_verified") is not True
        or preserved.get("cleanup_verified") is not True
    ):
        raise RuntimeError("general animation did not preserve its authored camera")
    command(
        host,
        port,
        "execute_code",
        {
            "code": (
                "camera = bpy.data.objects['SmokeAuthoredCamera']\n"
                "camera.location.z = -20.0\n"
                "result = camera.location.z\n"
            )
        },
    )
    try:
        command(
            host,
            port,
            "job_render_product",
            {
                "path": ".printable/jobs/smoke/presented-below-ground.png",
                "presentation": {"profile": "studio_dark"},
                "frame": 3,
                "camera_behavior": "preserve",
                "framing_bounds": product["bounds"],
                "width": 128,
                "height": 96,
                "timeout_seconds": 120.0,
            },
        )
    except RuntimeError as error:
        if "must remain above the studio ground plane" not in str(error):
            raise
    else:
        raise RuntimeError(
            "preserved camera below the studio ground rendered an occluded artifact"
        )
    below_ground_state = command(
        host,
        port,
        "execute_code",
        {
            "code": (
                "from pathlib import Path\n"
                "artifact = Path(workspace_root, '.printable', 'jobs', 'smoke', 'presented-below-ground.png')\n"
                "camera = bpy.data.objects['SmokeAuthoredCamera']\n"
                "camera.location = (35.0, -35.0, 24.0)\n"
                "result = {'artifact_exists': artifact.exists(), 'camera_z': camera.location.z}\n"
            )
        },
    ).get("result")
    if below_ground_state != {"artifact_exists": False, "camera_z": 24.0}:
        raise RuntimeError(
            f"below-ground camera rejection left presentation state behind: {below_ground_state}"
        )
    timeline_fixture = command(
        host,
        port,
        "execute_code",
        {
            "code": (
                "scene = bpy.context.scene\n"
                "product = bpy.data.objects['SmokeCollectionInstance']\n"
                "product.animation_data_clear()\n"
                "product.location = (0.0, 0.0, 0.0)\n"
                "camera_data = bpy.data.objects['SmokeAuthoredCamera'].data\n"
                "scene.frame_set(1)\n"
                "camera_data.lens = 42.0\n"
                "camera_data.keyframe_insert(data_path='lens', frame=1)\n"
                "camera_data.lens = 72.0\n"
                "camera_data.keyframe_insert(data_path='lens', frame=3)\n"
                "source = bpy.data.objects['SmokeHollowCoupler']\n"
                "source.data.materials.clear()\n"
                "material = bpy.data.materials.new('SmokeTimelineMaterial')\n"
                "material.use_nodes = True\n"
                "base_color = material.node_tree.nodes['Principled BSDF'].inputs['Base Color']\n"
                "base_color.default_value = (0.8, 0.03, 0.02, 1.0)\n"
                "base_color.keyframe_insert(data_path='default_value', frame=1)\n"
                "base_color.default_value = (0.02, 0.08, 0.8, 1.0)\n"
                "base_color.keyframe_insert(data_path='default_value', frame=3)\n"
                "source.data.materials.append(material)\n"
                "scene.frame_set(2)\n"
                "result = {'frame': scene.frame_current, 'camera_action': camera_data.animation_data is not None, 'material_action': material.node_tree.animation_data is not None}\n"
            )
        },
    ).get("result")
    if timeline_fixture != {
        "frame": 2,
        "camera_action": True,
        "material_action": True,
    }:
        raise RuntimeError(
            f"animation presentation smoke could not author keyed presentation data: {timeline_fixture}"
        )
    timeline_presentations = []
    for frame in (1, 3):
        timeline_presentations.append(
            command(
                host,
                port,
                "job_render_product",
                {
                    "path": f".printable/jobs/smoke/presented-timeline-{frame}.png",
                    "presentation": {"profile": "studio_neutral"},
                    "frame": frame,
                    "camera_behavior": "preserve",
                    "framing_bounds": product["bounds"],
                    "allow_ground": False,
                    "width": 128,
                    "height": 96,
                    "timeout_seconds": 120.0,
                },
            )
        )
    timeline_color_biases = command(
        host,
        port,
        "execute_code",
        {
            "code": (
                "from pathlib import Path\n"
                "biases = []\n"
                "for frame in (1, 3):\n"
                "    path = Path(workspace_root, '.printable', 'jobs', 'smoke', f'presented-timeline-{frame}.png')\n"
                "    image = bpy.data.images.load(str(path), check_existing=False)\n"
                "    try:\n"
                "        pixels = list(image.pixels)\n"
                "        biases.append(sum(pixels[offset] - pixels[offset + 2] for offset in range(0, len(pixels), 4)) / (len(pixels) / 4))\n"
                "    finally:\n"
                "        bpy.data.images.remove(image)\n"
                "result = biases\n"
            )
        },
    ).get("result")
    timeline_lenses = [
        rendered.get("presentation", {}).get("camera", {}).get("lens_mm")
        for rendered in timeline_presentations
    ]
    if (
        timeline_lenses != [42.0, 72.0]
        or timeline_presentations[0].get("sha256")
        == timeline_presentations[1].get("sha256")
        or not isinstance(timeline_color_biases, list)
        or len(timeline_color_biases) != 2
        or timeline_color_biases[0] <= 0.0
        or timeline_color_biases[1] >= 0.0
        or any(
            rendered.get("source_state_verified") is not True
            or rendered.get("cleanup_verified") is not True
            for rendered in timeline_presentations
        )
    ):
        raise RuntimeError(
            "presented timeline did not evaluate keyed camera and material data at each requested frame"
        )
    original_frame = animation_setup["original_frame"]
    animation_cleanup = command(
        host,
        port,
        "execute_code",
        {
            "code": (
                "scene = bpy.context.scene\n"
                "product = bpy.data.objects['SmokeCollectionInstance']\n"
                "product.animation_data_clear()\n"
                "product.location = (0.0, 0.0, 0.0)\n"
                "source = bpy.data.objects['SmokeHollowCoupler']\n"
                "source.data.materials.clear()\n"
                "timeline_material = bpy.data.materials.get('SmokeTimelineMaterial')\n"
                "if timeline_material is not None:\n"
                "    bpy.data.materials.remove(timeline_material)\n"
                "camera = bpy.data.objects['SmokeAuthoredCamera']\n"
                "camera_data = camera.data\n"
                "camera_data.animation_data_clear()\n"
                "bpy.data.objects.remove(camera, do_unlink=True)\n"
                "bpy.data.cameras.remove(camera_data)\n"
                f"scene.frame_set({int(original_frame)})\n"
                "result = {'frame': scene.frame_current, 'temporary_objects': sorted(obj.name for obj in bpy.data.objects if obj.name.startswith('PrintableProduct'))}\n"
            )
        },
    ).get("result")
    if animation_cleanup != {
        "frame": original_frame,
        "temporary_objects": [],
    }:
        raise RuntimeError(
            f"animation presentation smoke did not restore its fixture: {animation_cleanup}"
        )
    assert_timeout_cleans_descendant(
        host,
        port,
        "trace-timeout-descendant.pid",
        1.0,
        False,
    )
    assert_timeout_cleans_descendant(
        host,
        port,
        "supervisor-timeout-descendant.pid",
        0.1,
        True,
    )
    assert_background_work_is_contained(host, port)
    command(host, port, "bridge_status", {})


def require_optix_bridge(host: str, port: int) -> list[dict[str, Any]]:
    wait_until_ready(host, port)
    status = command(host, port, "bridge_status", {})
    if status.get("render_device") != "OPTIX":
        raise RuntimeError("GPU smoke bridge is not configured for OptiX")
    devices = status.get("cycles_devices")
    if not isinstance(devices, list) or not any(
        isinstance(device, dict)
        and device.get("type") == "OPTIX"
        and device.get("enabled") is True
        for device in devices
    ):
        raise RuntimeError("GPU smoke bridge did not enable an OptiX device")
    return devices


def gpu_eevee_smoke(host: str, port: int) -> None:
    devices = require_optix_bridge(host, port)
    clear_scene(host, port, 0)
    command(
        host,
        port,
        "create_primitive",
        {"primitive": "uv_sphere", "name": "GpuSmokeSphere", "radius": 1.0},
    )
    eevee = command(
        host,
        port,
        "render_still",
        {
            "path": "smoke/gpu-eevee.png",
            "width": 512,
            "height": 512,
            "engine": "EEVEE",
        },
    )
    if eevee.get("engine") not in {"BLENDER_EEVEE", "BLENDER_EEVEE_NEXT"}:
        raise RuntimeError("GPU smoke did not render with EEVEE")
    if eevee.get("render_device") != "GRAPHICS":
        raise RuntimeError("EEVEE smoke did not use the graphics render path")
    graphics = eevee.get("graphics_backend")
    if not isinstance(graphics, dict) or not any(
        "NVIDIA" in str(graphics.get(field, ""))
        for field in ("vendor", "renderer")
    ):
        raise RuntimeError("EEVEE smoke did not use the NVIDIA graphics stack")
    print(
        "bridge_eevee_evidence="
        + json.dumps(
            {
                "cycles_devices": devices,
                "eevee_engine": eevee.get("engine"),
                "graphics_backend": graphics,
            },
            separators=(",", ":"),
            sort_keys=True,
        )
    )


def gpu_cycles_smoke(host: str, port: int) -> None:
    devices = require_optix_bridge(host, port)
    sphere = command(host, port, "get_object_info", {"name": "GpuSmokeSphere"})
    if sphere.get("type") != "MESH":
        raise RuntimeError("Cycles smoke scene is missing the EEVEE stage mesh")
    rendered_profiles = []
    for profile, path in (
        ("studio_neutral", "smoke/gpu-cycles.png"),
        ("studio_dark", "smoke/gpu-cycles-dark.png"),
    ):
        cycles = command(
            host,
            port,
            "render_product",
            {
                "path": path,
                "objects": ["GpuSmokeSphere"],
                "presentation": {"profile": profile},
                "width": 768,
                "height": 768,
                "engine": "CYCLES",
                "samples": 256,
            },
        )
        if (
            cycles.get("engine") != "CYCLES"
            or cycles.get("render_device") != "OPTIX"
            or cycles.get("samples") != 256
            or cycles.get("presentation", {}).get("profile") != profile
            or cycles.get("source_state_verified") is not True
            or cycles.get("cleanup_verified") is not True
            or not isinstance(cycles.get("size_bytes"), int)
            or cycles["size_bytes"] <= 0
        ):
            raise RuntimeError(
                f"{profile} product smoke did not render with Cycles and OptiX"
            )
        rendered_profiles.append(profile)
    print(
        "bridge_cycles_evidence="
        + json.dumps(
            {
                "cycles_devices": devices,
                "cycles_engine": "CYCLES",
                "cycles_render_device": "OPTIX",
                "product_profiles": rendered_profiles,
            },
            separators=(",", ":"),
            sort_keys=True,
        )
    )


def busy_wait(host: str, port: int) -> None:
    wait_until_ready(host, port)
    try:
        command(host, port, "bridge_test_wait", {"seconds": 120})
    except RuntimeError as error:
        message = str(error)
        if "shutting down" in message or "bridge closed" in message:
            return
        raise
    raise RuntimeError("busy command completed instead of being interrupted")


def unresponsive_execute(host: str, port: int) -> None:
    wait_until_ready(host, port)
    try:
        command(
            host,
            port,
            "execute_code",
            {
                "code": (
                    "import os\n"
                    "import sys\n"
                    "from pathlib import Path\n"
                    "Path(workspace_root, 'shutdown-smoke.started').write_text("
                    "'started\\n', encoding='utf-8')\n"
                    "os._exit = lambda _code: None\n"
                    "sys.settrace(None)\n"
                    "sys.setswitchinterval(600.0)\n"
                    "while True:\n"
                    "    pass\n"
                ),
                "timeout_seconds": 3600.0,
            },
        )
    except OSError:
        return
    except RuntimeError as error:
        if "bridge closed" in str(error):
            return
        raise
    raise RuntimeError("unresponsive execution completed during shutdown")


def overdue_wait(host: str, port: int) -> None:
    wait_until_ready(host, port)
    try:
        command(host, port, "bridge_test_wait", {"seconds": 3})
    except RuntimeError as error:
        message = str(error).lower()
        if "timeout" in message or "bridge closed" in message:
            return
        raise
    raise RuntimeError("overdue command completed instead of timing out")


def partial_frame(host: str, port: int) -> None:
    wait_until_ready(host, port)
    with socket.create_connection((host, port), timeout=5) as connection:
        connection.sendall(b"\x00")
        connection.settimeout(15)
        try:
            received = connection.recv(1)
        except ConnectionResetError:
            return
        if received != b"":
            raise RuntimeError("partial-frame connection received unexpected data")


def ui_smoke(host: str, port: int, *, require_nvidia: bool) -> None:
    wait_until_ready(host, port)
    probe = """
import gpu
import os
window = bpy.context.window_manager.windows[0]
area = next(area for area in window.screen.areas if area.type == 'VIEW_3D')
region = next(region for region in area.regions if region.type == 'WINDOW')
space = area.spaces.active
offscreen = gpu.types.GPUOffScreen(64, 64)
try:
    with bpy.context.temp_override(window=window, area=area, region=region):
        bpy.context.view_layer.update()
        offscreen.draw_view3d(
            bpy.context.scene, bpy.context.view_layer, space, region,
            space.region_3d.view_matrix, space.region_3d.window_matrix,
            do_color_management=True,
        )
    with offscreen.bind():
        pixels = gpu.state.active_framebuffer_get().read_color(0, 0, 64, 64, 4, 0, 'UBYTE')
    pixels.dimensions = 64 * 64 * 4
    colors = {
        tuple(int(pixels[offset + channel]) for channel in range(3))
        for offset in range(0, len(pixels), 4)
    }
    result = {
        'background': bpy.app.background,
        'vendor': gpu.platform.vendor_get(),
        'renderer': gpu.platform.renderer_get(),
        'display': os.environ.get('DISPLAY', ''),
        'width': offscreen.width,
        'height': offscreen.height,
        'pixel_count': len(pixels) // 4,
        'unique_rgb': len(colors),
        'max_rgb': max((max(color) for color in colors), default=0),
    }
finally:
    offscreen.free()
"""
    for restored in (False, True):
        if restored:
            command(host, port, "save_blend", {"path": "smoke/ui.blend"})
            command(host, port, "restore_checkpoint", {"path": "smoke/ui.blend"})
        observed = command(host, port, "execute_code", {"code": probe}).get("result")
        if not isinstance(observed, dict) or observed.get("background") is not False:
            raise RuntimeError("UI smoke did not reach a normal Blender window")
        if observed.get("width") != 64 or observed.get("height") != 64:
            raise RuntimeError("native viewport drawing did not complete")
        if (
            observed.get("pixel_count") != 64 * 64
            or observed.get("unique_rgb", 0) < 2
            or observed.get("max_rgb", 0) <= 0
        ):
            raise RuntimeError("native viewport pixels are empty, black, or uniform")
        display = str(observed.get("display", ""))
        if not display.startswith(":") or not display[1:].isdigit():
            raise RuntimeError("UI display is not a private local X display")
        if require_nvidia and "NVIDIA" not in str(observed.get("vendor", "")).upper():
            raise RuntimeError("native viewport is not using the required NVIDIA GPU")
        print(json.dumps({"ui_probe": observed, "after_restore": restored}))


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--host", required=True)
    parser.add_argument("--port", type=int, default=9876)
    parser.add_argument(
        "--mode",
        choices=(
            "capabilities",
            "gpu-eevee",
            "gpu-cycles",
            "ui",
            "gpu-ui",
            "busy-wait",
            "unresponsive-execute",
            "overdue-wait",
            "partial-frame",
        ),
        default="capabilities",
    )
    args = parser.parse_args()
    if args.mode == "capabilities":
        capability_smoke(args.host, args.port)
    elif args.mode == "gpu-eevee":
        gpu_eevee_smoke(args.host, args.port)
    elif args.mode == "gpu-cycles":
        gpu_cycles_smoke(args.host, args.port)
    elif args.mode in {"ui", "gpu-ui"}:
        ui_smoke(args.host, args.port, require_nvidia=args.mode == "gpu-ui")
        if args.mode == "gpu-ui":
            native_observation_smoke(args.host, args.port)
    elif args.mode == "busy-wait":
        busy_wait(args.host, args.port)
    elif args.mode == "unresponsive-execute":
        unresponsive_execute(args.host, args.port)
    elif args.mode == "overdue-wait":
        overdue_wait(args.host, args.port)
    else:
        partial_frame(args.host, args.port)
    return 0


if __name__ == "__main__":
    sys.exit(main())
