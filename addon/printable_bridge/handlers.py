"""Blender main-thread command handlers."""

from __future__ import annotations

from contextlib import ExitStack, contextmanager
import hashlib
import math
from pathlib import Path
import time
from itertools import islice
from typing import Any, Callable, Iterator

from .config import BridgeConfig
from .native_view import DEFAULT_CAPTURE_TIMEOUT_SECONDS, NativeViewError, capture as capture_native_view
from .state import SceneState
from .editor_context import EditorContextError, context_summary, editing_state, execution_context
from .inspection import InspectionError, node_tree_info, object_details, page_arguments
from .execution import (
    DEFAULT_TIMEOUT_SECONDS,
    MAX_OUTPUT_BYTES,
    MAX_RESULT_BYTES,
    CodeExecutionError,
    execute_code,
)
from .watchdog import ExecutionWatchdog, NoopExecutionWatchdog, WatchdogError
from .workspace import SecureWorkspace, WorkspacePath


DEFAULT_RENDER_TIMEOUT_SECONDS = 3600.0
MAX_RENDER_VIEWS = 36
MAX_RENDER_VIEW_PIXELS = 64 * 1024 * 1024
MAX_REVIEW_SOURCE_PIXELS = 8 * 1024 * 1024
MAX_DIAGNOSTIC_VERTICES = 1_000_000
MAX_DIAGNOSTIC_EDGES = 3_000_000
MAX_DIAGNOSTIC_FACES = 2_000_000
MAX_DIAGNOSTIC_LOOPS = 6_000_000
MAX_DIAGNOSTIC_ATTRIBUTE_VALUES = 16_000_000
MAX_JOB_ANALYSIS_MESH_BYTES = 1024 * 1024 * 1024
PRODUCT_PRESENTATION_MARGIN = 1.15
MAX_PRODUCT_MATERIAL_OVERRIDES = 64
MAX_PRODUCT_RENDER_PIXELS = 16 * 1024 * 1024
MAX_PRODUCT_RENDER_BYTES = 64 * 1024 * 1024
MAX_PRODUCT_INSTANCES = 4096
MAX_PRODUCT_VERTICES = MAX_DIAGNOSTIC_VERTICES
MAX_PRODUCT_EDGES = MAX_DIAGNOSTIC_EDGES
MAX_PRODUCT_FACES = MAX_DIAGNOSTIC_FACES
MAX_PRODUCT_LOOPS = MAX_DIAGNOSTIC_LOOPS
MAX_PRODUCT_ATTRIBUTE_VALUES = MAX_DIAGNOSTIC_ATTRIBUTE_VALUES
MAX_PRODUCT_MATERIAL_SLOTS = 4096

PRODUCT_PRESENTATION_PROFILES = {
    "engineering": {
        "camera_type": "ORTHO",
        "lens_mm": None,
        "view_transform": "Khronos PBR Neutral",
        "world_color_srgb": (0.18, 0.18, 0.18),
        "world_strength": 0.8,
        "ground": None,
        "default_shading": "preserve",
        "lights": (
            ("key", (1.5, -1.5, 2.0), 1000.0, 1.5),
            ("fill", (-1.0, 0.5, 1.0), 500.0, 1.5),
        ),
    },
    "studio_neutral": {
        "camera_type": "PERSP",
        "lens_mm": 70.0,
        "view_transform": "Khronos PBR Neutral",
        "world_color_srgb": (0.055, 0.055, 0.055),
        "world_strength": 0.65,
        "ground": {
            "base_color_srgb": (0.18, 0.18, 0.18),
            "metallic": 0.0,
            "roughness": 0.72,
        },
        "default_shading": "smooth_by_angle",
        "lights": (
            ("key", (1.6, -1.8, 2.2), 1000.0, 1.8),
            ("fill", (-1.5, -0.3, 1.2), 350.0, 2.2),
            ("rim", (0.4, 1.8, 2.0), 650.0, 1.3),
        ),
    },
    "studio_dark": {
        "camera_type": "PERSP",
        "lens_mm": 85.0,
        "view_transform": "AgX",
        "world_color_srgb": (0.008, 0.015, 0.028),
        "world_strength": 0.45,
        "ground": {
            "base_color_srgb": (0.012, 0.022, 0.038),
            "metallic": 0.0,
            "roughness": 0.58,
        },
        "default_shading": "smooth_by_angle",
        "lights": (
            ("key", (1.7, -1.7, 2.1), 1100.0, 1.5),
            ("fill", (-1.5, -0.4, 1.0), 220.0, 2.0),
            ("rim", (0.3, 1.7, 2.2), 900.0, 1.0),
        ),
    },
}


def _enforce_diagnostic_topology_limits(
    vertices: int, edges: int, faces: int, loops: int, stage: str
) -> None:
    for kind, count, limit in (
        ("vertices", vertices, MAX_DIAGNOSTIC_VERTICES),
        ("edges", edges, MAX_DIAGNOSTIC_EDGES),
        ("faces", faces, MAX_DIAGNOSTIC_FACES),
        ("loops", loops, MAX_DIAGNOSTIC_LOOPS),
    ):
        if count > limit:
            raise HandlerError(
                f"diagnostic {stage} geometry exceeds {limit} {kind}; hide unrelated objects or use objects to render a subset"
            )


def _enforce_product_geometry_limits(usage: dict[str, int]) -> None:
    for kind, limit in (
        ("instances", MAX_PRODUCT_INSTANCES),
        ("vertices", MAX_PRODUCT_VERTICES),
        ("edges", MAX_PRODUCT_EDGES),
        ("faces", MAX_PRODUCT_FACES),
        ("loops", MAX_PRODUCT_LOOPS),
        ("attribute_values", MAX_PRODUCT_ATTRIBUTE_VALUES),
        ("material_slots", MAX_PRODUCT_MATERIAL_SLOTS),
    ):
        if usage[kind] > limit:
            readable = kind.replace("_", " ")
            raise HandlerError(
                f"product presentation exceeds {limit} evaluated {readable}; reduce instancing or select a smaller object subset"
            )


class HandlerError(ValueError):
    """A caller-visible command validation or execution error."""


class HandlerStartupError(RuntimeError):
    """A safe operator-visible Blender initialization error."""


def _string(params: dict[str, Any], name: str) -> str:
    value = params.get(name)
    if not isinstance(value, str) or not value or len(value.encode("utf-8")) > 255:
        raise HandlerError(f"{name} must be a non-empty string")
    return value


def _positive_integer(
    params: dict[str, Any],
    name: str,
    default: int,
    maximum: int,
    minimum: int = 1,
) -> int:
    value = params.get(name, default)
    if (
        isinstance(value, bool)
        or not isinstance(value, int)
        or not minimum <= value <= maximum
    ):
        raise HandlerError(
            f"{name} must be an integer between {minimum} and {maximum}"
        )
    return value


def _nonnegative_integer(
    params: dict[str, Any], name: str, default: int, maximum: int
) -> int:
    value = params.get(name, default)
    if (
        isinstance(value, bool)
        or not isinstance(value, int)
        or not 0 <= value <= maximum
    ):
        raise HandlerError(f"{name} must be an integer between 0 and {maximum}")
    return value


def _positive_float(params: dict[str, Any], name: str, default: float) -> float:
    value = params.get(name, default)
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise HandlerError(f"{name} must be a positive finite number")
    number = float(value)
    if not math.isfinite(number) or number <= 0:
        raise HandlerError(f"{name} must be a positive finite number")
    return number


def _optional_output_budget(params: dict[str, Any]) -> int | None:
    value = params.get("max_output_bytes")
    if value is None:
        return None
    if isinstance(value, bool) or not isinstance(value, int) or value <= 0:
        raise HandlerError("max_output_bytes must be a positive integer")
    return value


def _finite_float(params: dict[str, Any], name: str) -> float:
    value = params.get(name)
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise HandlerError(f"{name} must be a finite number")
    try:
        number = float(value)
    except OverflowError as error:
        raise HandlerError(f"{name} must be a finite number") from error
    if not math.isfinite(number):
        raise HandlerError(f"{name} must be a finite number")
    return number


def _bounded_float(
    params: dict[str, Any], name: str, default: float, minimum: float, maximum: float
) -> float:
    value = params.get(name, default)
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise HandlerError(
            f"{name} must be a finite number from {minimum:g} through {maximum:g}"
        )
    number = float(value)
    if not math.isfinite(number) or not minimum <= number <= maximum:
        raise HandlerError(
            f"{name} must be a finite number from {minimum:g} through {maximum:g}"
        )
    return number


def _finite_vector(
    params: dict[str, Any],
    name: str,
    default: tuple[float, float, float] | None = None,
) -> tuple[float, float, float]:
    value = params.get(name) if default is None else params.get(name, list(default))
    if not isinstance(value, list) or len(value) != 3:
        raise HandlerError(f"{name} must contain three finite numbers")
    components: list[float] = []
    for component in value:
        if isinstance(component, bool) or not isinstance(component, (int, float)):
            raise HandlerError(f"{name} must contain three finite numbers")
        try:
            number = float(component)
        except OverflowError as error:
            raise HandlerError(f"{name} must contain three finite numbers") from error
        if not math.isfinite(number):
            raise HandlerError(f"{name} must contain three finite numbers")
        components.append(number)
    return tuple(components)


def _render_parameters(
    params: dict[str, Any],
    *,
    default_width: int = 512,
    default_height: int = 512,
) -> tuple[int, int, float, str, int | None]:
    width = _positive_integer(params, "width", default_width, 8192)
    height = _positive_integer(params, "height", default_height, 8192)
    timeout_seconds = _positive_float(
        params, "timeout_seconds", DEFAULT_RENDER_TIMEOUT_SECONDS
    )
    engine = params.get("engine", "EEVEE")
    if not isinstance(engine, str) or engine not in {"EEVEE", "CYCLES"}:
        raise HandlerError("engine must be EEVEE or CYCLES")
    if engine == "EEVEE" and "samples" in params:
        raise HandlerError("samples is only valid for CYCLES renders")
    samples = (
        _positive_integer(params, "samples", 128, 4096)
        if engine == "CYCLES"
        else None
    )
    return width, height, timeout_seconds, engine, samples


def _normalized_vector(
    params: dict[str, Any],
    name: str,
    default: tuple[float, float, float] | None = None,
) -> tuple[float, float, float]:
    components = _finite_vector(params, name, default)
    scale = max(abs(component) for component in components)
    if scale == 0.0:
        raise HandlerError(f"{name} must have a finite non-zero magnitude")
    scaled = tuple(component / scale for component in components)
    magnitude = math.hypot(*scaled)
    return tuple(component / magnitude for component in scaled)


def _optional_object_names(params: dict[str, Any]) -> set[str] | None:
    value = params.get("objects")
    if value is None:
        return None
    if not isinstance(value, list) or not 1 <= len(value) <= 1000:
        raise HandlerError("objects must contain between 1 and 1000 unique names")
    names: set[str] = set()
    for item in value:
        if not isinstance(item, str) or not item or len(item.encode("utf-8")) > 255:
            raise HandlerError("objects must contain between 1 and 1000 unique names")
        if item in names:
            raise HandlerError("objects must contain unique names")
        names.add(item)
    return names


def _required_object_names(params: dict[str, Any]) -> list[str]:
    return _required_named_object_names(params, "objects")


def _required_named_object_names(
    params: dict[str, Any], name: str
) -> list[str]:
    value = params.get(name)
    if not isinstance(value, list) or not 1 <= len(value) <= 1000:
        raise HandlerError(
            f"{name} must contain between 1 and 1000 unique names"
        )
    names: list[str] = []
    seen: set[str] = set()
    for item in value:
        if not isinstance(item, str) or not item or len(item.encode("utf-8")) > 255:
            raise HandlerError(
                f"{name} must contain between 1 and 1000 unique names"
            )
        if item in seen:
            raise HandlerError(f"{name} must contain unique names")
        seen.add(item)
        names.append(item)
    return names


def _only_keys(params: dict[str, Any], allowed: set[str]) -> None:
    unknown = sorted(set(params).difference(allowed))
    if unknown:
        raise HandlerError(f"unknown parameter: {unknown[0]}")


def _optional_name(params: dict[str, Any]) -> str | None:
    if "name" not in params:
        return None
    return _string(params, "name")


class BlenderHandlers:
    def __init__(
        self,
        config: BridgeConfig,
        shutdown_requested: Callable[[], bool],
        bpy_module: Any | None = None,
        execution_watchdog: ExecutionWatchdog | None = None,
        owned_display_pid: int | None = None,
    ):
        if execution_watchdog is None:
            if bpy_module is None:
                raise HandlerStartupError("execution watchdog is required")
            execution_watchdog = NoopExecutionWatchdog()
        if bpy_module is None:
            import bpy as bpy_module  # type: ignore[import-not-found]

        self._bpy = bpy_module
        self._config = config
        if config.role == "render_worker" and not bpy_module.app.background:
            raise HandlerStartupError("render_worker requires background Blender")
        self._shutdown_requested = shutdown_requested
        self._execution_watchdog = execution_watchdog
        self._owned_display_pid = owned_display_pid
        self._scene_state = SceneState()
        self._dispatching = False
        self._state_callbacks: list[tuple[Any, Any]] = []
        self._cycles_devices = self._configure_render_device()
        self._workspace = SecureWorkspace(
            config.workspace_root, config.state_dir / "staging"
        )
        self._registry: dict[str, Callable[[dict[str, Any]], Any]] = {
            "boolean": self._boolean,
            "bridge_status": self._bridge_status,
            "clear_scene": self._clear_scene,
            "create_primitive": self._create_primitive,
            "animate_rotation": self._animate_rotation,
            "execute_code": self._execute_code,
            "export_stl": self._export_stl,
            "get_object_info": self._get_object_info,
            "get_node_tree_info": self._get_node_tree_info,
            "get_editing_state": self._get_editing_state,
            "get_scene_info": self._get_scene_info,
            "capture_native_view": self._capture_native_view,
            "import_stl": self._import_stl,
            "open_project": self._open_project,
            "attach_cad": self._attach_cad,
            "job_measure_sequence_bounds": self._job_measure_sequence_bounds,
            "job_render_product": self._job_render_product,
            "job_render_frame": self._job_render_frame,
            "job_render_still": self._job_render_still,
            "job_render_views": self._job_render_views,
            "job_prepare_mechanical_rotation": self._job_prepare_mechanical_rotation,
            "job_restore_checkpoint": self._job_restore_checkpoint,
            "job_save_checkpoint": self._job_save_checkpoint,
            "rename_object": self._rename_object,
            "render_still": self._render_still,
            "render_diagnostic": self._render_diagnostic,
            "render_product": self._render_product,
            "render_views": self._render_views,
            "restore_checkpoint": self._restore_checkpoint,
            "save_blend": self._save_blend,
        }
        if config.enable_test_commands:
            self._registry["bridge_test_wait"] = self._bridge_test_wait

    @property
    def commands(self) -> tuple[str, ...]:
        return tuple(sorted(self._registry))

    def dispatch(self, command: str, params: dict[str, Any]) -> Any:
        handler = self._registry.get(command)
        if handler is None:
            raise HandlerError(
                f"unknown command {command}; available commands: {', '.join(self.commands)}"
            )
        params = dict(params)
        if self._state_callbacks:
            self._bpy.context.view_layer.update()
        self._scene_state.begin(command, params.pop("expected_scene", None))
        self._dispatching = True
        try:
            result = handler(params)
            if self._state_callbacks:
                self._bpy.context.view_layer.update()
            return result
        finally:
            self._dispatching = False

    @property
    def scene_state(self) -> dict[str, Any]:
        return self._scene_state.snapshot

    def install_state_observers(self) -> None:
        handlers = self._bpy.app.handlers

        @handlers.persistent
        def on_load(_unused: Any) -> None:
            self._scene_state.bind_project(self._loaded_project())

        @handlers.persistent
        def on_save(_unused: Any) -> None:
            project = self._scene_state.snapshot.get("project_id")
            if project is not None:
                for scene in self._bpy.data.scenes:
                    scene["printable_project_id"] = project

        @handlers.persistent
        def on_update(_scene: Any, graph: Any) -> None:
            if not self._dispatching and any(graph.updates):
                self._scene_state.begin("external_update")

        for callbacks, callback in (
            (handlers.load_post, on_load),
            (handlers.save_pre, on_save),
            (handlers.depsgraph_update_post, on_update),
        ):
            callbacks.append(callback)
            self._state_callbacks.append((callbacks, callback))

    def close(self) -> None:
        for callbacks, callback in self._state_callbacks:
            if callback in callbacks:
                callbacks.remove(callback)
        self._state_callbacks.clear()
        self._workspace.close()

    def _configure_render_device(self) -> list[dict[str, Any]]:
        if self._config.render_device == "CPU":
            for scene in self._bpy.data.scenes:
                scene.cycles.device = "CPU"
            return []

        cycles_addon = self._bpy.context.preferences.addons.get("cycles")
        if cycles_addon is None:
            raise HandlerStartupError(
                "OPTIX requested but the Cycles add-on is unavailable"
            )
        preferences = cycles_addon.preferences
        try:
            preferences.compute_device_type = "OPTIX"
            preferences.get_devices()
        except (AttributeError, RuntimeError, TypeError, ValueError) as error:
            raise HandlerStartupError(
                "OPTIX requested but no compatible device is available"
            ) from error
        devices = list(preferences.devices)
        optix_devices = [device for device in devices if device.type == "OPTIX"]
        if not optix_devices:
            raise HandlerStartupError(
                "OPTIX requested but no compatible device is available"
            )
        for device in devices:
            device.use = device in optix_devices
        for scene in self._bpy.data.scenes:
            scene.cycles.device = "GPU"
        return [
            {
                "name": str(device.name),
                "type": str(device.type),
                "enabled": bool(device.use),
            }
            for device in devices
        ]

    def _bridge_status(self, params: dict[str, Any]) -> dict[str, Any]:
        _only_keys(params, set())
        return {
            "blender_version": self._bpy.app.version_string,
            "role": self._config.role,
            "background": self._bpy.app.background,
            "render_device": self._config.render_device,
            "cycles_devices": self._cycles_devices,
            "commands": list(self.commands),
            "native_observation": {
                "available": not self._bpy.app.background,
                "mode": "background" if self._bpy.app.background else "ui",
                "methods": [] if self._bpy.app.background else ["viewport", "editor"],
            },
            "execution_limits": {
                "max_output_bytes": MAX_OUTPUT_BYTES,
                "max_result_bytes": MAX_RESULT_BYTES,
                "default_timeout_seconds": DEFAULT_TIMEOUT_SECONDS,
                "timeout_policy": (
                    "caller-selected positive runtime-representable seconds; "
                    "no configured maximum"
                ),
                "timeout_recovery": (
                    "watchdog restart; persistent unhealthy state requires "
                    "container restart"
                ),
                "output_capture": (
                    "Python, native, and inherited subprocess stdout/stderr"
                ),
                "background_work_policy": "must finish before execute_code returns",
                "timeout_outcome": (
                    "unknown after request delivery; never automatically retry"
                ),
            },
        }

    def _get_scene_info(self, params: dict[str, Any]) -> dict[str, Any]:
        _only_keys(params, {"offset", "limit", "name_contains", "object_type", "collection", "include_transforms"})
        offset = _nonnegative_integer(params, "offset", 0, 1_000_000)
        limit = _positive_integer(params, "limit", 100, 1000)
        filters = {}
        for key in ("name_contains", "object_type", "collection"):
            if key in params:
                filters[key] = _string(params, key)
                if len(filters[key]) > 255:
                    raise HandlerError(f"{key} must be at most 255 characters")
        include_transforms = params.get("include_transforms", True)
        if type(include_transforms) is not bool:
            raise HandlerError("include_transforms must be boolean")
        scene = self._bpy.context.scene
        object_count = len(scene.objects)
        if "collection" in filters and self._bpy.data.collections.get(filters["collection"]) is None:
            raise HandlerError(f"collection not found: {filters['collection']}")
        objects = []
        next_offset = offset
        for obj in islice(scene.objects, offset, offset + 10_000):
            next_offset += 1
            if "name_contains" in filters and filters["name_contains"].casefold() not in obj.name.casefold():
                continue
            if "object_type" in filters and obj.type != filters["object_type"]:
                continue
            if "collection" in filters and not any(col.name == filters["collection"] for col in obj.users_collection):
                continue
            objects.append(self._object_summary(obj) if include_transforms else {"name": obj.name, "type": obj.type})
            if len(objects) == limit:
                break
        return {
            "blender_version": self._bpy.app.version_string,
            "scene": scene.name,
            "active_object": (
                self._bpy.context.view_layer.objects.active.name
                if self._bpy.context.view_layer.objects.active is not None
                else None
            ),
            "object_count": object_count,
            "offset": offset,
            "limit": limit,
            "next_offset": next_offset if next_offset < object_count else None,
            "objects": objects,
        }

    def _get_object_info(self, params: dict[str, Any]) -> dict[str, Any]:
        _only_keys(params, {"name", "section", "offset", "limit"})
        name = _string(params, "name")
        section = params.get("section", "summary")
        try:
            offset, limit = page_arguments(params)
        except InspectionError as error:
            raise HandlerError(str(error)) from error
        obj = self._bpy.data.objects.get(name)
        if obj is None:
            raise HandlerError(f"object not found: {name}")
        if section == "summary":
            return self._object_summary(obj)
        try:
            return {"name": obj.name, "section": section, **object_details(obj, section, offset, limit)}
        except InspectionError as error:
            raise HandlerError(str(error)) from error

    def _get_node_tree_info(self, params: dict[str, Any]) -> dict[str, Any]:
        try:
            return node_tree_info(self._bpy, params)
        except InspectionError as error:
            raise HandlerError(str(error)) from error

    def _get_editing_state(self, params: dict[str, Any]) -> dict[str, Any]:
        try:
            return editing_state(self._bpy, params)
        except InspectionError as error:
            raise HandlerError(str(error)) from error

    def _clear_scene(self, params: dict[str, Any]) -> dict[str, Any]:
        _only_keys(params, set())
        scene = self._bpy.context.scene
        removed = len(scene.objects)
        root = scene.collection
        candidate_objects = list(scene.objects)
        protected_collections: set[int] = set()
        for other_scene in self._bpy.data.scenes:
            if other_scene is scene:
                continue
            pending = [other_scene.collection]
            while pending:
                collection = pending.pop()
                identity = id(collection)
                if identity in protected_collections:
                    continue
                protected_collections.add(identity)
                pending.extend(collection.children)

        view_layer = self._bpy.context.view_layer
        view_layer.active_layer_collection = view_layer.layer_collection
        view_layer.objects.active = None
        for obj in list(root.objects):
            root.objects.unlink(obj)

        visited_collections: set[int] = set()
        pending = [(root, collection) for collection in list(root.children)]
        while pending:
            parent, collection = pending.pop()
            identity = id(collection)
            if identity in protected_collections:
                parent.children.unlink(collection)
                continue
            if identity in visited_collections:
                continue
            visited_collections.add(identity)
            for obj in list(collection.objects):
                collection.objects.unlink(obj)
            pending.extend(
                (collection, child) for child in list(collection.children)
            )

        view_layer.update()
        if len(root.children) == 0:
            workspace_collection = self._bpy.data.collections.new("Printable")
            root.children.link(workspace_collection)
            view_layer.update()
        view_layer.active_layer_collection = next(
            iter(view_layer.layer_collection.children)
        )

        unused_objects = [obj for obj in candidate_objects if obj.users == 0]
        if unused_objects:
            self._bpy.data.batch_remove(unused_objects)
            view_layer.update()
        return {
            "removed_objects": removed,
            "freed_objects": len(unused_objects),
        }

    def _rename_object(self, params: dict[str, Any]) -> dict[str, Any]:
        _only_keys(params, {"name", "new_name"})
        name = _string(params, "name")
        new_name = _string(params, "new_name")
        obj = self._bpy.data.objects.get(name)
        if obj is None:
            raise HandlerError(f"object not found: {name}")
        collision = self._bpy.data.objects.get(new_name)
        if collision is not None and collision is not obj:
            raise HandlerError(f"object already exists: {new_name}")
        obj.name = new_name
        return self._object_summary(obj)

    def _animate_rotation(self, params: dict[str, Any]) -> dict[str, Any]:
        _only_keys(
            params,
            {
                "objects",
                "controller_name",
                "pivot",
                "axis",
                "angle_degrees",
                "frame_start",
                "frame_end",
            },
        )
        object_names = _required_object_names(params)
        controller_name = _string(params, "controller_name")
        pivot = _finite_vector(params, "pivot")
        axis = _normalized_vector(params, "axis")
        angle_degrees = _finite_float(params, "angle_degrees")
        if angle_degrees <= 0.0:
            raise HandlerError("angle_degrees must be a positive finite number")
        frame_start = _positive_integer(
            params, "frame_start", 1, 1_048_574, -1_048_574
        )
        frame_end = _positive_integer(
            params, "frame_end", 250, 1_048_574, -1_048_574
        )
        if frame_start >= frame_end:
            raise HandlerError("frame_start must be less than frame_end")

        if self._bpy.data.objects.get(controller_name) is not None:
            raise HandlerError(f"object already exists: {controller_name}")
        objects = []
        for name in object_names:
            obj = self._bpy.data.objects.get(name)
            if obj is None:
                raise HandlerError(f"object not found: {name}")
            if obj.parent is not None:
                raise HandlerError(f"object already has a parent: {name}")
            if len(obj.children) != 0:
                raise HandlerError(f"object already has children: {name}")
            if obj.animation_data is not None:
                raise HandlerError(f"object already has animation data: {name}")
            if len(obj.constraints) != 0:
                raise HandlerError(f"object already has constraints: {name}")
            if obj.rigid_body is not None:
                raise HandlerError(f"object already has rigid body simulation: {name}")
            if obj.rigid_body_constraint is not None:
                raise HandlerError(f"object already has a rigid body constraint: {name}")
            objects.append(obj)

        edit_preferences = self._bpy.context.preferences.edit
        original_interpolation = edit_preferences.keyframe_new_interpolation_type
        scene = self._bpy.context.scene
        original_frame = scene.frame_current
        original_subframe = scene.frame_subframe
        original_transforms = [
            (obj, obj.matrix_world.copy(), obj.matrix_parent_inverse.copy())
            for obj in objects
        ]
        controller = self._bpy.data.objects.new(controller_name, None)
        interpolation_changed = False
        try:
            controller.empty_display_type = "PLAIN_AXES"
            controller.location = pivot
            controller.rotation_mode = "AXIS_ANGLE"
            scene.collection.objects.link(controller)
            self._bpy.context.view_layer.update()
            controller_inverse = controller.matrix_world.inverted()

            for obj, _, _ in original_transforms:
                obj.parent = controller
                obj.matrix_parent_inverse = controller_inverse.copy()

            edit_preferences.keyframe_new_interpolation_type = "LINEAR"
            interpolation_changed = True
            controller.rotation_axis_angle = (0.0, *axis)
            if not controller.keyframe_insert(
                data_path="rotation_axis_angle", frame=frame_start
            ):
                raise HandlerError("could not insert the starting rotation keyframe")
            controller.rotation_axis_angle = (math.radians(angle_degrees), *axis)
            if not controller.keyframe_insert(
                data_path="rotation_axis_angle", frame=frame_end
            ):
                raise HandlerError("could not insert the ending rotation keyframe")
            rotation_fcurves = self._rotation_action_fcurves(controller)
            for fcurve in rotation_fcurves.values():
                for keyframe in fcurve.keyframe_points:
                    keyframe.interpolation = "LINEAR"
                fcurve.update()
            motion = self._read_authored_rotation(
                controller, objects, rotation_fcurves
            )
            edit_preferences.keyframe_new_interpolation_type = original_interpolation
            interpolation_changed = False
            scene.frame_set(original_frame, subframe=original_subframe)
        except Exception as error:
            rollback_errors: list[str] = []
            if interpolation_changed:
                try:
                    edit_preferences.keyframe_new_interpolation_type = (
                        original_interpolation
                    )
                except Exception as rollback:
                    rollback_errors.append(f"interpolation restore failed: {rollback}")
            for obj, world_matrix, parent_inverse in original_transforms:
                try:
                    obj.parent = None
                    obj.matrix_parent_inverse = parent_inverse
                    obj.matrix_world = world_matrix
                except Exception as rollback:
                    rollback_errors.append(
                        f"object transform restore failed for {obj.name}: {rollback}"
                    )
            try:
                animation_data = getattr(controller, "animation_data", None)
                action = (
                    getattr(animation_data, "action", None)
                    if animation_data is not None
                    else None
                )
                if action is not None:
                    controller.animation_data_clear()
                self._bpy.data.objects.remove(controller, do_unlink=True)
                if action is not None and action.users == 0:
                    self._bpy.data.actions.remove(action)
            except Exception as rollback:
                rollback_errors.append(f"controller cleanup failed: {rollback}")
            try:
                scene.frame_set(original_frame, subframe=original_subframe)
            except Exception as rollback:
                rollback_errors.append(f"frame restore failed: {rollback}")
            if rollback_errors:
                raise HandlerError(
                    f"rotation authoring failed and rollback was incomplete: {'; '.join(rollback_errors)}"
                ) from error
            raise

        return motion

    @staticmethod
    def _rotation_action_fcurves(controller: Any) -> dict[int, Any]:
        animation_data = controller.animation_data
        action = animation_data.action if animation_data is not None else None
        layers = list(action.layers) if action is not None else []
        if len(layers) != 1 or len(layers[0].strips) != 1:
            raise HandlerError("authored rotation action has an unexpected structure")
        channelbag = layers[0].strips[0].channelbag(animation_data.action_slot)
        if channelbag is None:
            raise HandlerError("authored rotation action has no controller channels")
        matching = [
            fcurve
            for fcurve in channelbag.fcurves
            if fcurve.data_path == "rotation_axis_angle"
        ]
        fcurves = {fcurve.array_index: fcurve for fcurve in matching}
        if len(matching) != 4 or set(fcurves) != {0, 1, 2, 3}:
            raise HandlerError("authored rotation action has incomplete axis-angle channels")
        if any(len(fcurve.modifiers) != 0 for fcurve in fcurves.values()):
            raise HandlerError("authored rotation action has unexpected F-curve modifiers")
        return fcurves

    @staticmethod
    def _read_authored_rotation(
        controller: Any, objects: list[Any], fcurves: dict[int, Any]
    ) -> dict[str, Any]:
        channel_points: dict[int, list[tuple[float, float]]] = {}
        for index, fcurve in fcurves.items():
            keyframes = list(fcurve.keyframe_points)
            if len(keyframes) != 2 or any(
                keyframe.interpolation != "LINEAR" for keyframe in keyframes
            ):
                raise HandlerError("authored rotation action is not one linear segment")
            points = sorted(
                (float(keyframe.co[0]), float(keyframe.co[1]))
                for keyframe in keyframes
            )
            if not all(math.isfinite(value) for point in points for value in point):
                raise HandlerError("authored rotation action contains non-finite keyframes")
            channel_points[index] = points

        start_frames = {points[0][0] for points in channel_points.values()}
        end_frames = {points[1][0] for points in channel_points.values()}
        if len(start_frames) != 1 or len(end_frames) != 1:
            raise HandlerError("authored rotation channels use different keyframe times")
        start_frame = start_frames.pop()
        end_frame = end_frames.pop()
        if (
            not start_frame.is_integer()
            or not end_frame.is_integer()
            or start_frame >= end_frame
        ):
            raise HandlerError("authored rotation keyframe times are invalid")

        start = [channel_points[index][0][1] for index in range(4)]
        end = [channel_points[index][1][1] for index in range(4)]
        pivot = [float(component) for component in controller.location]
        if (
            controller.rotation_mode != "AXIS_ANGLE"
            or start[0] != 0.0
            or start[1:] != end[1:]
            or end[0] <= 0.0
            or not all(math.isfinite(value) for value in [*pivot, *start, *end])
        ):
            raise HandlerError("authored rotation values are invalid")
        return {
            "controller": str(controller.name),
            "objects": [str(obj.name) for obj in objects],
            "pivot": pivot,
            "axis": end[1:],
            "angle_degrees": math.degrees(end[0]),
            "frame_start": int(start_frame),
            "frame_end": int(end_frame),
            "interpolation": "LINEAR",
        }

    def _job_prepare_mechanical_rotation(
        self, params: dict[str, Any]
    ) -> dict[str, Any]:
        _only_keys(
            params,
            {
                "fixed_objects",
                "moving_objects",
                "controller_name",
                "pivot",
                "axis",
                "angle_degrees",
                "frame_start",
                "frame_end",
                "fixed_path",
                "moving_path",
                "max_output_bytes",
                "timeout_seconds",
            },
        )
        fixed_names = _required_named_object_names(params, "fixed_objects")
        moving_names = _required_named_object_names(params, "moving_objects")
        if not set(fixed_names).isdisjoint(moving_names):
            raise HandlerError("fixed_objects and moving_objects must be disjoint")
        controller_name = _string(params, "controller_name")
        pivot = _finite_vector(params, "pivot")
        axis = _normalized_vector(params, "axis")
        angle_degrees = _finite_float(params, "angle_degrees")
        if angle_degrees <= 0.0:
            raise HandlerError("angle_degrees must be a positive finite number")
        frame_start = _positive_integer(
            params, "frame_start", 1, 1_048_574, -1_048_574
        )
        frame_end = _positive_integer(
            params, "frame_end", 250, 1_048_574, -1_048_574
        )
        if frame_start >= frame_end:
            raise HandlerError("frame_start must be less than frame_end")
        max_output_bytes = _optional_output_budget(params)
        if (
            max_output_bytes is None
            or max_output_bytes > MAX_JOB_ANALYSIS_MESH_BYTES
        ):
            raise HandlerError(
                "max_output_bytes must be a positive integer no greater than 1073741824"
            )
        timeout_seconds = _positive_float(
            params, "timeout_seconds", DEFAULT_RENDER_TIMEOUT_SECONDS
        )
        fixed_request = self._workspace.validate_reserved(
            params.get("fixed_path"), ".stl"
        )
        moving_request = self._workspace.validate_reserved(
            params.get("moving_path"), ".stl"
        )
        if fixed_request.relative == moving_request.relative:
            raise HandlerError("fixed_path and moving_path must be different")
        if self._bpy.data.objects.get(controller_name) is not None:
            raise HandlerError(f"object already exists: {controller_name}")

        render_visible = self._stable_render_layer_objects()
        fixed_objects = self._mechanical_objects(fixed_names, render_visible)
        moving_objects = self._mechanical_objects(moving_names, render_visible)
        classified = set(fixed_names) | set(moving_names)
        scene_objects = list(self._bpy.context.scene.objects)
        instancers = sorted(
            obj.name
            for obj in scene_objects
            if getattr(obj, "instance_type", "NONE") != "NONE"
        )
        if instancers:
            raise HandlerError(
                f"mechanical scene must not contain instancers: {', '.join(instancers)}"
            )
        unsupported = sorted(
            obj.name
            for obj in scene_objects
            if obj.type not in {"MESH", "CAMERA", "LIGHT"}
        )
        if unsupported:
            raise HandlerError(
                "mechanical scene contains unsupported non-mesh objects: "
                + ", ".join(unsupported)
            )
        scene_meshes = {obj.name for obj in scene_objects if obj.type == "MESH"}
        if classified != scene_meshes:
            omitted = sorted(scene_meshes - classified)
            unknown = sorted(classified - scene_meshes)
            detail = []
            if omitted:
                detail.append(f"unclassified scene meshes: {', '.join(omitted)}")
            if unknown:
                detail.append(f"objects outside the active scene: {', '.join(unknown)}")
            raise HandlerError("; ".join(detail))

        deadline = time.monotonic() + timeout_seconds
        try:
            self._execution_watchdog.arm(deadline)
        except WatchdogError as error:
            raise HandlerError("mechanical preparation watchdog is unavailable") from error
        try:
            with (
                self._workspace.stage_output(fixed_request) as fixed_output,
                self._workspace.stage_output(moving_request) as moving_output,
            ):
                fixed_size = self._export_mechanical_objects(
                    fixed_objects, fixed_output.path, max_output_bytes
                )
                moving_size = self._export_mechanical_objects(
                    moving_objects, moving_output.path, max_output_bytes
                )
                motion = self._animate_rotation(
                    {
                        "objects": moving_names,
                        "controller_name": controller_name,
                        "pivot": list(pivot),
                        "axis": list(axis),
                        "angle_degrees": angle_degrees,
                        "frame_start": frame_start,
                        "frame_end": frame_end,
                    }
                )
                fixed_output.commit()
                moving_output.commit()
        finally:
            try:
                self._execution_watchdog.disarm()
            except WatchdogError as error:
                raise HandlerError("mechanical preparation watchdog is unavailable") from error
        return {
            "fixed_path": fixed_request.relative,
            "fixed_size_bytes": fixed_size,
            "moving_path": moving_request.relative,
            "moving_size_bytes": moving_size,
            "motion": motion,
        }

    def _stable_render_layer_objects(self) -> set[str]:
        view_layer = self._bpy.context.view_layer
        if not view_layer.use:
            return set()
        visible: set[str] = set()
        pending = [(view_layer.layer_collection, False)]
        while pending:
            layer_collection, inherited_ineligible = pending.pop()
            collection = layer_collection.collection
            ineligible = inherited_ineligible or any(
                (
                    layer_collection.exclude,
                    layer_collection.holdout,
                    layer_collection.indirect_only,
                    collection.hide_render,
                    getattr(collection, "animation_data", None) is not None,
                )
            )
            if not ineligible:
                visible.update(obj.name for obj in collection.objects)
            pending.extend(
                (child, ineligible) for child in layer_collection.children
            )
        return visible

    def _mechanical_objects(
        self, names: list[str], render_visible: set[str]
    ) -> list[Any]:
        objects = []
        for name in names:
            obj = self._bpy.data.objects.get(name)
            if obj is None:
                raise HandlerError(f"object not found: {name}")
            if obj.type != "MESH":
                raise HandlerError(f"mechanical object must be a mesh: {name}")
            if obj.parent is not None or len(obj.children) != 0:
                raise HandlerError(f"mechanical object must have no hierarchy: {name}")
            if obj.animation_data is not None:
                raise HandlerError(f"mechanical object already has animation data: {name}")
            if len(obj.constraints) != 0:
                raise HandlerError(f"mechanical object already has constraints: {name}")
            if obj.rigid_body is not None or obj.rigid_body_constraint is not None:
                raise HandlerError(f"mechanical object uses rigid-body simulation: {name}")
            if len(obj.modifiers) != 0:
                raise HandlerError(f"mechanical object must have no modifiers: {name}")
            if getattr(obj.data, "animation_data", None) is not None:
                raise HandlerError(f"mechanical mesh data is animated: {name}")
            if getattr(obj.data, "shape_keys", None) is not None:
                raise HandlerError(f"mechanical object must have no shape keys: {name}")
            if getattr(obj, "instance_type", "NONE") != "NONE":
                raise HandlerError(f"mechanical object must not instance geometry: {name}")
            if getattr(obj, "hide_render", False):
                raise HandlerError(f"mechanical object is hidden from rendering: {name}")
            if not obj.visible_camera:
                raise HandlerError(
                    f"mechanical object is hidden from camera rays: {name}"
                )
            if name not in render_visible:
                raise HandlerError(
                    "mechanical object is not continuously visible in the active "
                    f"render view layer: {name}"
                )
            objects.append(obj)
        return objects

    def _export_mechanical_objects(
        self, objects: list[Any], path: Path, max_output_bytes: int
    ) -> int:
        self._bpy.ops.object.select_all(action="DESELECT")
        for obj in objects:
            obj.select_set(True)
        self._bpy.context.view_layer.objects.active = objects[0]
        result = self._bpy.ops.wm.stl_export(
            filepath=str(path),
            export_selected_objects=True,
            apply_modifiers=True,
        )
        self._require_finished(result, "mechanical STL export")
        size_bytes = path.stat().st_size
        if size_bytes > max_output_bytes:
            raise HandlerError(
                "mechanical STL exceeds the caller-selected output byte budget"
            )
        return size_bytes

    def _create_primitive(self, params: dict[str, Any]) -> dict[str, Any]:
        primitive = params.get("primitive", "cube")
        name = _optional_name(params)
        if primitive == "cube":
            _only_keys(params, {"primitive", "name", "size"})
            operation = self._bpy.ops.mesh.primitive_cube_add
            arguments = {"size": _positive_float(params, "size", 2.0)}
        elif primitive == "cylinder":
            _only_keys(
                params, {"primitive", "name", "vertices", "radius", "depth"}
            )
            operation = self._bpy.ops.mesh.primitive_cylinder_add
            arguments = {
                "vertices": _positive_integer(params, "vertices", 64, 1024, 3),
                "radius": _positive_float(params, "radius", 1.0),
                "depth": _positive_float(params, "depth", 2.0),
            }
        elif primitive == "uv_sphere":
            _only_keys(
                params,
                {"primitive", "name", "segments", "ring_count", "radius"},
            )
            operation = self._bpy.ops.mesh.primitive_uv_sphere_add
            arguments = {
                "segments": _positive_integer(params, "segments", 64, 1024, 3),
                "ring_count": _positive_integer(params, "ring_count", 32, 512, 3),
                "radius": _positive_float(params, "radius", 1.0),
            }
        else:
            raise HandlerError("primitive must be cube, cylinder, or uv_sphere")
        if name is not None and self._bpy.data.objects.get(name) is not None:
            raise HandlerError(f"object already exists: {name}")
        result = operation(**arguments)
        self._require_finished(result, "primitive creation")
        obj = self._bpy.context.active_object
        if name is not None:
            obj.name = name
        return self._object_summary(obj)

    def _boolean(self, params: dict[str, Any]) -> dict[str, Any]:
        _only_keys(
            params,
            {"target", "operand", "operation", "result_name", "delete_operand"},
        )
        target_name = _string(params, "target")
        operand_name = _string(params, "operand")
        operation = params.get("operation")
        if operation not in {"UNION", "DIFFERENCE", "INTERSECT"}:
            raise HandlerError("operation must be UNION, DIFFERENCE, or INTERSECT")
        result_name = (
            _string(params, "result_name") if "result_name" in params else None
        )
        delete_operand = params.get("delete_operand", False)
        if not isinstance(delete_operand, bool):
            raise HandlerError("delete_operand must be a boolean")

        target = self._bpy.data.objects.get(target_name)
        operand = self._bpy.data.objects.get(operand_name)
        if target is None:
            raise HandlerError(f"object not found: {target_name}")
        if operand is None:
            raise HandlerError(f"object not found: {operand_name}")
        if target is operand:
            raise HandlerError("target and operand must be different objects")
        if target.type != "MESH" or operand.type != "MESH":
            raise HandlerError("boolean target and operand must both be mesh objects")
        if result_name is not None:
            collision = self._bpy.data.objects.get(result_name)
            if collision is not None and collision is not target:
                raise HandlerError(f"object already exists: {result_name}")

        self._bpy.ops.object.select_all(action="DESELECT")
        target.select_set(True)
        self._bpy.context.view_layer.objects.active = target
        modifier = target.modifiers.new(name="PrintableBoolean", type="BOOLEAN")
        modifier.operation = operation
        modifier.solver = "EXACT"
        modifier.object = operand
        try:
            applied = self._bpy.ops.object.modifier_apply(modifier=modifier.name)
            self._require_finished(applied, "boolean operation")
        except (HandlerError, RuntimeError, TypeError, ValueError):
            if modifier in target.modifiers:
                target.modifiers.remove(modifier)
            raise

        if result_name is not None:
            target.name = result_name
        if delete_operand:
            self._bpy.data.objects.remove(operand, do_unlink=True)
        return self._object_summary(target)

    def _execute_code(self, params: dict[str, Any]) -> dict[str, Any]:
        _only_keys(params, {"code", "timeout_seconds", "context"})
        try:
            selector = params.get("context")
            with execution_context(self._bpy, selector):
                before = context_summary(self._bpy) if selector is not None else None
                output = execute_code(
                    params.get("code"),
                    params.get("timeout_seconds", DEFAULT_TIMEOUT_SECONDS),
                    self._bpy,
                    str(self._config.workspace_root),
                    self._shutdown_requested,
                    self._execution_watchdog,
                    owned_display_pid=self._owned_display_pid,
                )
                if selector is not None:
                    output["context"] = {"before": before, "after": context_summary(self._bpy)}
                return output
        except (CodeExecutionError, EditorContextError) as error:
            raise HandlerError(str(error)) from error

    def _export_stl(self, params: dict[str, Any]) -> dict[str, Any]:
        _only_keys(params, {"path", "selected_only"})
        request = self._workspace.validate(params.get("path"), ".stl")
        selected_only = params.get("selected_only", False)
        if not isinstance(selected_only, bool):
            raise HandlerError("selected_only must be a boolean")
        with self._workspace.stage_output(request) as output:
            result = self._bpy.ops.wm.stl_export(
                filepath=str(output.path),
                export_selected_objects=selected_only,
                apply_modifiers=True,
            )
            self._require_finished(result, "STL export")
            output.commit()
        return {"path": request.relative}

    def _import_stl(self, params: dict[str, Any]) -> dict[str, Any]:
        _only_keys(params, {"path"})
        request = self._workspace.validate(params.get("path"), ".stl")
        before = set(self._bpy.data.objects)
        with self._workspace.stage_input(request) as source:
            result = self._bpy.ops.wm.stl_import(filepath=str(source))
            self._require_finished(result, "STL import")
        imported = [obj for obj in self._bpy.data.objects if obj not in before]
        return {"objects": [self._object_summary(obj) for obj in imported]}

    def _loaded_project(self) -> str | None:
        project = self._bpy.context.scene.get("printable_project_id")
        if isinstance(project, str) and 1 <= len(project) <= 64 and all(
            character in "abcdefghijklmnopqrstuvwxyz0123456789_-" for character in project
        ):
            return project
        return None

    def _open_project(self, params: dict[str, Any]) -> dict[str, Any]:
        _only_keys(params, {"project_id", "mode", "checkpoint", "save_current_to", "discard_current", "timeout_seconds"})
        project = params.get("project_id")
        if not isinstance(project, str) or not 1 <= len(project) <= 64 or any(
            character not in "abcdefghijklmnopqrstuvwxyz0123456789_-" for character in project
        ):
            raise HandlerError("invalid project_id")
        mode = params.get("mode")
        if mode not in {"empty", "checkpoint", "adopt"}:
            raise HandlerError("unsupported project scene mode")
        checkpoint = params.get("checkpoint")
        backup = params.get("save_current_to")
        discard = params.get("discard_current", False)
        if type(discard) is not bool or ((mode == "checkpoint") != (checkpoint is not None)):
            raise HandlerError("checkpoint mode requires a checkpoint; other modes do not accept one")
        current = self._scene_state.snapshot.get("project_id")
        if mode == "adopt" and current not in {None, project}:
            raise HandlerError("cannot adopt another project's live scene; checkpoint and switch explicitly")
        if mode != "adopt" and backup is None and not discard:
            raise HandlerError("switching requires a backup or explicit discard_current")
        request = self._workspace.validate(checkpoint, ".blend") if checkpoint is not None else None
        if request is not None and not request.relative.startswith(f"projects/{project}/"):
            raise HandlerError("checkpoint belongs to a different project")
        if backup is not None:
            destination = self._workspace.validate(backup, ".blend")
            if current is not None and not destination.relative.startswith(f"projects/{current}/"):
                raise HandlerError("backup belongs to a different project")
            if request is not None and destination.relative == request.relative:
                raise HandlerError("backup must not replace the checkpoint being restored")
        with ExitStack() as staging:
            source = staging.enter_context(self._workspace.stage_input(request)) if request is not None else None
            if backup is not None:
                self._save_blend({"path": backup})
            if mode != "adopt":
                if source is not None:
                    result = self._bpy.ops.wm.open_mainfile(filepath=str(source), load_ui=False, use_scripts=False)
                    self._require_finished(result, "project checkpoint restore")
                    loaded = self._loaded_project()
                    if loaded not in {None, project}:
                        self._scene_state.bind_project(loaded)
                        raise HandlerError("restored checkpoint identifies a different project; inspect before recovery")
                else:
                    result = self._bpy.ops.wm.read_factory_settings(use_empty=True)
                    self._require_finished(result, "empty project initialization")
            self._bpy.context.scene["printable_project_id"] = project
            self._scene_state.bind_project(project)
        return {"project_id": project, "mode": mode, "checkpoint": checkpoint,
                "saved_previous": backup, "objects": len(self._bpy.context.scene.objects)}

    def _attach_cad(self, params: dict[str, Any]) -> dict[str, Any]:
        from mathutils import Matrix
        from .cad_import import prepare_glb
        _only_keys(params, {"project_id", "path", "timeout_seconds"})
        project = params.get("project_id")
        if project is None or self._scene_state.snapshot.get("project_id") != project:
            raise HandlerError("CAD attachment requires the currently bound project")
        request = self._workspace.validate(params.get("path"), ".glb")
        if not request.relative.startswith(f"projects/{project}/"):
            raise HandlerError("CAD artifact belongs to a different project")
        before = set(self._bpy.data.objects)
        inventory = self._workspace.validate(str(Path(request.relative).with_name("components.json")), ".json")
        with self._workspace.stage_input(request) as source, self._workspace.stage_input(inventory) as names:
            with prepare_glb(source, names) as (prepared, evidence):
                result = self._bpy.ops.import_scene.gltf(filepath=str(prepared))
                self._require_finished(result, "CAD assembly import")
        imported = [obj for obj in self._bpy.data.objects if obj not in before]
        imported_set = set(imported)
        scale = 1000.0 * self._bpy.context.scene.unit_settings.scale_length
        for obj in imported:
            if obj.parent not in imported_set:
                obj.matrix_world = Matrix.Scale(scale, 4) @ obj.matrix_world
            obj["printable_cad_source"] = request.relative
            obj["printable_project_id"] = project
        self._bpy.context.view_layer.update()
        roots = [obj.name for obj in imported if obj.parent not in imported_set]
        return {"project_id": project, "source": request.relative, "object_count": len(imported),
                "roots": roots[:100], "root_count": len(roots), "units": "mm",
                "cad": evidence}

    def _save_blend(self, params: dict[str, Any]) -> dict[str, Any]:
        return self._save_blend_scoped(params, reserved=False)

    def _job_save_checkpoint(self, params: dict[str, Any]) -> dict[str, Any]:
        return self._save_blend_scoped(params, reserved=True)

    def _save_blend_scoped(
        self, params: dict[str, Any], *, reserved: bool
    ) -> dict[str, Any]:
        _only_keys(params, {"path"})
        validator = (
            self._workspace.validate_reserved if reserved else self._workspace.validate
        )
        request = validator(params.get("path"), ".blend")
        with self._workspace.stage_output(request) as output:
            result = self._bpy.ops.wm.save_as_mainfile(
                filepath=str(output.path),
                check_existing=False,
                copy=True,
            )
            self._require_finished(result, "blend save")
            output.commit()
        return {"path": request.relative}

    def _restore_checkpoint(self, params: dict[str, Any]) -> dict[str, Any]:
        return self._restore_checkpoint_scoped(params, reserved=False)

    def _job_restore_checkpoint(self, params: dict[str, Any]) -> dict[str, Any]:
        restored = self._restore_checkpoint_scoped(params, reserved=True)
        restored["frame_current"] = int(self._bpy.context.scene.frame_current)
        return restored

    def _restore_checkpoint_scoped(
        self, params: dict[str, Any], *, reserved: bool
    ) -> dict[str, Any]:
        _only_keys(params, {"path", "expected_sha256"} if reserved else {"path"})
        expected = params.get("expected_sha256")
        if expected is not None and (not isinstance(expected, str) or len(expected) != 64
                                     or any(character not in "0123456789abcdef" for character in expected)):
            raise HandlerError("expected_sha256 must be a lowercase SHA-256 digest")
        validator = (
            self._workspace.validate_reserved if reserved else self._workspace.validate
        )
        request = validator(params.get("path"), ".blend")
        with self._workspace.stage_input(request) as source:
            if expected is not None:
                with source.open("rb") as checkpoint:
                    actual = hashlib.file_digest(checkpoint, "sha256").hexdigest()
                if actual != expected:
                    raise HandlerError("checkpoint bytes do not match the submitted source digest")
            result = self._bpy.ops.wm.open_mainfile(
                filepath=str(source),
                load_ui=False,
                use_scripts=False,
            )
            self._require_finished(result, "checkpoint restore")
        return self._get_scene_info({})

    def _capture_native_view(self, params: dict[str, Any]) -> dict[str, Any]:
        _only_keys(params, {"path", "expected_scene", "context", "method", "view", "max_size", "timeout_seconds"})
        timeout = params.get("timeout_seconds", DEFAULT_CAPTURE_TIMEOUT_SECONDS)
        if isinstance(timeout, bool) or not isinstance(timeout, (int, float)) or not math.isfinite(timeout) or not 0.1 <= timeout <= 120:
            raise HandlerError("native capture timeout must be between 0.1 and 120 seconds")
        request = self._workspace.validate(params.get("path"), ".png")
        with self._workspace.stage_output(request) as output:
            try:
                self._execution_watchdog.arm(time.monotonic() + timeout)
            except WatchdogError as error:
                raise HandlerError("native capture watchdog is unavailable") from error
            try:
                import gpu
                try:
                    observed = capture_native_view(self._bpy, gpu, output.path, params)
                except (NativeViewError, EditorContextError) as error:
                    raise HandlerError(str(error)) from error
                size = output.path.stat().st_size
                if not 0 < size <= 64 * 1024 * 1024:
                    raise HandlerError("native capture exceeds its output byte budget")
                with output.path.open("rb") as image_file:
                    digest = hashlib.file_digest(image_file, "sha256").hexdigest()
                output.commit()
                return {**observed, "path": request.relative, "media_type": "image/png",
                        "size_bytes": size, "sha256": digest}
            finally:
                try:
                    self._execution_watchdog.disarm()
                except WatchdogError as error:
                    raise HandlerError("native capture watchdog is unavailable") from error

    def _render_still(self, params: dict[str, Any]) -> dict[str, Any]:
        return self._render_still_scoped(params, reserved=False)

    def _job_render_still(self, params: dict[str, Any]) -> dict[str, Any]:
        return self._render_still_scoped(params, reserved=True)

    def _render_still_scoped(
        self, params: dict[str, Any], *, reserved: bool
    ) -> dict[str, Any]:
        _only_keys(
            params,
            {
                "path",
                "width",
                "height",
                "engine",
                "samples",
                "timeout_seconds",
                "max_output_bytes",
            },
        )
        validator = (
            self._workspace.validate_reserved if reserved else self._workspace.validate
        )
        request = validator(params.get("path"), ".png")
        width, height, timeout_seconds, engine, samples = _render_parameters(params)
        max_output_bytes = _optional_output_budget(params)
        scene = self._bpy.context.scene
        with self._workspace.stage_output(request) as output:
            deadline = time.monotonic() + timeout_seconds
            try:
                self._execution_watchdog.arm(deadline)
            except WatchdogError as error:
                raise HandlerError("render watchdog is unavailable") from error
            try:
                self._apply_render_settings(scene, width, height, engine, samples)
                scene.render.filepath = str(output.path)
                self._ensure_camera_and_light()
                result = self._bpy.ops.render.render(write_still=True)
                self._require_finished(result, "still render")
                size_bytes = output.path.stat().st_size
                if max_output_bytes is not None and size_bytes > max_output_bytes:
                    raise HandlerError(
                        "rendered image exceeds the caller-selected output byte budget"
                    )
                output.commit()
            finally:
                try:
                    self._execution_watchdog.disarm()
                except WatchdogError as error:
                    raise HandlerError("render watchdog is unavailable") from error
        return {
            "path": request.relative,
            "size_bytes": size_bytes,
            "media_type": "image/png",
            "width": scene.render.resolution_x,
            "height": scene.render.resolution_y,
            "engine": scene.render.engine,
            "render_device": (
                self._config.render_device if engine == "CYCLES" else "GRAPHICS"
            ),
            "graphics_backend": self._graphics_backend(),
            "samples": samples,
        }

    def _job_render_frame(self, params: dict[str, Any]) -> dict[str, Any]:
        _only_keys(
            params,
            {
                "path",
                "frame",
                "width",
                "height",
                "engine",
                "samples",
                "timeout_seconds",
                "max_output_bytes",
            },
        )
        frame = params.get("frame")
        if (
            isinstance(frame, bool)
            or not isinstance(frame, int)
            or not -1_048_574 <= frame <= 1_048_574
        ):
            raise HandlerError(
                "frame must be an integer between -1048574 and 1048574"
            )
        request = self._workspace.validate_reserved(params.get("path"), ".png")
        width, height, timeout_seconds, engine, samples = _render_parameters(params)
        max_output_bytes = _optional_output_budget(params)
        scene = self._bpy.context.scene
        with self._workspace.stage_output(request) as output:
            deadline = time.monotonic() + timeout_seconds
            try:
                self._execution_watchdog.arm(deadline)
            except WatchdogError as error:
                raise HandlerError("render watchdog is unavailable") from error
            try:
                scene.frame_set(frame)
                self._apply_render_settings(scene, width, height, engine, samples)
                scene.render.filepath = str(output.path)
                self._ensure_camera_and_light()
                result = self._bpy.ops.render.render(write_still=True)
                self._require_finished(result, "animation frame render")
                size_bytes = output.path.stat().st_size
                if max_output_bytes is not None and size_bytes > max_output_bytes:
                    raise HandlerError(
                        "rendered image exceeds the caller-selected output byte budget"
                    )
                output.commit()
            finally:
                try:
                    self._execution_watchdog.disarm()
                except WatchdogError as error:
                    raise HandlerError("render watchdog is unavailable") from error
        return {
            "path": request.relative,
            "size_bytes": size_bytes,
            "media_type": "image/png",
            "frame": frame,
            "width": scene.render.resolution_x,
            "height": scene.render.resolution_y,
            "engine": scene.render.engine,
            "render_device": (
                self._config.render_device if engine == "CYCLES" else "GRAPHICS"
            ),
            "graphics_backend": self._graphics_backend(),
            "samples": samples,
        }

    def _job_measure_sequence_bounds(
        self, params: dict[str, Any]
    ) -> dict[str, Any]:
        _only_keys(
            params,
            {
                "frame_start",
                "frame_end",
                "frame_step",
                "timeout_seconds",
            },
        )
        frame_start = params.get("frame_start")
        frame_end = params.get("frame_end")
        frame_step = params.get("frame_step")
        if (
            isinstance(frame_start, bool)
            or not isinstance(frame_start, int)
            or isinstance(frame_end, bool)
            or not isinstance(frame_end, int)
            or not -1_048_574 <= frame_start <= frame_end <= 1_048_574
        ):
            raise HandlerError(
                "sequence frame range must be ordered within Blender's supported interval"
            )
        if (
            isinstance(frame_step, bool)
            or not isinstance(frame_step, int)
            or frame_step <= 0
        ):
            raise HandlerError("frame_step must be a positive integer")
        timeout_seconds = _positive_float(
            params, "timeout_seconds", DEFAULT_RENDER_TIMEOUT_SECONDS
        )
        deadline = time.monotonic() + timeout_seconds
        try:
            self._execution_watchdog.arm(deadline)
        except WatchdogError as error:
            raise HandlerError("render watchdog is unavailable") from error
        scene = self._bpy.context.scene
        original_frame = scene.frame_current
        minimum = [math.inf, math.inf, math.inf]
        maximum = [-math.inf, -math.inf, -math.inf]
        frames_evaluated = 0
        try:
            with self._suspend_product_app_handlers():
                try:
                    for frame in range(frame_start, frame_end + 1, frame_step):
                        scene.frame_set(frame)
                        corners, _center, _diagonal = self._render_bounds()
                        for corner in corners:
                            for axis in range(3):
                                minimum[axis] = min(
                                    minimum[axis], float(corner[axis])
                                )
                                maximum[axis] = max(
                                    maximum[axis], float(corner[axis])
                                )
                        frames_evaluated += 1
                finally:
                    scene.frame_set(original_frame)
        finally:
            try:
                self._execution_watchdog.disarm()
            except WatchdogError as error:
                raise HandlerError("render watchdog is unavailable") from error
        from mathutils import Vector  # type: ignore[import-not-found]

        low = Vector(tuple(minimum))
        high = Vector(tuple(maximum))
        center = (low + high) * 0.5
        diagonal = max(float((high - low).length), 0.1)
        corners = [
            Vector((x, y, z))
            for x in (low.x, high.x)
            for y in (low.y, high.y)
            for z in (low.z, high.z)
        ]
        return {
            "bounds": self._bounds_metadata(
                corners, center, diagonal
            ),
            "frames_evaluated": frames_evaluated,
            "frame_start": frame_start,
            "frame_end": frame_end,
            "frame_step": frame_step,
        }

    def _render_product(self, params: dict[str, Any]) -> dict[str, Any]:
        return self._render_product_scoped(params, reserved=False)

    def _job_render_product(self, params: dict[str, Any]) -> dict[str, Any]:
        return self._render_product_scoped(params, reserved=True)

    def _render_product_scoped(
        self, params: dict[str, Any], *, reserved: bool
    ) -> dict[str, Any]:
        _only_keys(
            params,
            {
                "path",
                "objects",
                "presentation",
                "width",
                "height",
                "engine",
                "samples",
                "timeout_seconds",
                "max_output_bytes",
                "frame",
                "allow_ground",
                "framing_bounds",
                "camera_behavior",
            },
        )
        validator = (
            self._workspace.validate_reserved if reserved else self._workspace.validate
        )
        request = validator(params.get("path"), ".png")
        job_frame = None
        if reserved:
            job_frame = params.get("frame")
            if job_frame is not None:
                if (
                    isinstance(job_frame, bool)
                    or not isinstance(job_frame, int)
                    or not -1_048_574 <= job_frame <= 1_048_574
                ):
                    raise HandlerError(
                        "frame must be an integer between -1048574 and 1048574"
                    )
            object_names: list[str] | None = (
                _required_object_names(params)
                if "objects" in params
                else None
            )
        else:
            if any(
                key in params
                for key in (
                    "frame",
                    "allow_ground",
                    "framing_bounds",
                    "camera_behavior",
                )
            ):
                raise HandlerError("unknown parameter: job presentation option")
            object_names = _required_object_names(params)
        presentation = (
            self._validate_product_presentation(
                params.get("presentation"), object_names
            )
            if object_names is not None
            else None
        )
        camera_behavior = params.get("camera_behavior", "profile")
        if camera_behavior not in {"profile", "preserve", "bounds"}:
            raise HandlerError(
                "camera_behavior must be profile, preserve, or bounds"
            )
        if not reserved and camera_behavior != "profile":
            raise HandlerError("unknown parameter: camera_behavior")
        allow_ground = params.get("allow_ground", True)
        if not isinstance(allow_ground, bool):
            raise HandlerError("allow_ground must be a boolean")
        if not reserved and not allow_ground:
            raise HandlerError("unknown parameter: allow_ground")
        if camera_behavior == "bounds" and "framing_bounds" not in params:
            raise HandlerError(
                "framing_bounds is required when camera_behavior is bounds"
            )
        if camera_behavior == "profile" and "framing_bounds" in params:
            raise HandlerError(
                "framing_bounds is only valid for preserve or bounds camera behavior"
            )
        if presentation is not None:
            presentation["camera_behavior"] = camera_behavior
            presentation["allow_ground"] = allow_ground
        width, height, timeout_seconds, engine, samples = _render_parameters(
            params,
            default_width=1024,
            default_height=768,
        )
        if width * height > MAX_PRODUCT_RENDER_PIXELS:
            raise HandlerError(
                f"product render exceeds the {MAX_PRODUCT_RENDER_PIXELS}-pixel output limit; reduce width or height"
            )
        max_output_bytes = _optional_output_budget(params)
        if max_output_bytes is None:
            max_output_bytes = MAX_PRODUCT_RENDER_BYTES
        elif max_output_bytes > MAX_PRODUCT_RENDER_BYTES:
            raise HandlerError(
                f"max_output_bytes must be no greater than {MAX_PRODUCT_RENDER_BYTES} for product renders"
            )

        deadline = time.monotonic() + timeout_seconds
        try:
            self._execution_watchdog.arm(deadline)
        except WatchdogError as error:
            raise HandlerError("render watchdog is unavailable") from error
        watchdog_armed = True

        def finish_watchdog() -> None:
            nonlocal watchdog_armed
            if not watchdog_armed:
                return
            try:
                self._execution_watchdog.disarm()
            except WatchdogError as error:
                raise HandlerError("render watchdog is unavailable") from error
            finally:
                watchdog_armed = False

        try:
            with self._workspace.stage_output(request) as output:
                with self._suspend_product_app_handlers():
                    if job_frame is not None:
                        self._bpy.context.scene.frame_set(job_frame)
                    if object_names is None:
                        object_names = self._renderable_product_names()
                        presentation = self._validate_product_presentation(
                            params.get("presentation"), object_names
                        )
                        presentation["camera_behavior"] = camera_behavior
                        presentation["allow_ground"] = allow_ground
                    if presentation is None:
                        raise HandlerError(
                            "product presentation could not be prepared"
                        )
                    selected = set(object_names)
                    geometry_usage = self._product_geometry_preflight(selected)
                    corners, center, diagonal = self._render_bounds(selected)
                    bounds = self._bounds_metadata(corners, center, diagonal)
                    if "framing_bounds" in params:
                        corners, center, diagonal, bounds = (
                            self._product_framing_bounds(
                                params["framing_bounds"],
                                bounds,
                                require_contains_geometry=camera_behavior
                                == "bounds",
                            )
                        )
                    source_signature = self._product_source_signature()
                    datablock_counts = self._product_datablock_counts()
                    rendered = self._render_product_staged(
                        output.path,
                        object_names,
                        presentation,
                        corners,
                        center,
                        diagonal,
                        bounds,
                        width,
                        height,
                        engine,
                        samples,
                        max_output_bytes,
                        geometry_usage,
                        source_signature,
                        datablock_counts,
                    )
                if self._product_source_signature() != source_signature:
                    raise HandlerError(
                        "source scene changed during product presentation"
                    )
                if self._product_datablock_counts() != datablock_counts:
                    raise HandlerError(
                        "product presentation cleanup was incomplete"
                    )
                finish_watchdog()
                output.commit()
            result = {
                "path": request.relative,
                "size_bytes": rendered["size_bytes"],
                "sha256": rendered["sha256"],
                "media_type": "image/png",
                "width": width,
                "height": height,
                "engine": rendered["engine"],
                "render_device": (
                    self._config.render_device if engine == "CYCLES" else "GRAPHICS"
                ),
                "graphics_backend": self._graphics_backend(),
                "samples": samples,
                "objects": sorted(object_names),
                "bounds": bounds,
                "presentation": rendered["presentation"],
                "source_state_verified": True,
                "cleanup_verified": True,
            }
            if job_frame is not None:
                result["frame"] = job_frame
            return result
        finally:
            finish_watchdog()

    @staticmethod
    def _validate_product_presentation(
        raw_presentation: Any, selected_objects: list[str]
    ) -> dict[str, Any]:
        if not isinstance(raw_presentation, dict):
            raise HandlerError("presentation must be an object")
        _only_keys(
            raw_presentation,
            {"profile", "view", "materials", "surface_shading",
             "exposure_stops", "light_intensity_scale"},
        )
        profile_name = raw_presentation.get("profile")
        if (
            not isinstance(profile_name, str)
            or profile_name not in PRODUCT_PRESENTATION_PROFILES
        ):
            raise HandlerError(
                "presentation profile must be engineering, studio_neutral, or studio_dark"
            )
        raw_view = raw_presentation.get("view", {})
        if not isinstance(raw_view, dict):
            raise HandlerError("presentation view must be an object")
        _only_keys(raw_view, {"azimuth_degrees", "elevation_degrees"})
        azimuth = _bounded_float(
            raw_view, "azimuth_degrees", 45.0, -360.0, 360.0
        )
        elevation = _bounded_float(
            raw_view, "elevation_degrees", 25.0, -89.0, 89.0
        )
        if (
            PRODUCT_PRESENTATION_PROFILES[profile_name]["ground"] is not None
            and elevation < 0.0
        ):
            raise HandlerError(
                "studio presentation elevation_degrees must be from 0 through 89 so the ground cannot occlude the product"
            )
        surface_shading = raw_presentation.get(
            "surface_shading",
            PRODUCT_PRESENTATION_PROFILES[profile_name]["default_shading"],
        )
        if surface_shading not in {"preserve", "smooth_by_angle"}:
            raise HandlerError(
                "surface_shading must be preserve or smooth_by_angle"
            )
        raw_materials = raw_presentation.get("materials", [])
        if (
            not isinstance(raw_materials, list)
            or len(raw_materials) > MAX_PRODUCT_MATERIAL_OVERRIDES
        ):
            raise HandlerError(
                f"presentation materials must contain at most {MAX_PRODUCT_MATERIAL_OVERRIDES} entries"
            )
        selected = set(selected_objects)
        assigned: set[str] = set()
        materials: list[dict[str, Any]] = []
        for index, raw_material in enumerate(raw_materials):
            if not isinstance(raw_material, dict):
                raise HandlerError("each presentation material must be an object")
            _only_keys(
                raw_material,
                {"objects", "base_color_srgb", "metallic", "roughness"},
            )
            objects = _required_named_object_names(raw_material, "objects")
            for name in objects:
                if name not in selected:
                    raise HandlerError(
                        f"presentation material object is not selected: {name}"
                    )
                if name in assigned:
                    raise HandlerError(
                        f"presentation material object is assigned more than once: {name}"
                    )
                assigned.add(name)
            color = _finite_vector(raw_material, "base_color_srgb")
            if any(component < 0.0 or component > 1.0 for component in color):
                raise HandlerError(
                    "base_color_srgb must contain three finite numbers from 0 through 1"
                )
            metallic = _bounded_float(
                raw_material, "metallic", 0.0, 0.0, 1.0
            )
            roughness = _bounded_float(
                raw_material, "roughness", 0.5, 0.0, 1.0
            )
            materials.append(
                {
                    "index": index,
                    "objects": objects,
                    "base_color_srgb": color,
                    "metallic": metallic,
                    "roughness": roughness,
                }
            )
        return {
            "profile": profile_name,
            "exposure_stops": _bounded_float(
                raw_presentation, "exposure_stops", 0.0, -10.0, 10.0
            ),
            "light_intensity_scale": _bounded_float(
                raw_presentation, "light_intensity_scale", 1.0, 0.0, 10.0
            ),
            "view": {
                "azimuth_degrees": azimuth,
                "elevation_degrees": elevation,
            },
            "surface_shading": surface_shading,
            "materials": materials,
        }

    def _render_product_staged(
        self,
        output_path: Path,
        object_names: list[str],
        presentation: dict[str, Any],
        corners: list[Any],
        center: Any,
        diagonal: float,
        bounds: dict[str, Any],
        width: int,
        height: int,
        engine: str,
        samples: int | None,
        max_output_bytes: int | None,
        geometry_usage: dict[str, int],
        source_signature: tuple[Any, ...],
        datablock_counts: tuple[int, ...],
    ) -> dict[str, Any]:
        assets: dict[str, Any] = {
            "scene": None,
            "objects": [],
            "meshes": [],
            "materials": [],
            "camera_data": None,
            "light_data": [],
            "world": None,
        }
        operation_error: Exception | None = None
        rendered: dict[str, Any] | None = None
        try:
            rendered = self._build_and_render_product_presentation(
                output_path,
                object_names,
                presentation,
                corners,
                center,
                diagonal,
                bounds,
                width,
                height,
                engine,
                samples,
                max_output_bytes,
                geometry_usage,
                assets,
            )
        except Exception as error:
            operation_error = error

        cleanup_error: Exception | None = None
        try:
            self._cleanup_product_presentation(assets)
        except Exception as error:
            cleanup_error = error

        verification_error: Exception | None = None
        try:
            if self._product_source_signature() != source_signature:
                raise HandlerError("source scene changed during product presentation")
            if self._product_datablock_counts() != datablock_counts:
                raise HandlerError("product presentation cleanup was incomplete")
        except Exception as error:
            verification_error = error

        if cleanup_error is not None:
            raise cleanup_error from operation_error
        if verification_error is not None:
            raise verification_error from operation_error
        if operation_error is not None:
            raise operation_error
        if rendered is None:
            raise HandlerError("product presentation did not produce a result")
        return rendered

    def _build_and_render_product_presentation(
        self,
        output_path: Path,
        object_names: list[str],
        presentation: dict[str, Any],
        corners: list[Any],
        center: Any,
        diagonal: float,
        bounds: dict[str, Any],
        width: int,
        height: int,
        engine: str,
        samples: int | None,
        max_output_bytes: int | None,
        geometry_usage: dict[str, int],
        assets: dict[str, Any],
    ) -> dict[str, Any]:
        from mathutils import Vector  # type: ignore[import-not-found]

        source_scene = self._bpy.context.scene
        source_frame = int(source_scene.frame_current)
        source_subframe = float(source_scene.frame_subframe)
        profile = PRODUCT_PRESENTATION_PROFILES[presentation["profile"]]
        scene = self._bpy.data.scenes.new("PrintableProductPresentation")
        assets["scene"] = scene
        self._apply_render_settings(scene, width, height, engine, samples)
        scene.render.image_settings.color_mode = "RGB"
        scene.render.image_settings.color_depth = "8"
        scene.render.film_transparent = False
        scene.render.filepath = str(output_path)
        scene.display_settings.display_device = "sRGB"
        scene.view_settings.view_transform = profile["view_transform"]
        scene.view_settings.look = "None"
        scene.view_settings.exposure = presentation["exposure_stops"]
        scene.view_settings.gamma = 1.0

        world = self._bpy.data.worlds.new("PrintableProductWorld")
        assets["world"] = world
        world.use_nodes = True
        background = world.node_tree.nodes.get("Background")
        if background is None:
            raise HandlerError("Blender world has no Background node")
        background.inputs["Color"].default_value = (
            *self._srgb_to_linear(profile["world_color_srgb"]),
            1.0,
        )
        world_strength = profile["world_strength"] * presentation["light_intensity_scale"]
        background.inputs["Strength"].default_value = world_strength
        scene.world = world

        override_by_object: dict[str, Any] = {}
        override_metadata: list[dict[str, Any]] = []
        for material_spec in presentation["materials"]:
            material = self._create_product_material(
                f"PrintableProductOverride{material_spec['index'] + 1}",
                material_spec["base_color_srgb"],
                material_spec["metallic"],
                material_spec["roughness"],
            )
            assets["materials"].append(material)
            for name in material_spec["objects"]:
                override_by_object[name] = material
            override_metadata.append(
                {
                    "objects": sorted(material_spec["objects"]),
                    "base_color_srgb": list(material_spec["base_color_srgb"]),
                    "metallic": material_spec["metallic"],
                    "roughness": material_spec["roughness"],
                }
            )

        fallback_spec = {
            "base_color_srgb": (0.42, 0.45, 0.5),
            "metallic": 0.0,
            "roughness": 0.5,
        }
        fallback_material = None
        fallback_objects: set[str] = set()
        preserved_objects: set[str] = set()
        instance_count = 0
        selected = set(object_names)
        depsgraph = self._bpy.context.evaluated_depsgraph_get()
        for instance in depsgraph.object_instances:
            if not self._is_renderable_geometry(instance):
                continue
            source_name = self._instance_source_name(instance)
            if source_name not in selected:
                continue
            mesh = self._bpy.data.meshes.new_from_object(
                instance.object,
                preserve_all_data_layers=True,
                depsgraph=depsgraph,
            )
            if mesh is None:
                raise HandlerError(f"renderable object has no evaluated mesh: {source_name}")
            assets["meshes"].append(mesh)
            product_object = self._bpy.data.objects.new(
                f"PrintableProduct_{source_name}", mesh
            )
            assets["objects"].append(product_object)
            product_object.matrix_world = instance.matrix_world.copy()
            scene.collection.objects.link(product_object)
            if presentation["surface_shading"] == "smooth_by_angle":
                self._smooth_product_mesh_by_angle(mesh, 30.0)
            override = override_by_object.get(source_name)
            if override is not None:
                mesh.materials.clear()
                mesh.materials.append(override)
                for polygon in mesh.polygons:
                    polygon.material_index = 0
            else:
                effective_materials = self._effective_product_materials(
                    instance.object, mesh
                )
                needs_fallback = not effective_materials or any(
                    material is None for material in effective_materials
                )
                if needs_fallback and fallback_material is None:
                    fallback_material = self._create_product_material(
                        "PrintableProductFallback",
                        fallback_spec["base_color_srgb"],
                        fallback_spec["metallic"],
                        fallback_spec["roughness"],
                    )
                    assets["materials"].append(fallback_material)
                preserved, used_fallback = self._assign_product_materials(
                    mesh, effective_materials, fallback_material
                )
                if preserved:
                    preserved_objects.add(source_name)
                if used_fallback:
                    fallback_objects.add(source_name)
            instance_count += 1
        if instance_count == 0:
            raise HandlerError("selected objects produced no evaluated presentation geometry")

        ground_metadata = None
        if profile["ground"] is not None and presentation.get(
            "allow_ground", True
        ):
            ground_material = self._create_product_material(
                "PrintableProductGround",
                profile["ground"]["base_color_srgb"],
                profile["ground"]["metallic"],
                profile["ground"]["roughness"],
            )
            assets["materials"].append(ground_material)
            ground_mesh = self._bpy.data.meshes.new("PrintableProductGroundMesh")
            assets["meshes"].append(ground_mesh)
            ground_size = max(diagonal * 8.0, 1.0)
            ground_z = bounds["minimum"][2] - max(diagonal * 0.002, 0.01)
            half = ground_size / 2.0
            ground_mesh.from_pydata(
                [
                    (-half, -half, 0.0),
                    (half, -half, 0.0),
                    (half, half, 0.0),
                    (-half, half, 0.0),
                ],
                [],
                [(0, 1, 2, 3)],
            )
            ground_mesh.materials.append(ground_material)
            ground_mesh.update()
            ground = self._bpy.data.objects.new(
                "PrintableProductGround", ground_mesh
            )
            assets["objects"].append(ground)
            ground.location = (bounds["center"][0], bounds["center"][1], ground_z)
            scene.collection.objects.link(ground)
            ground_metadata = {
                "enabled": True,
                "style": "seamless",
                "z": ground_z,
                "size": ground_size,
                "base_color_srgb": list(profile["ground"]["base_color_srgb"]),
                "metallic": profile["ground"]["metallic"],
                "roughness": profile["ground"]["roughness"],
            }

        camera_behavior = presentation.get("camera_behavior", "profile")
        if camera_behavior == "preserve":
            source_camera = self._bpy.context.scene.camera
            if source_camera is None or source_camera.type != "CAMERA":
                raise HandlerError(
                    "preserve camera behavior requires an authored scene camera"
                )
            evaluated_camera = source_camera.evaluated_get(depsgraph)
            camera_data = evaluated_camera.data.copy()
            assets["camera_data"] = camera_data
            camera = self._bpy.data.objects.new(
                "PrintableProductCamera", camera_data
            )
            assets["objects"].append(camera)
            camera.matrix_world = evaluated_camera.matrix_world.copy()
            camera_position = [
                float(value) for value in camera.matrix_world.translation
            ]
            if not all(math.isfinite(value) for value in camera_position):
                raise HandlerError("authored camera position must be finite")
            if (
                ground_metadata is not None
                and camera_position[2] <= ground_metadata["z"]
            ):
                raise HandlerError(
                    "preserved authored camera must remain above the studio ground plane"
                )
            scene.collection.objects.link(camera)
            scene.camera = camera
            camera_metadata = {
                "type": (
                    "orthographic"
                    if camera_data.type == "ORTHO"
                    else "perspective"
                ),
                "behavior": "preserve",
                "source_object": str(
                    getattr(evaluated_camera, "original", evaluated_camera).name
                ),
                "position": camera_position,
                "lens_mm": (
                    None if camera_data.type == "ORTHO" else camera_data.lens
                ),
                "ortho_scale": (
                    camera_data.ortho_scale
                    if camera_data.type == "ORTHO"
                    else None
                ),
                "clip_start": camera_data.clip_start,
                "clip_end": camera_data.clip_end,
            }
        else:
            camera_data = self._bpy.data.cameras.new(
                "PrintableProductCamera"
            )
            assets["camera_data"] = camera_data
            camera_data.type = profile["camera_type"]
            if profile["lens_mm"] is not None:
                camera_data.lens = profile["lens_mm"]
                camera_data.sensor_fit = "HORIZONTAL"
                camera_data.sensor_width = 36.0
            camera = self._bpy.data.objects.new(
                "PrintableProductCamera", camera_data
            )
            assets["objects"].append(camera)
            scene.collection.objects.link(camera)
            scene.camera = camera
            camera_metadata = self._frame_product_camera(
                camera,
                corners,
                center,
                diagonal,
                presentation["view"],
                width,
                height,
            )
            camera_metadata["behavior"] = camera_behavior
            camera_metadata["target"] = bounds["center"]

        light_metadata: list[dict[str, Any]] = []
        scale = max(diagonal, 1.0)
        for role, offset, base_energy, size_factor in profile["lights"]:
            light_data = self._bpy.data.lights.new(
                name=f"PrintableProduct{role.title()}", type="AREA"
            )
            assets["light_data"].append(light_data)
            energy = base_energy * scale * scale * presentation["light_intensity_scale"]
            if not math.isfinite(energy):
                raise HandlerError("scene geometry bounds are too large to light")
            light_data.energy = energy
            light_data.shape = "DISK"
            light_data.size = size_factor * scale
            light = self._bpy.data.objects.new(
                f"PrintableProduct{role.title()}", light_data
            )
            assets["objects"].append(light)
            scene.collection.objects.link(light)
            light.location = center + Vector(offset) * scale
            light.rotation_euler = (center - light.location).to_track_quat(
                "-Z", "Y"
            ).to_euler()
            light_metadata.append(
                {
                    "role": role,
                    "type": "AREA",
                    "position": [float(value) for value in light.location],
                    "energy_watts": energy,
                    "shape": "DISK",
                    "size": light_data.size,
                }
            )

        scene.frame_set(source_frame, subframe=source_subframe)
        result = self._bpy.ops.render.render(
            write_still=True, scene=scene.name
        )
        self._require_finished(result, "product render")
        size_bytes = output_path.stat().st_size
        if max_output_bytes is not None and size_bytes > max_output_bytes:
            raise HandlerError(
                "rendered image exceeds the caller-selected output byte budget"
            )
        with output_path.open("rb") as image_file:
            sha256 = hashlib.file_digest(image_file, "sha256").hexdigest()
        return {
            "size_bytes": size_bytes,
            "sha256": sha256,
            "engine": scene.render.engine,
            "presentation": {
                "profile": presentation["profile"],
                "camera": camera_metadata,
                "lighting": light_metadata,
                "light_intensity_scale": presentation["light_intensity_scale"],
                "color_management": {
                    "display_device": scene.display_settings.display_device,
                    "view_transform": scene.view_settings.view_transform,
                    "look": scene.view_settings.look,
                    "exposure": scene.view_settings.exposure,
                    "gamma": scene.view_settings.gamma,
                },
                "world": {
                    "base_color_srgb": list(profile["world_color_srgb"]),
                    "strength": world_strength,
                },
                "ground": ground_metadata
                if ground_metadata is not None
                else {"enabled": False},
                "materials": {
                    "preserved_objects": sorted(preserved_objects),
                    "fallback": {
                        "objects": sorted(fallback_objects),
                        "base_color_srgb": list(fallback_spec["base_color_srgb"]),
                        "metallic": fallback_spec["metallic"],
                        "roughness": fallback_spec["roughness"],
                    },
                    "overrides": override_metadata,
                },
                "shading": {
                    "mode": presentation["surface_shading"],
                    "angle_degrees": (
                        30.0
                        if presentation["surface_shading"] == "smooth_by_angle"
                        else None
                    ),
                    "presentation_only": True,
                },
                "framing": {
                    "margin_percent": 15.0,
                    "bounds": bounds,
                    "instance_count": instance_count,
                },
                "geometry": geometry_usage,
            },
        }

    @staticmethod
    def _srgb_to_linear(color: tuple[float, float, float]) -> tuple[float, ...]:
        return tuple(
            component / 12.92
            if component <= 0.04045
            else ((component + 0.055) / 1.055) ** 2.4
            for component in color
        )

    def _create_product_material(
        self,
        name: str,
        base_color_srgb: tuple[float, float, float],
        metallic: float,
        roughness: float,
    ) -> Any:
        material = self._bpy.data.materials.new(name)
        try:
            material.use_nodes = True
            principled = material.node_tree.nodes.get("Principled BSDF")
            if principled is None:
                raise HandlerError("Blender material has no Principled BSDF node")
            linear = self._srgb_to_linear(base_color_srgb)
            principled.inputs["Base Color"].default_value = (*linear, 1.0)
            principled.inputs["Metallic"].default_value = metallic
            principled.inputs["Roughness"].default_value = roughness
            material.diffuse_color = (*linear, 1.0)
            return material
        except Exception:
            if material.users == 0:
                self._bpy.data.materials.remove(material)
            raise

    @staticmethod
    def _smooth_product_mesh_by_angle(mesh: Any, angle_degrees: float) -> None:
        import bmesh  # type: ignore[import-not-found]

        mesh_buffer = bmesh.new()
        try:
            mesh_buffer.from_mesh(mesh)
            mesh_buffer.normal_update()
            threshold = math.radians(angle_degrees)
            for face in mesh_buffer.faces:
                face.smooth = True
            for edge in mesh_buffer.edges:
                edge.smooth = (
                    len(edge.link_faces) == 2
                    and edge.calc_face_angle(math.pi) <= threshold
                )
            mesh_buffer.to_mesh(mesh)
            mesh.update()
        finally:
            mesh_buffer.free()

    @staticmethod
    def _effective_product_materials(obj: Any, mesh: Any) -> list[Any | None]:
        material_slots = getattr(obj, "material_slots", None)
        if material_slots is None:
            return list(mesh.materials)
        return [slot.material for slot in material_slots]

    @staticmethod
    def _assign_product_materials(
        mesh: Any, effective_materials: list[Any | None], fallback: Any | None
    ) -> tuple[bool, bool]:
        preserved = any(material is not None for material in effective_materials)
        used_fallback = not effective_materials or any(
            material is None for material in effective_materials
        )
        if used_fallback and fallback is None:
            raise HandlerError("product fallback material is unavailable")
        resolved = [
            material if material is not None else fallback
            for material in effective_materials
        ]
        if not resolved:
            resolved = [fallback]
        mesh.materials.clear()
        for material in resolved:
            mesh.materials.append(material)
        if not effective_materials:
            for polygon in mesh.polygons:
                polygon.material_index = 0
        return preserved, used_fallback

    def _frame_product_camera(
        self,
        camera: Any,
        corners: list[Any],
        center: Any,
        diagonal: float,
        view: dict[str, float],
        width: int,
        height: int,
    ) -> dict[str, Any]:
        from mathutils import Vector  # type: ignore[import-not-found]

        azimuth = math.radians(view["azimuth_degrees"])
        elevation = math.radians(view["elevation_degrees"])
        direction = Vector(
            (
                math.cos(elevation) * math.cos(azimuth),
                math.cos(elevation) * math.sin(azimuth),
                math.sin(elevation),
            )
        ).normalized()
        up_axis = "X" if abs(direction.z) > 0.999 else "Y"
        camera.location = center + direction * max(diagonal * 2.0, 1.0)
        camera_rotation = (center - camera.location).to_track_quat("-Z", up_axis)
        camera.rotation_euler = camera_rotation.to_euler()
        camera.data.clip_start = max(diagonal / 10000.0, 0.0001)
        camera.data.clip_end = max(diagonal * 20.0, 10.0)

        if camera.data.type == "ORTHO":
            inverse_rotation = camera_rotation.inverted()
            camera_corners = [
                inverse_rotation @ (corner - center) for corner in corners
            ]
            span_x = max(corner.x for corner in camera_corners) - min(
                corner.x for corner in camera_corners
            )
            span_y = max(corner.y for corner in camera_corners) - min(
                corner.y for corner in camera_corners
            )
            camera.data.ortho_scale = self._product_orthographic_scale(
                span_x, span_y, width / height
            )
            return {
                "type": "orthographic",
                "azimuth_degrees": view["azimuth_degrees"],
                "elevation_degrees": view["elevation_degrees"],
                "position": [float(value) for value in camera.location],
                "target": [float(value) for value in center],
                "ortho_scale": camera.data.ortho_scale,
                "lens_mm": None,
                "clip_start": camera.data.clip_start,
                "clip_end": camera.data.clip_end,
            }

        camera.location = center
        framing_rotation = direction.to_track_quat("Z", up_axis)
        camera.rotation_euler = framing_rotation.to_euler()
        inverse_rotation = framing_rotation.inverted()
        centered_corners = [
            inverse_rotation @ (corner - center) for corner in corners
        ]
        aspect = width / height
        tan_horizontal = camera.data.sensor_width / (2.0 * camera.data.lens)
        tan_vertical = tan_horizontal / aspect
        distance = max(
            max(
                corner.z
                + PRODUCT_PRESENTATION_MARGIN
                * max(
                    abs(corner.x) / tan_horizontal,
                    abs(corner.y) / tan_vertical,
                )
                for corner in centered_corners
            ),
            diagonal,
            1.0,
        )
        camera.location = center + direction * distance
        camera.rotation_euler = (center - camera.location).to_track_quat(
            "-Z", up_axis
        ).to_euler()
        camera.data.clip_end = max(distance + diagonal * 4.0, 10.0)
        return {
            "type": "perspective",
            "azimuth_degrees": view["azimuth_degrees"],
            "elevation_degrees": view["elevation_degrees"],
            "position": [float(value) for value in camera.location],
            "target": [float(value) for value in center],
            "ortho_scale": None,
            "lens_mm": camera.data.lens,
            "sensor_width_mm": camera.data.sensor_width,
            "clip_start": camera.data.clip_start,
            "clip_end": camera.data.clip_end,
        }

    @staticmethod
    def _product_orthographic_scale(
        span_x: float, span_y: float, aspect: float
    ) -> float:
        if aspect >= 1.0:
            fitted_span = max(span_x, span_y * aspect)
        else:
            fitted_span = max(span_y, span_x / aspect)
        return max(fitted_span, 0.1) * PRODUCT_PRESENTATION_MARGIN

    def _cleanup_product_presentation(self, assets: dict[str, Any]) -> None:
        first_error: Exception | None = None

        def attempt(operation: Callable[[], None]) -> None:
            nonlocal first_error
            try:
                operation()
            except Exception as error:
                if first_error is None:
                    first_error = error

        for obj in reversed(assets["objects"]):
            attempt(lambda obj=obj: self._bpy.data.objects.remove(obj, do_unlink=True))
        scene = assets["scene"]
        if scene is not None:
            attempt(lambda: self._bpy.data.scenes.remove(scene))
        for mesh in reversed(assets["meshes"]):
            if mesh.users == 0:
                attempt(lambda mesh=mesh: self._bpy.data.meshes.remove(mesh))
        camera_data = assets["camera_data"]
        if camera_data is not None and camera_data.users == 0:
            attempt(lambda: self._bpy.data.cameras.remove(camera_data))
        for light_data in reversed(assets["light_data"]):
            if light_data.users == 0:
                attempt(lambda light_data=light_data: self._bpy.data.lights.remove(light_data))
        for material in reversed(assets["materials"]):
            if material.users == 0:
                attempt(lambda material=material: self._bpy.data.materials.remove(material))
        world = assets["world"]
        if world is not None and world.users == 0:
            attempt(lambda: self._bpy.data.worlds.remove(world))
        if first_error is not None:
            raise first_error

    def _product_datablock_counts(self) -> tuple[int, ...]:
        data = self._bpy.data
        return (
            len(data.scenes),
            len(data.objects),
            len(data.meshes),
            len(data.materials),
            len(data.cameras),
            len(data.lights),
            len(data.worlds),
        )

    @contextmanager
    def _suspend_product_app_handlers(self) -> Iterator[None]:
        handler_namespace = getattr(
            getattr(self._bpy, "app", None), "handlers", None
        )
        if handler_namespace is None:
            raise HandlerError("Blender application handlers are unavailable")
        snapshots = [
            (value, tuple(value))
            for name in sorted(dir(handler_namespace))
            if isinstance(
                value := getattr(handler_namespace, name, None),
                list,
            )
        ]
        if not snapshots:
            raise HandlerError("Blender application handlers are unavailable")
        try:
            for handlers, _original in snapshots:
                handlers.clear()
            yield
        finally:
            first_error: Exception | None = None
            for handlers, original in snapshots:
                try:
                    handlers.clear()
                    handlers.extend(original)
                except Exception as error:
                    if first_error is None:
                        first_error = error
            if first_error is not None:
                raise HandlerError(
                    "Blender application handlers could not be restored"
                ) from first_error

    def _product_source_signature(self) -> tuple[Any, ...]:
        def pointer(value: Any) -> int | None:
            return value.as_pointer() if value is not None else None

        scene = self._bpy.context.scene
        objects = []
        for obj in sorted(self._bpy.data.objects, key=lambda item: item.name):
            matrix = tuple(
                float(component)
                for row in obj.matrix_world
                for component in row
            )
            materials = tuple(
                pointer(slot.material) for slot in getattr(obj, "material_slots", ())
            )
            objects.append(
                (
                    obj.name,
                    pointer(obj),
                    obj.type,
                    pointer(getattr(obj, "data", None)),
                    pointer(getattr(obj, "parent", None)),
                    bool(obj.hide_render),
                    bool(getattr(obj, "visible_camera", True)),
                    matrix,
                    materials,
                )
            )
        render = scene.render
        return (
            pointer(scene),
            pointer(scene.camera),
            pointer(scene.world),
            scene.frame_current,
            render.engine,
            render.resolution_x,
            render.resolution_y,
            render.resolution_percentage,
            render.image_settings.file_format,
            render.image_settings.color_mode,
            render.image_settings.color_depth,
            render.filepath,
            scene.view_settings.view_transform,
            scene.view_settings.look,
            scene.view_settings.exposure,
            scene.view_settings.gamma,
            tuple(objects),
        )

    def _render_diagnostic(self, params: dict[str, Any]) -> dict[str, Any]:
        _only_keys(
            params,
            {
                "path",
                "mode",
                "objects",
                "axis",
                "position",
                "build_direction",
                "overhang_angle_degrees",
                "view_direction",
                "width",
                "height",
                "engine",
                "samples",
                "timeout_seconds",
            },
        )
        request = self._workspace.validate(params.get("path"), ".png")
        width, height, timeout_seconds, engine, samples = _render_parameters(params)
        if width * height > MAX_REVIEW_SOURCE_PIXELS:
            raise HandlerError(
                f"diagnostic render must be at most {MAX_REVIEW_SOURCE_PIXELS} pixels"
            )
        mode = params.get("mode")
        if mode not in {"cross_section", "overhang"}:
            raise HandlerError("mode must be cross_section or overhang")
        object_names = _optional_object_names(params)
        if mode == "cross_section":
            if "build_direction" in params or "overhang_angle_degrees" in params:
                raise HandlerError(
                    "build_direction and overhang_angle_degrees are only valid for overhang"
                )
            axis = params.get("axis", "Z")
            if axis not in {"X", "Y", "Z"}:
                raise HandlerError("axis must be X, Y, or Z")
            axis_index = {"X": 0, "Y": 1, "Z": 2}[axis]
            axis_direction = tuple(1.0 if index == axis_index else 0.0 for index in range(3))
            view_direction = _normalized_vector(
                params, "view_direction", axis_direction
            )
            diagnostic_options: dict[str, Any] = {
                "axis": axis,
                "axis_index": axis_index,
                "position": (
                    _finite_float(params, "position")
                    if "position" in params
                    else None
                ),
            }
        else:
            if "axis" in params or "position" in params:
                raise HandlerError("axis and position are only valid for cross_section")
            build_direction = _normalized_vector(
                params, "build_direction", (0.0, 0.0, 1.0)
            )
            overhang_angle = _bounded_float(
                params, "overhang_angle_degrees", 45.0, 0.0, 90.0
            )
            view_direction = _normalized_vector(
                params, "view_direction", (1.0, -1.0, 1.0)
            )
            diagnostic_options = {
                "build_direction": build_direction,
                "overhang_angle_degrees": overhang_angle,
            }

        deadline = time.monotonic() + timeout_seconds
        try:
            self._execution_watchdog.arm(deadline)
        except WatchdogError as error:
            raise HandlerError("render watchdog is unavailable") from error
        watchdog_armed = True

        def finish_watchdog() -> None:
            nonlocal watchdog_armed
            if not watchdog_armed:
                return
            try:
                self._execution_watchdog.disarm()
            except WatchdogError as error:
                raise HandlerError("render watchdog is unavailable") from error
            finally:
                watchdog_armed = False

        try:
            return self._render_diagnostic_armed(
                request,
                mode,
                object_names,
                diagnostic_options,
                view_direction,
                width,
                height,
                engine,
                samples,
                finish_watchdog,
            )
        finally:
            finish_watchdog()

    def _render_diagnostic_armed(
        self,
        request: WorkspacePath,
        mode: str,
        object_names: set[str] | None,
        diagnostic_options: dict[str, Any],
        view_direction: tuple[float, float, float],
        width: int,
        height: int,
        engine: str,
        samples: int | None,
        finish_watchdog: Callable[[], None],
    ) -> dict[str, Any]:
        if object_names is not None:
            missing = self._missing_renderable_names(object_names)
            if missing:
                raise HandlerError(f"renderable object not found: {missing[0]}")
        diagnostic_counts = self._diagnostic_geometry_counts(
            self._bpy.context.evaluated_depsgraph_get(), object_names
        )

        original_corners, original_center, original_diagonal = self._render_bounds(
            object_names
        )
        original_bounds = self._bounds_metadata(
            original_corners, original_center, original_diagonal
        )
        if mode == "cross_section":
            axis_index = diagnostic_options["axis_index"]
            position = diagnostic_options["position"]
            if position is None:
                position = original_bounds["center"][axis_index]
            if not (
                original_bounds["minimum"][axis_index]
                < position
                < original_bounds["maximum"][axis_index]
            ):
                raise HandlerError("position must lie inside the selected geometry bounds")
            diagnostic_options = {**diagnostic_options, "position": position}

        scene = self._bpy.context.scene
        original_render_state = self._capture_render_state(scene)
        original_camera = scene.camera
        original_visibility = [
            (obj, obj.hide_render) for obj in scene.objects if obj.type != "LIGHT"
        ]
        diagnostic_objects: list[Any] = []
        diagnostic_meshes: list[Any] = []
        diagnostic_materials: list[Any] = []
        camera = None
        camera_data = None
        temporary_lights: list[tuple[Any | None, Any]] = []
        with self._workspace.stage_output(request) as output:
            try:
                self._apply_render_settings(scene, width, height, engine, samples)
                scene.render.image_settings.color_mode = "RGB"
                scene.render.image_settings.color_depth = "8"
                rendered_engine = scene.render.engine
                analysis = self._create_diagnostic_geometry(
                    mode,
                    object_names,
                    diagnostic_options,
                    diagnostic_counts,
                    diagnostic_objects,
                    diagnostic_meshes,
                    diagnostic_materials,
                )
                for obj, _hidden in original_visibility:
                    obj.hide_render = True
                for obj in diagnostic_objects:
                    scene.collection.objects.link(obj)
                self._bpy.context.view_layer.update()
                corners, center, diagonal = self._render_bounds()
                bounds = self._bounds_metadata(corners, center, diagonal)
                camera_data = self._bpy.data.cameras.new("PrintableDiagnosticCamera")
                camera_data.type = "ORTHO"
                camera = self._bpy.data.objects.new(
                    "PrintableDiagnosticCamera", camera_data
                )
                scene.collection.objects.link(camera)
                scene.camera = camera
                if not any(
                    obj.type == "LIGHT" and not obj.hide_render for obj in scene.objects
                ):
                    temporary_lights = self._create_review_lights(center, diagonal)
                self._frame_review_camera(
                    camera, corners, center, diagonal, view_direction, width, height
                )
                scene.render.filepath = str(output.path)
                result = self._bpy.ops.render.render(write_still=True)
                self._require_finished(result, "diagnostic render")
                size_bytes = output.path.stat().st_size
            finally:
                self._cleanup_diagnostic_render(
                    scene,
                    original_camera,
                    original_render_state,
                    original_visibility,
                    camera,
                    camera_data,
                    temporary_lights,
                    diagnostic_objects,
                    diagnostic_meshes,
                    diagnostic_materials,
                )
            finish_watchdog()
            output.commit()
        return {
            "path": request.relative,
            "size_bytes": size_bytes,
            "media_type": "image/png",
            "width": width,
            "height": height,
            "engine": rendered_engine,
            "render_device": (
                self._config.render_device if engine == "CYCLES" else "GRAPHICS"
            ),
            "graphics_backend": self._graphics_backend(),
            "samples": samples,
            "mode": mode,
            "objects": sorted(object_names) if object_names is not None else None,
            "source_bounds": original_bounds,
            "rendered_bounds": bounds,
            "analysis": analysis,
        }

    def _render_views(self, params: dict[str, Any]) -> dict[str, Any]:
        return self._render_views_scoped(params, reserved=False)

    def _job_render_views(self, params: dict[str, Any]) -> dict[str, Any]:
        return self._render_views_scoped(params, reserved=True)

    def _render_views_scoped(
        self, params: dict[str, Any], *, reserved: bool
    ) -> dict[str, Any]:
        _only_keys(
            params,
            {
                "views",
                "width",
                "height",
                "engine",
                "samples",
                "timeout_seconds",
                "max_output_bytes",
                "presentation",
            },
        )
        width, height, timeout_seconds, engine, samples = _render_parameters(params)
        remaining_output_bytes = _optional_output_budget(params)
        raw_views = params.get("views")
        if not isinstance(raw_views, list) or not 1 <= len(raw_views) <= MAX_RENDER_VIEWS:
            raise HandlerError(
                f"views must contain between 1 and {MAX_RENDER_VIEWS} entries"
            )
        if len(raw_views) * width * height > MAX_RENDER_VIEW_PIXELS:
            raise HandlerError(
                f"view renders exceed the {MAX_RENDER_VIEW_PIXELS}-pixel aggregate surface limit"
            )
        if width * height > MAX_REVIEW_SOURCE_PIXELS:
            raise HandlerError(
                f"each review source must be at most {MAX_REVIEW_SOURCE_PIXELS} pixels"
            )

        validator = (
            self._workspace.validate_reserved if reserved else self._workspace.validate
        )
        views: list[tuple[Any, str, tuple[float, float, float]]] = []
        paths: set[str] = set()
        for raw_view in raw_views:
            if not isinstance(raw_view, dict):
                raise HandlerError("each view must be an object")
            _only_keys(raw_view, {"path", "label", "direction"})
            request = validator(raw_view.get("path"), ".png")
            label = _string(raw_view, "label")
            raw_direction = raw_view.get("direction")
            if not isinstance(raw_direction, list) or len(raw_direction) != 3:
                raise HandlerError("view direction must contain three finite numbers")
            direction: list[float] = []
            for component in raw_direction:
                if isinstance(component, bool) or not isinstance(component, (int, float)):
                    raise HandlerError(
                        "view direction must contain three finite numbers"
                    )
                number = float(component)
                if not math.isfinite(number):
                    raise HandlerError(
                        "view direction must contain three finite numbers"
                    )
                direction.append(number)
            if math.sqrt(sum(component * component for component in direction)) < 1e-9:
                raise HandlerError("view direction must be non-zero")
            if request.relative in paths:
                raise HandlerError("view paths must be unique")
            paths.add(request.relative)
            views.append((request, label, tuple(direction)))

        raw_presentation = params.get("presentation")
        if raw_presentation is not None:
            object_names = self._renderable_product_names()
            presentation = self._validate_product_presentation(
                raw_presentation, object_names
            )
            return self._render_product_view_batch(
                views,
                object_names,
                presentation,
                width,
                height,
                timeout_seconds,
                engine,
                samples,
                remaining_output_bytes,
            )

        scene = self._bpy.context.scene
        original_render_state = self._capture_render_state(scene)
        with ExitStack() as stack:
            outputs = [
                stack.enter_context(self._workspace.stage_output(request))
                for request, _label, _direction in views
            ]
            deadline = time.monotonic() + timeout_seconds
            try:
                self._execution_watchdog.arm(deadline)
            except WatchdogError as error:
                raise HandlerError("render watchdog is unavailable") from error
            camera = None
            camera_data = None
            temporary_lights: list[tuple[Any | None, Any]] = []
            original_camera = scene.camera
            try:
                self._apply_render_settings(scene, width, height, engine, samples)
                scene.render.image_settings.color_mode = "RGB"
                scene.render.image_settings.color_depth = "8"
                rendered_engine = scene.render.engine
                corners, center, diagonal = self._render_bounds()
                bounds = self._bounds_metadata(corners, center, diagonal)
                camera_data = self._bpy.data.cameras.new("PrintableReviewCamera")
                camera_data.type = "ORTHO"
                camera = self._bpy.data.objects.new(
                    "PrintableReviewCamera", camera_data
                )
                scene.collection.objects.link(camera)
                scene.camera = camera
                if not any(obj.type == "LIGHT" for obj in scene.objects):
                    temporary_lights = self._create_review_lights(center, diagonal)

                rendered: list[dict[str, Any]] = []
                for (request, label, direction), output in zip(views, outputs):
                    self._frame_review_camera(
                        camera, corners, center, diagonal, direction, width, height
                    )
                    scene.render.filepath = str(output.path)
                    result = self._bpy.ops.render.render(write_still=True)
                    self._require_finished(result, "view render")
                    size_bytes = output.path.stat().st_size
                    if (
                        remaining_output_bytes is not None
                        and size_bytes > remaining_output_bytes
                    ):
                        raise HandlerError(
                            "view renders exceed the caller-selected output byte budget"
                        )
                    if remaining_output_bytes is not None:
                        remaining_output_bytes -= size_bytes
                    rendered.append(
                        {
                            "path": request.relative,
                            "label": label,
                            "size_bytes": size_bytes,
                            "media_type": "image/png",
                            "width": width,
                            "height": height,
                        }
                    )
            finally:
                try:
                    scene.camera = original_camera
                    self._restore_render_state(scene, original_render_state)
                    if camera is not None:
                        self._bpy.data.objects.remove(camera, do_unlink=True)
                    if camera_data is not None and camera_data.users == 0:
                        self._bpy.data.cameras.remove(camera_data)
                    self._remove_review_lights(temporary_lights)
                finally:
                    try:
                        self._execution_watchdog.disarm()
                    except WatchdogError as error:
                        raise HandlerError("render watchdog is unavailable") from error
            for output in outputs:
                output.commit()
        return {
            "views": rendered,
            "engine": rendered_engine,
            "render_device": (
                self._config.render_device if engine == "CYCLES" else "GRAPHICS"
            ),
            "graphics_backend": self._graphics_backend(),
            "samples": samples,
            "bounds": bounds,
        }

    def _render_product_view_batch(
        self,
        views: list[tuple[Any, str, tuple[float, float, float]]],
        object_names: list[str],
        presentation: dict[str, Any],
        width: int,
        height: int,
        timeout_seconds: float,
        engine: str,
        samples: int | None,
        remaining_output_bytes: int | None,
    ) -> dict[str, Any]:
        if presentation["profile"] != "engineering" and any(
            direction[2] < 0.0 for _request, _label, direction in views
        ):
            raise HandlerError(
                "grounded studio presentation cannot render a below-ground gallery or turntable view"
            )
        with ExitStack() as stack:
            outputs = [
                stack.enter_context(self._workspace.stage_output(request))
                for request, _label, _direction in views
            ]
            deadline = time.monotonic() + timeout_seconds
            try:
                self._execution_watchdog.arm(deadline)
            except WatchdogError as error:
                raise HandlerError("render watchdog is unavailable") from error
            try:
                with self._suspend_product_app_handlers():
                    selected = set(object_names)
                    geometry_usage = self._product_geometry_preflight(selected)
                    corners, center, diagonal = self._render_bounds(selected)
                    bounds = self._bounds_metadata(
                        corners, center, diagonal
                    )
                    source_signature = self._product_source_signature()
                    datablock_counts = self._product_datablock_counts()
                    rendered_views: list[dict[str, Any]] = []
                    presentation_views: list[dict[str, Any]] = []
                    rendered_engine = None
                    for (
                        request,
                        label,
                        direction,
                    ), output in zip(views, outputs):
                        view_presentation = dict(presentation)
                        view_presentation["view"] = (
                            self._product_view_from_direction(direction)
                        )
                        rendered = self._render_product_staged(
                            output.path,
                            object_names,
                            view_presentation,
                            corners,
                            center,
                            diagonal,
                            bounds,
                            width,
                            height,
                            engine,
                            samples,
                            remaining_output_bytes,
                            geometry_usage,
                            source_signature,
                            datablock_counts,
                        )
                        size_bytes = rendered["size_bytes"]
                        if remaining_output_bytes is not None:
                            remaining_output_bytes -= size_bytes
                        rendered_engine = rendered["engine"]
                        rendered_views.append(
                            {
                                "path": request.relative,
                                "label": label,
                                "size_bytes": size_bytes,
                                "media_type": "image/png",
                                "width": width,
                                "height": height,
                            }
                        )
                        presentation_view = rendered["presentation"]
                        presentation_view["source_state_verified"] = True
                        presentation_view["cleanup_verified"] = True
                        presentation_views.append(presentation_view)
                if self._product_source_signature() != source_signature:
                    raise HandlerError(
                        "source scene changed during product presentation"
                    )
                if self._product_datablock_counts() != datablock_counts:
                    raise HandlerError(
                        "product presentation cleanup was incomplete"
                    )
            finally:
                try:
                    self._execution_watchdog.disarm()
                except WatchdogError as error:
                    raise HandlerError("render watchdog is unavailable") from error
            self._workspace.commit_batch(outputs)
        return {
            "views": rendered_views,
            "engine": rendered_engine,
            "render_device": (
                self._config.render_device if engine == "CYCLES" else "GRAPHICS"
            ),
            "graphics_backend": self._graphics_backend(),
            "samples": samples,
            "bounds": bounds,
            "presentation": {
                "profile": presentation["profile"],
                "views": presentation_views,
            },
        }

    @staticmethod
    def _product_view_from_direction(
        direction: tuple[float, float, float]
    ) -> dict[str, float]:
        magnitude = math.hypot(direction[0], direction[1], direction[2])
        normalized = tuple(component / magnitude for component in direction)
        return {
            "azimuth_degrees": math.degrees(
                math.atan2(normalized[1], normalized[0])
            ),
            "elevation_degrees": math.degrees(
                math.asin(max(-1.0, min(1.0, normalized[2])))
            ),
        }

    @staticmethod
    def _product_framing_bounds(
        raw: Any,
        source_bounds: dict[str, Any],
        *,
        require_contains_geometry: bool,
    ) -> tuple[list[Any], Any, float, dict[str, Any]]:
        from mathutils import Vector  # type: ignore[import-not-found]

        if not isinstance(raw, dict):
            raise HandlerError("framing_bounds must be an object")
        _only_keys(
            raw,
            {
                "minimum",
                "maximum",
                "dimensions",
                "center",
                "diagonal",
                "coordinate_space",
                "unit",
            },
        )
        vectors: dict[str, list[float]] = {}
        for name in ("minimum", "maximum", "dimensions", "center"):
            value = raw.get(name)
            if (
                not isinstance(value, list)
                or len(value) != 3
                or any(
                    isinstance(component, bool)
                    or not isinstance(component, (int, float))
                    or not math.isfinite(float(component))
                    for component in value
                )
            ):
                raise HandlerError(
                    "framing_bounds vectors must contain three finite numbers"
                )
            vectors[name] = [float(component) for component in value]
        diagonal = raw.get("diagonal")
        if (
            isinstance(diagonal, bool)
            or not isinstance(diagonal, (int, float))
            or not math.isfinite(float(diagonal))
            or float(diagonal) < 0.1
            or raw.get("coordinate_space") != "world"
            or raw.get("unit") != "blender_unit"
        ):
            raise HandlerError("framing_bounds metadata is invalid")
        for axis in range(3):
            minimum = vectors["minimum"][axis]
            maximum = vectors["maximum"][axis]
            if (
                maximum < minimum
                or not math.isclose(
                    vectors["dimensions"][axis],
                    maximum - minimum,
                    rel_tol=1e-9,
                    abs_tol=1e-9,
                )
                or not math.isclose(
                    vectors["center"][axis],
                    (minimum + maximum) * 0.5,
                    rel_tol=1e-9,
                    abs_tol=1e-9,
                )
            ):
                raise HandlerError("framing_bounds geometry is inconsistent")
        expected_diagonal = max(
            math.hypot(*vectors["dimensions"]), 0.1
        )
        if not math.isclose(
            float(diagonal),
            expected_diagonal,
            rel_tol=1e-9,
            abs_tol=1e-9,
        ):
            raise HandlerError("framing_bounds diagonal is inconsistent")
        tolerance = (
            max(
                abs(value)
                for value in (
                    *vectors["minimum"],
                    *vectors["maximum"],
                )
            )
            * 1e-9
            + 1e-9
        )
        if require_contains_geometry and any(
            source_bounds["minimum"][axis]
            < vectors["minimum"][axis] - tolerance
            or source_bounds["maximum"][axis]
            > vectors["maximum"][axis] + tolerance
            for axis in range(3)
        ):
            raise HandlerError(
                "framing_bounds do not contain the rendered product geometry"
            )
        low = Vector(tuple(vectors["minimum"]))
        high = Vector(tuple(vectors["maximum"]))
        center = Vector(tuple(vectors["center"]))
        corners = [
            Vector((x, y, z))
            for x in (low.x, high.x)
            for y in (low.y, high.y)
            for z in (low.z, high.z)
        ]
        normalized = {
            "minimum": vectors["minimum"],
            "maximum": vectors["maximum"],
            "dimensions": vectors["dimensions"],
            "center": vectors["center"],
            "diagonal": float(diagonal),
            "coordinate_space": "world",
            "unit": "blender_unit",
        }
        return corners, center, float(diagonal), normalized

    def _apply_render_settings(
        self,
        scene: Any,
        width: int,
        height: int,
        engine: str,
        samples: int | None,
    ) -> None:
        if engine == "CYCLES":
            scene.render.engine = "CYCLES"
            scene.cycles.device = (
                "GPU" if self._config.render_device == "OPTIX" else "CPU"
            )
            scene.cycles.samples = samples
        else:
            self._select_eevee_engine(scene.render)
        scene.render.resolution_x = width
        scene.render.resolution_y = height
        scene.render.resolution_percentage = 100
        scene.render.image_settings.file_format = "PNG"

    @staticmethod
    def _capture_render_state(scene: Any) -> tuple[Any, ...]:
        return (
            scene.render.engine,
            scene.render.resolution_x,
            scene.render.resolution_y,
            scene.render.resolution_percentage,
            scene.render.image_settings.file_format,
            scene.render.image_settings.color_mode,
            scene.render.image_settings.color_depth,
            scene.render.filepath,
            scene.cycles.device,
            scene.cycles.samples,
        )

    @staticmethod
    def _restore_render_state(scene: Any, state: tuple[Any, ...]) -> None:
        (
            scene.render.engine,
            scene.render.resolution_x,
            scene.render.resolution_y,
            scene.render.resolution_percentage,
            scene.render.image_settings.file_format,
            scene.render.image_settings.color_mode,
            scene.render.image_settings.color_depth,
            scene.render.filepath,
            scene.cycles.device,
            scene.cycles.samples,
        ) = state

    def _missing_renderable_names(self, required_names: set[str]) -> list[str]:
        missing = set(required_names)
        depsgraph = self._bpy.context.evaluated_depsgraph_get()
        for instance in depsgraph.object_instances:
            if self._is_renderable_geometry(instance):
                missing.discard(self._instance_source_name(instance))
                if not missing:
                    break
        return sorted(missing)

    def _renderable_product_names(self) -> list[str]:
        names = {
            self._instance_source_name(instance)
            for instance in self._bpy.context.evaluated_depsgraph_get().object_instances
            if self._is_renderable_geometry(instance)
        }
        if not names:
            raise HandlerError("scene has no renderable product geometry")
        return sorted(names)

    def _product_geometry_preflight(
        self, required_names: set[str]
    ) -> dict[str, int]:
        missing = set(required_names)
        usage = {
            "instances": 0,
            "unique_evaluated_meshes": 0,
            "vertices": 0,
            "edges": 0,
            "faces": 0,
            "loops": 0,
            "attribute_values": 0,
            "material_slots": 0,
        }
        object_counts: dict[int, tuple[int, int, int, int, int, int]] = {}
        depsgraph = self._bpy.context.evaluated_depsgraph_get()
        for instance in depsgraph.object_instances:
            if not self._is_renderable_geometry(instance):
                continue
            source_name = self._instance_source_name(instance)
            if source_name not in required_names:
                continue
            missing.discard(source_name)
            usage["instances"] += 1
            _enforce_product_geometry_limits(usage)

            object_key = self._evaluated_object_key(instance.object)
            counts = object_counts.get(object_key)
            if counts is None:
                with self._evaluated_mesh(instance.object) as mesh:
                    if mesh is None:
                        raise HandlerError(
                            f"renderable object has no evaluated mesh: {source_name}"
                        )
                    attribute_values = 0
                    for attribute in getattr(mesh, "attributes", ()):
                        if getattr(attribute, "data_type", None) == "STRING":
                            raise HandlerError(
                                "product presentation geometry contains an unbounded string attribute; remove it or select other objects"
                            )
                        attribute_values += len(attribute.data)
                    attribute_values += sum(
                        len(getattr(vertex, "groups", ()))
                        for vertex in mesh.vertices
                    )
                    counts = (
                        len(mesh.vertices),
                        len(mesh.edges),
                        len(mesh.polygons),
                        len(mesh.loops),
                        attribute_values,
                        len(mesh.materials),
                    )
                object_counts[object_key] = counts
                usage["unique_evaluated_meshes"] += 1

            for kind, count in zip(
                (
                    "vertices",
                    "edges",
                    "faces",
                    "loops",
                    "attribute_values",
                    "material_slots",
                ),
                counts,
            ):
                usage[kind] += count
            _enforce_product_geometry_limits(usage)

        if missing:
            raise HandlerError(f"renderable object not found: {sorted(missing)[0]}")
        if usage["instances"] == 0:
            raise HandlerError("selected objects produced no evaluated presentation geometry")
        return usage

    @staticmethod
    def _evaluated_object_key(obj: Any) -> int:
        as_pointer = getattr(obj, "as_pointer", None)
        if callable(as_pointer):
            pointer = int(as_pointer())
            if pointer != 0:
                return pointer
        return id(obj)

    def _render_bounds(
        self, object_names: set[str] | None = None
    ) -> tuple[list[Any], Any, float]:
        from mathutils import Vector  # type: ignore[import-not-found]

        minimum_components: list[float] | None = None
        maximum_components: list[float] | None = None
        depsgraph = self._bpy.context.evaluated_depsgraph_get()
        for instance in depsgraph.object_instances:
            if not self._is_renderable_geometry(instance):
                continue
            if (
                object_names is not None
                and self._instance_source_name(instance) not in object_names
            ):
                continue
            with self._evaluated_mesh(instance.object) as mesh:
                if mesh is None:
                    continue
                for vertex in mesh.vertices:
                    world_vertex = instance.matrix_world @ Vector(vertex.co)
                    components = [float(world_vertex[axis]) for axis in range(3)]
                    if not all(math.isfinite(component) for component in components):
                        raise HandlerError("scene geometry has non-finite bounds")
                    if minimum_components is None:
                        minimum_components = components.copy()
                        maximum_components = components.copy()
                    else:
                        minimum_components = [
                            min(current, component)
                            for current, component in zip(minimum_components, components)
                        ]
                        maximum_components = [
                            max(current, component)
                            for current, component in zip(maximum_components, components)
                        ]
        if minimum_components is None or maximum_components is None:
            raise HandlerError("scene has no renderable geometry")
        minimum = Vector(tuple(minimum_components))
        maximum = Vector(tuple(maximum_components))
        corners = [
            Vector((x, y, z))
            for x in (minimum[0], maximum[0])
            for y in (minimum[1], maximum[1])
            for z in (minimum[2], maximum[2])
        ]
        center = (minimum + maximum) * 0.5
        raw_diagonal = float((maximum - minimum).length)
        if not math.isfinite(raw_diagonal):
            raise HandlerError("scene geometry bounds are too large to render")
        diagonal = max(raw_diagonal, 0.1)
        return corners, center, diagonal

    @staticmethod
    def _is_renderable_geometry(instance: Any) -> bool:
        obj = instance.object
        return (
            instance.show_self
            and obj.type in {"MESH", "CURVE", "SURFACE", "META", "FONT"}
            and not obj.hide_render
            and obj.visible_camera
        )

    @staticmethod
    def _instance_source_name(instance: Any) -> str:
        original = getattr(instance.object, "original", None)
        return str((original if original is not None else instance.object).name)

    @contextmanager
    def _evaluated_mesh(self, obj: Any) -> Iterator[Any | None]:
        if obj.type == "MESH":
            yield obj.data
            return
        mesh = obj.to_mesh()
        try:
            yield mesh
        finally:
            if mesh is not None:
                obj.to_mesh_clear()

    def _diagnostic_geometry_counts(
        self, depsgraph: Any, object_names: set[str] | None
    ) -> tuple[int, int, int, int, int, int]:
        source_instances = 0
        total_vertices = 0
        total_edges = 0
        total_faces = 0
        total_loops = 0
        total_attribute_values = 0
        for instance in depsgraph.object_instances:
            if not self._is_renderable_geometry(instance):
                continue
            source_name = self._instance_source_name(instance)
            if object_names is not None and source_name not in object_names:
                continue
            if instance.object.type != "MESH":
                raise HandlerError(
                    f"diagnostic analysis requires mesh geometry; convert {source_name} to a mesh, hide it, or select only mesh objects"
                )
            with self._evaluated_mesh(instance.object) as source_mesh:
                if source_mesh is None:
                    continue
                source_instances += 1
                total_vertices += len(source_mesh.vertices)
                total_edges += len(source_mesh.edges)
                total_faces += len(source_mesh.polygons)
                total_loops += len(source_mesh.loops)
                _enforce_diagnostic_topology_limits(
                    total_vertices,
                    total_edges,
                    total_faces,
                    total_loops,
                    "evaluated",
                )
                for attribute in getattr(source_mesh, "attributes", ()):
                    if getattr(attribute, "data_type", None) == "STRING":
                        raise HandlerError(
                            "diagnostic geometry contains an unbounded string attribute; remove it or select other mesh objects"
                        )
                    total_attribute_values += len(attribute.data)
                total_attribute_values += sum(
                    len(getattr(vertex, "groups", ()))
                    for vertex in source_mesh.vertices
                )
                if total_attribute_values > MAX_DIAGNOSTIC_ATTRIBUTE_VALUES:
                    raise HandlerError(
                        f"diagnostic geometry exceeds {MAX_DIAGNOSTIC_ATTRIBUTE_VALUES} copied attribute values; remove unused attributes or use objects to render a subset"
                    )
            determinant = float(instance.matrix_world.determinant())
            if not math.isfinite(determinant) or determinant == 0.0:
                raise HandlerError("diagnostic geometry has a non-invertible transform")
        return (
            source_instances,
            total_vertices,
            total_edges,
            total_faces,
            total_loops,
            total_attribute_values,
        )

    @staticmethod
    def _transform_diagnostic_mesh(bm: Any, matrix: Any, bmesh_module: Any) -> None:
        determinant = float(matrix.determinant())
        if not math.isfinite(determinant) or determinant == 0.0:
            raise HandlerError("diagnostic geometry has a non-invertible transform")
        bmesh_module.ops.transform(bm, matrix=matrix, verts=list(bm.verts))
        if determinant < 0.0:
            bmesh_module.ops.reverse_faces(bm, faces=list(bm.faces))
        bm.normal_update()

    @staticmethod
    def _append_diagnostic_faces(
        bm: Any,
        vertices: list[tuple[float, float, float]],
        faces: list[tuple[int, ...]],
        material_indices: list[int],
    ) -> None:
        vertex_indices: dict[Any, int] = {}
        for face in bm.faces:
            face_indices = []
            for vertex in face.verts:
                index = vertex_indices.get(vertex)
                if index is None:
                    index = len(vertices)
                    vertex_indices[vertex] = index
                    vertices.append(tuple(float(component) for component in vertex.co))
                face_indices.append(index)
            faces.append(tuple(face_indices))
            material_indices.append(face.material_index)

    @staticmethod
    def _ordered_cut_contours(cut_edges: list[Any]) -> list[list[Any]]:
        adjacency: dict[Any, list[Any]] = {}
        for edge in cut_edges:
            vertices = tuple(edge.verts)
            if len(vertices) != 2:
                raise HandlerError("cross-section cut contains an invalid edge")
            for vertex in vertices:
                adjacency.setdefault(vertex, []).append(edge)
        if any(len(edges) != 2 for edges in adjacency.values()):
            raise HandlerError(
                "cross-section cut contains an open or non-manifold contour"
            )

        unused = set(cut_edges)
        contours: list[list[Any]] = []
        while unused:
            first_edge = next(iter(unused))
            first_vertex = first_edge.verts[0]
            current_edge = first_edge
            current_vertex = first_vertex
            contour: list[Any] = []
            while True:
                contour.append(current_vertex)
                unused.remove(current_edge)
                next_vertex = current_edge.other_vert(current_vertex)
                if next_vertex is first_vertex:
                    break
                next_edges = [
                    edge for edge in adjacency[next_vertex] if edge in unused
                ]
                if len(next_edges) != 1:
                    raise HandlerError(
                        "cross-section cut contains an open or non-manifold contour"
                    )
                current_vertex = next_vertex
                current_edge = next_edges[0]
            if len(contour) < 3:
                raise HandlerError("cross-section cut contains a degenerate contour")
            contours.append(contour)
        return contours

    @classmethod
    def _fill_cross_section(cls, bm: Any, cut_edges: list[Any]) -> list[Any]:
        from mathutils.geometry import (  # type: ignore[import-not-found]
            tessellate_polygon,
        )

        polylines: list[list[Any]] = []
        tessellated_vertices: list[Any] = []
        for contour in cls._ordered_cut_contours(cut_edges):
            polyline = []
            for vertex in contour:
                polyline.append(vertex.co.copy())
                tessellated_vertices.append(vertex)
            polylines.append(polyline)

        triangles = tessellate_polygon(polylines)
        if not triangles:
            raise HandlerError("cross-section contours could not be capped")
        filled_faces: list[Any] = []
        for triangle in triangles:
            vertices = []
            for index in triangle:
                if (
                    type(index) is not int
                    or index < 0
                    or index >= len(tessellated_vertices)
                ):
                    raise HandlerError(
                        "cross-section tessellation returned an unknown vertex index"
                    )
                vertices.append(tessellated_vertices[index])
            if len(vertices) != 3 or len(set(vertices)) != 3:
                raise HandlerError(
                    "cross-section tessellation returned a degenerate face"
                )
            try:
                filled_faces.append(bm.faces.new(vertices))
            except ValueError as error:
                raise HandlerError(
                    "cross-section contours could not be capped"
                ) from error
        return filled_faces

    def _create_diagnostic_geometry(
        self,
        mode: str,
        object_names: set[str] | None,
        options: dict[str, Any],
        geometry_counts: tuple[int, int, int, int, int, int] | None,
        created_objects: list[Any],
        created_meshes: list[Any],
        created_materials: list[Any],
    ) -> dict[str, Any]:
        import bmesh  # type: ignore[import-not-found]
        from mathutils import Vector  # type: ignore[import-not-found]

        depsgraph = self._bpy.context.evaluated_depsgraph_get()
        if geometry_counts is None:
            geometry_counts = self._diagnostic_geometry_counts(
                depsgraph, object_names
            )
        (
            source_instances,
            total_vertices,
            total_edges,
            total_faces,
            total_loops,
            total_attribute_values,
        ) = geometry_counts

        if mode == "cross_section":
            colors = [
                (0.18, 0.42, 0.62, 1.0),
                (1.0, 0.18, 0.05, 1.0),
            ]
            names = ["PrintableSectionShell", "PrintableSectionCut"]
        else:
            colors = [
                (0.08, 0.62, 0.22, 1.0),
                (1.0, 0.68, 0.0, 1.0),
                (0.9, 0.04, 0.03, 1.0),
            ]
            names = [
                "PrintableSupported",
                "PrintableOverhangWarning",
                "PrintableOverhangSevere",
            ]
        for name, color in zip(names, colors):
            created_materials.append(self._create_diagnostic_material(name, color))

        section_faces = 0
        section_area = 0.0
        overhang = {
            "supported": {"faces": 0, "area": 0.0},
            "warning": {"faces": 0, "area": 0.0},
            "severe": {"faces": 0, "area": 0.0},
        }
        build_direction = (
            Vector(options["build_direction"]).normalized()
            if mode == "overhang"
            else None
        )
        built_vertices = 0
        built_edges = 0
        built_faces = 0
        built_loops = 0
        combined_vertices: list[tuple[float, float, float]] = []
        combined_faces: list[tuple[int, ...]] = []
        combined_material_indices: list[int] = []
        rendered_vertices = 0
        rendered_edges = 0
        rendered_faces = 0
        rendered_loops = 0
        for instance in depsgraph.object_instances:
            if not self._is_renderable_geometry(instance):
                continue
            source_name = self._instance_source_name(instance)
            if object_names is not None and source_name not in object_names:
                continue
            bm = bmesh.new()
            try:
                with self._evaluated_mesh(instance.object) as source_mesh:
                    if source_mesh is None:
                        continue
                    bm.from_mesh(source_mesh)
                built_vertices += len(bm.verts)
                built_edges += len(bm.edges)
                built_faces += len(bm.faces)
                built_loops += sum(len(face.loops) for face in bm.faces)
                if (
                    built_vertices > total_vertices
                    or built_edges > total_edges
                    or built_faces > total_faces
                    or built_loops > total_loops
                ):
                    raise HandlerError(
                        "diagnostic geometry changed after resource validation"
                    )
                self._transform_diagnostic_mesh(bm, instance.matrix_world, bmesh)
                if mode == "cross_section":
                    axis_index = options["axis_index"]
                    plane_co = Vector(
                        tuple(
                            options["position"] if index == axis_index else 0.0
                            for index in range(3)
                        )
                    )
                    plane_no = Vector(
                        tuple(1.0 if index == axis_index else 0.0 for index in range(3))
                    )
                    for face in bm.faces:
                        face.material_index = 0
                    cut = bmesh.ops.bisect_plane(
                        bm,
                        geom=list(bm.verts) + list(bm.edges) + list(bm.faces),
                        dist=1e-6,
                        plane_co=plane_co,
                        plane_no=plane_no,
                        use_snap_center=False,
                        clear_outer=True,
                        clear_inner=False,
                    )
                    cut_edges = [
                        geom
                        for geom in cut["geom_cut"]
                        if isinstance(geom, bmesh.types.BMEdge) and geom.is_valid
                    ]
                    filled_faces = (
                        self._fill_cross_section(bm, cut_edges) if cut_edges else []
                    )
                    for face in filled_faces:
                        face.material_index = 1
                        section_area += float(face.calc_area())
                    section_faces += len(filled_faces)
                else:
                    threshold = options["overhang_angle_degrees"]
                    for face in bm.faces:
                        angle = self._overhang_angle_degrees(
                            face.normal, build_direction
                        )
                        category, material_index = self._overhang_category(
                            angle, threshold
                        )
                        face.material_index = material_index
                        overhang[category]["faces"] += 1
                        overhang[category]["area"] += float(face.calc_area())
                if not bm.faces:
                    continue
                rendered_vertices += len(bm.verts)
                rendered_edges += len(bm.edges)
                rendered_faces += len(bm.faces)
                rendered_loops += sum(len(face.loops) for face in bm.faces)
                _enforce_diagnostic_topology_limits(
                    rendered_vertices,
                    rendered_edges,
                    rendered_faces,
                    rendered_loops,
                    "rendered",
                )
                self._append_diagnostic_faces(
                    bm,
                    combined_vertices,
                    combined_faces,
                    combined_material_indices,
                )
            finally:
                bm.free()
        if (
            built_vertices != total_vertices
            or built_edges != total_edges
            or built_faces != total_faces
            or built_loops != total_loops
        ):
            raise HandlerError("diagnostic geometry changed after resource validation")
        if not combined_faces:
            raise HandlerError("diagnostic plane produced no renderable geometry")
        mesh = self._bpy.data.meshes.new("PrintableDiagnosticMesh")
        created_meshes.append(mesh)
        mesh.from_pydata(combined_vertices, [], combined_faces)
        mesh.update()
        for material in created_materials:
            mesh.materials.append(material)
        if len(mesh.polygons) != len(combined_material_indices):
            raise HandlerError("diagnostic mesh construction changed face topology")
        for polygon, material_index in zip(
            mesh.polygons, combined_material_indices, strict=True
        ):
            polygon.material_index = material_index
        combined_vertices.clear()
        combined_faces.clear()
        combined_material_indices.clear()
        created_objects.append(
            self._bpy.data.objects.new("PrintableDiagnostic", mesh)
        )
        if mode == "cross_section":
            return {
                "axis": options["axis"],
                "position": options["position"],
                "source_instances": source_instances,
                "evaluated_vertices": total_vertices,
                "evaluated_edges": total_edges,
                "evaluated_faces": total_faces,
                "evaluated_loops": total_loops,
                "copied_attribute_values": total_attribute_values,
                "section_faces": section_faces,
                "section_area": section_area,
            }
        return {
            "build_direction": list(options["build_direction"]),
            "overhang_angle_degrees": options["overhang_angle_degrees"],
            "source_instances": source_instances,
            "evaluated_vertices": total_vertices,
            "evaluated_edges": total_edges,
            "evaluated_faces": total_faces,
            "evaluated_loops": total_loops,
            "copied_attribute_values": total_attribute_values,
            "categories": overhang,
        }

    def _create_diagnostic_material(
        self, name: str, color: tuple[float, float, float, float]
    ) -> Any:
        material = self._bpy.data.materials.new(name)
        try:
            material.diffuse_color = color
            material.use_nodes = True
            principled = material.node_tree.nodes.get("Principled BSDF")
            if principled is None:
                raise HandlerError("diagnostic material has no Principled BSDF")
            principled.inputs["Base Color"].default_value = color
            principled.inputs["Roughness"].default_value = 0.6
            return material
        except Exception:
            if material.users == 0:
                self._bpy.data.materials.remove(material)
            raise

    @staticmethod
    def _overhang_angle_degrees(normal: Any, build_direction: Any) -> float:
        downward = max(0.0, min(1.0, -float(normal.dot(build_direction))))
        return math.degrees(math.asin(downward))

    @staticmethod
    def _overhang_category(angle: float, threshold: float) -> tuple[str, int]:
        if angle <= threshold:
            return "supported", 0
        if angle <= min(90.0, threshold + 15.0):
            return "warning", 1
        return "severe", 2

    def _cleanup_diagnostic_render(
        self,
        scene: Any,
        original_camera: Any,
        original_render_state: tuple[Any, ...],
        original_visibility: list[tuple[Any, bool]],
        camera: Any | None,
        camera_data: Any | None,
        temporary_lights: list[tuple[Any | None, Any]],
        diagnostic_objects: list[Any],
        diagnostic_meshes: list[Any],
        diagnostic_materials: list[Any],
    ) -> None:
        first_error: Exception | None = None

        def attempt(operation: Callable[[], None]) -> None:
            nonlocal first_error
            try:
                operation()
            except Exception as error:
                if first_error is None:
                    first_error = error

        attempt(lambda: setattr(scene, "camera", original_camera))
        attempt(lambda: self._restore_render_state(scene, original_render_state))
        for obj, hidden in original_visibility:
            attempt(lambda obj=obj, hidden=hidden: setattr(obj, "hide_render", hidden))
        if camera is not None:
            attempt(lambda: self._bpy.data.objects.remove(camera, do_unlink=True))
        if camera_data is not None and camera_data.users == 0:
            attempt(lambda: self._bpy.data.cameras.remove(camera_data))
        attempt(lambda: self._remove_review_lights(temporary_lights))
        for obj in reversed(diagnostic_objects):
            attempt(lambda obj=obj: self._bpy.data.objects.remove(obj, do_unlink=True))
        for mesh in reversed(diagnostic_meshes):
            if mesh.users == 0:
                attempt(lambda mesh=mesh: self._bpy.data.meshes.remove(mesh))
        for material in reversed(diagnostic_materials):
            if material.users == 0:
                attempt(lambda material=material: self._bpy.data.materials.remove(material))
        attempt(self._bpy.context.view_layer.update)
        if first_error is not None:
            raise first_error

    @staticmethod
    def _bounds_metadata(
        corners: list[Any], _center: Any, _diagonal: float
    ) -> dict[str, Any]:
        minimum = [min(float(corner[axis]) for corner in corners) for axis in range(3)]
        maximum = [max(float(corner[axis]) for corner in corners) for axis in range(3)]
        dimensions = [high - low for low, high in zip(minimum, maximum)]
        center = [(low + high) * 0.5 for low, high in zip(minimum, maximum)]
        diagonal = max(math.sqrt(sum(value * value for value in dimensions)), 0.1)
        return {
            "minimum": minimum,
            "maximum": maximum,
            "dimensions": dimensions,
            "center": center,
            "diagonal": diagonal,
            "coordinate_space": "world",
            "unit": "blender_unit",
        }

    def _frame_review_camera(
        self,
        camera: Any,
        corners: list[Any],
        center: Any,
        diagonal: float,
        raw_direction: tuple[float, float, float],
        width: int,
        height: int,
    ) -> None:
        from mathutils import Vector  # type: ignore[import-not-found]

        direction = Vector(raw_direction).normalized()
        camera.location = center + direction * max(diagonal * 2.0, 1.0)
        toward_target = center - camera.location
        up_axis = "X" if abs(direction.z) > 0.999 else "Y"
        camera.rotation_euler = toward_target.to_track_quat("-Z", up_axis).to_euler()
        camera.data.clip_start = max(diagonal / 10000.0, 0.0001)
        camera.data.clip_end = max(diagonal * 10.0, 10.0)
        self._bpy.context.view_layer.update()
        inverse = camera.matrix_world.inverted()
        camera_corners = [inverse @ corner for corner in corners]
        span_x = max(corner.x for corner in camera_corners) - min(
            corner.x for corner in camera_corners
        )
        span_y = max(corner.y for corner in camera_corners) - min(
            corner.y for corner in camera_corners
        )
        aspect = width / height
        camera.data.ortho_scale = max(span_y, span_x / aspect, 0.1) * 1.15

    def _create_review_lights(
        self, center: Any, diagonal: float
    ) -> list[tuple[Any | None, Any]]:
        from mathutils import Vector  # type: ignore[import-not-found]

        created: list[tuple[Any | None, Any]] = []
        scale = max(diagonal, 1.0)
        try:
            for name, offset, base_energy in (
                ("PrintableReviewKey", (1.5, -1.5, 2.0), 1000.0),
                ("PrintableReviewFill", (-1.0, 0.5, 1.0), 500.0),
            ):
                light_data = self._bpy.data.lights.new(name=name, type="AREA")
                created.append((None, light_data))
                light_data.energy = base_energy * scale * scale
                light_data.shape = "DISK"
                light_data.size = scale
                light = self._bpy.data.objects.new(name, light_data)
                created[-1] = (light, light_data)
                self._bpy.context.scene.collection.objects.link(light)
                light.location = center + Vector(offset) * scale
                light.rotation_euler = (center - light.location).to_track_quat(
                    "-Z", "Y"
                ).to_euler()
            return created
        except Exception as create_error:
            try:
                self._remove_review_lights(created)
            except Exception as cleanup_error:
                raise cleanup_error from create_error
            raise

    def _remove_review_lights(
        self, created: list[tuple[Any | None, Any]]
    ) -> None:
        for light, light_data in reversed(created):
            if light is not None:
                self._bpy.data.objects.remove(light, do_unlink=True)
            if light_data.users == 0:
                self._bpy.data.lights.remove(light_data)

    @staticmethod
    def _graphics_backend() -> dict[str, str] | None:
        try:
            import gpu  # type: ignore[import-not-found]

            return {
                "backend": str(gpu.platform.backend_type_get()),
                "device_type": str(gpu.platform.device_type_get()),
                "renderer": str(gpu.platform.renderer_get()),
                "vendor": str(gpu.platform.vendor_get()),
                "version": str(gpu.platform.version_get()),
            }
        except (ImportError, SystemError):
            return None

    @staticmethod
    def _select_eevee_engine(render: Any) -> None:
        for engine in ("BLENDER_EEVEE", "BLENDER_EEVEE_NEXT"):
            try:
                render.engine = engine
            except (TypeError, ValueError):
                continue
            if render.engine == engine:
                return
        raise HandlerError("this Blender build does not expose an EEVEE render engine")

    def _bridge_test_wait(self, params: dict[str, Any]) -> dict[str, Any]:
        _only_keys(params, {"seconds"})
        raw_duration = params.get("seconds", 30.0)
        if isinstance(raw_duration, bool) or not isinstance(
            raw_duration, (int, float)
        ):
            raise HandlerError("seconds must be between 0 and 120")
        duration = float(raw_duration)
        if not math.isfinite(duration) or not 0 < duration <= 120:
            raise HandlerError("seconds must be between 0 and 120")
        deadline = time.monotonic() + duration
        while time.monotonic() < deadline:
            if self._shutdown_requested():
                raise HandlerError("test wait interrupted by shutdown")
            time.sleep(min(0.05, deadline - time.monotonic()))
        return {"waited_seconds": duration}

    def _object_summary(self, obj: Any) -> dict[str, Any]:
        summary: dict[str, Any] = {
            "name": obj.name,
            "type": obj.type,
            "location": list(obj.location),
            "rotation_euler": list(obj.rotation_euler),
            "scale": list(obj.scale),
            "dimensions": list(obj.dimensions),
        }
        if obj.type == "MESH":
            summary["vertices"] = len(obj.data.vertices)
            summary["polygons"] = len(obj.data.polygons)
        return summary

    def _ensure_camera_and_light(self) -> None:
        from mathutils import Vector  # type: ignore[import-not-found]

        scene = self._bpy.context.scene
        if scene.camera is None:
            self._bpy.ops.object.camera_add(location=(5.5, -5.5, 4.5))
            camera = self._bpy.context.active_object
            camera.rotation_euler = (
                Vector((0.0, 0.0, 0.0)) - camera.location
            ).to_track_quat("-Z", "Y").to_euler()
            scene.camera = camera
        if not any(obj.type == "LIGHT" for obj in scene.objects):
            self._bpy.ops.object.light_add(type="AREA", location=(4.0, -4.0, 6.0))
            light = self._bpy.context.active_object
            light.data.energy = 1000
            light.data.shape = "DISK"
            light.data.size = 5.0

    @staticmethod
    def _require_finished(result: Any, operation: str) -> None:
        if "FINISHED" not in result:
            raise HandlerError(f"{operation} did not finish")
