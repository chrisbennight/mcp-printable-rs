"""Native viewport drawing and explicit editor capture without fallback."""

import hashlib
from array import array
import json
import math
import time

from .editor_context import context_summary, execution_context
from .x11_capture import DisplayCaptureError, capture_rgb

DEFAULT_CAPTURE_TIMEOUT_SECONDS = 30.0


class NativeViewError(ValueError):
    pass


def _number(value, name, minimum, maximum):
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise NativeViewError(f"{name} must be a finite number")
    value = float(value)
    if not math.isfinite(value) or not minimum <= value <= maximum:
        raise NativeViewError(f"{name} is outside its supported range")
    return value


def _vector(value, name, size):
    if not isinstance(value, list) or len(value) != size:
        raise NativeViewError(f"{name} requires {size} finite components")
    return [_number(item, name, -1e12, 1e12) for item in value]


def view_options(value):
    if value is None:
        return {}
    if not isinstance(value, dict) or set(value) - {
        "axis", "location", "rotation", "distance", "perspective", "shading", "overlays", "xray"
    }:
        raise NativeViewError("view requires supported viewport options")
    result = dict(value)
    for name, choices in {
        "axis": {"FRONT", "BACK", "LEFT", "RIGHT", "TOP", "BOTTOM"},
        "perspective": {"PERSP", "ORTHO", "CAMERA"},
        "shading": {"WIREFRAME", "SOLID", "MATERIAL", "RENDERED"},
    }.items():
        if name in result and (not isinstance(result[name], str) or result[name] not in choices):
            raise NativeViewError(f"unsupported {name}")
    for name in ("overlays", "xray"):
        if name in result and type(result[name]) is not bool:
            raise NativeViewError(f"{name} must be a boolean")
    if "location" in result:
        result["location"] = _vector(result["location"], "location", 3)
    if "rotation" in result:
        if "axis" in result:
            raise NativeViewError("choose axis or rotation, not both")
        rotation = _vector(result["rotation"], "rotation", 4)
        norm = math.sqrt(sum(component * component for component in rotation))
        if norm < 1e-12:
            raise NativeViewError("rotation quaternion must be nonzero")
        result["rotation"] = [component / norm for component in rotation]
    if "distance" in result:
        result["distance"] = _number(result["distance"], "distance", 1e-6, 1e12)
    return result


def _finished(result, operation):
    if "FINISHED" not in result:
        raise NativeViewError(f"{operation} did not finish")


def apply_view(bpy, options):
    if not options:
        return
    if bpy.context.area.type != "VIEW_3D":
        raise NativeViewError("viewport options require a VIEW_3D editor")
    space = bpy.context.space_data
    region = space.region_3d
    if "axis" in options:
        _finished(bpy.ops.view3d.view_axis(type=options["axis"]), "view axis selection")
    for name, attribute in (("location", "view_location"), ("rotation", "view_rotation"),
                            ("distance", "view_distance"), ("perspective", "view_perspective")):
        if name in options:
            setattr(region, attribute, options[name])
    if "shading" in options:
        space.shading.type = options["shading"]
    if "overlays" in options:
        space.overlay.show_overlays = options["overlays"]
    if "xray" in options:
        space.shading.show_xray = options["xray"]
    region.update()


def view_configuration(bpy, selector):
    context = bpy.context
    area = context.area
    space = context.space_data
    value = {
        "selector": {"window": selector.get("window", 0),
                     "area_type": selector["area_type"],
                     "area_index": selector.get("area_index", 0),
                     "region_type": selector.get("region_type", "WINDOW")},
        "ui_type": area.ui_type,
        "editor_size": [area.width, area.height],
        "region_size": [context.region.width, context.region.height],
    }
    if area.type == "VIEW_3D":
        region = space.region_3d
        value["view"] = {
            "location": list(region.view_location), "rotation": list(region.view_rotation),
            "distance": region.view_distance, "perspective": region.view_perspective,
            "shading": space.shading.type, "overlays": space.overlay.show_overlays,
            "xray": space.shading.show_xray,
        }
        value["projection"] = {"lens": space.lens, "clip_start": space.clip_start,
                               "clip_end": space.clip_end}
    elif area.type == "NODE_EDITOR":
        tree = space.edit_tree
        value["node_tree"] = tree.name if tree else None
    elif area.type == "IMAGE_EDITOR":
        value["image"] = space.image.name if space.image else None
    if area.type in {"NODE_EDITOR", "IMAGE_EDITOR"}:
        view2d = context.region.view2d
        value["view_bounds"] = [list(view2d.region_to_view(0, 0)),
                                list(view2d.region_to_view(context.region.width, context.region.height))]
    return value


def _viewport_png(bpy, gpu, path, maximum):
    context = bpy.context
    region = context.region
    space = context.space_data
    if context.area.type != "VIEW_3D" or region.type != "WINDOW":
        raise NativeViewError("native viewport drawing requires a VIEW_3D WINDOW region")
    scale = min(1.0, maximum / max(region.width, region.height))
    width, height = max(1, round(region.width * scale)), max(1, round(region.height * scale))
    offscreen = gpu.types.GPUOffScreen(width, height)
    image = None
    try:
        space.region_3d.update()
        offscreen.draw_view3d(
            context.scene, context.view_layer, space, region,
            space.region_3d.view_matrix, space.region_3d.window_matrix,
            do_color_management=True,
        )
        with offscreen.bind():
            pixels = gpu.state.active_framebuffer_get().read_color(0, 0, width, height, 4, 0, 'UBYTE')
        pixels.dimensions = width * height * 4
        image = bpy.data.images.new("Printable native observation", width, height, alpha=True)
        image.pixels.foreach_set(array('f', (component / 255.0 for component in pixels)))
        image.filepath_raw = str(path)
        image.file_format = "PNG"
        image.save()
    finally:
        try:
            if image is not None:
                bpy.data.images.remove(image)
        finally:
            offscreen.free()
    return width, height


def _editor_png(bpy, path, maximum):
    area = bpy.context.area
    target = {"window": bpy.context.window, "area": area, "region": bpy.context.region}
    expected_size = [area.width, area.height]
    if area.width * area.height > 8388608 or max(area.width, area.height) > 4096:
        raise NativeViewError("editor capture exceeds the bounded source pixel budget")
    with bpy.context.temp_override(**target):
        _finished(bpy.ops.wm.redraw_timer(type="DRAW_WIN_SWAP", iterations=1), "editor redraw")
    with bpy.context.temp_override(**target):
        try:
            rgb, source_width, source_height = capture_rgb(target["window"], area)
        except DisplayCaptureError as error:
            raise NativeViewError(str(error)) from error
    source_size = [source_width, source_height]
    if source_size != expected_size:
        raise NativeViewError("captured image does not match the selected editor dimensions")
    rgba = bytearray(source_width * source_height * 4)
    rgba[3::4] = b'\xff' * (source_width * source_height)
    for row in range(source_height):
        source = memoryview(rgb)[row * source_width * 3:(row + 1) * source_width * 3]
        offset = (source_height - 1 - row) * source_width * 4
        for channel in range(3):
            rgba[offset + channel:offset + source_width * 4:4] = source[channel::3]
    image = bpy.data.images.new("Printable editor observation", source_width, source_height, alpha=True)
    try:
        image.pixels.foreach_set(array('f', (component / 255.0 for component in rgba)))
        scale = min(1.0, maximum / max(source_size))
        width, height = [max(1, round(size * scale)) for size in source_size]
        if [width, height] != source_size:
            image.scale(width, height)
        image.filepath_raw = str(path)
        image.file_format = "PNG"
        image.save()
        return width, height, source_size
    finally:
        bpy.data.images.remove(image)


def capture(bpy, gpu, path, params):
    started = time.monotonic()
    method = params.get("method", "viewport")
    if not isinstance(method, str) or method not in {"viewport", "editor"}:
        raise NativeViewError("method must be viewport or editor")
    maximum = params.get("max_size", 1024)
    if type(maximum) is not int or not 64 <= maximum <= 2048:
        raise NativeViewError("max_size must be between 64 and 2048")
    options = view_options(params.get("view"))
    selector = params.get("context") or {"area_type": "VIEW_3D"}
    if bpy.app.background:
        raise NativeViewError("native observation requires normal Blender with a private display")
    with execution_context(bpy, selector):
        target = {"window": bpy.context.window, "area": bpy.context.area, "region": bpy.context.region}
        if bpy.context.region.width <= 0 or bpy.context.region.height <= 0:
            raise NativeViewError("requested editor region has no drawable area")
        if method == "viewport" and (bpy.context.area.type != "VIEW_3D" or bpy.context.region.type != "WINDOW"):
            raise NativeViewError("native viewport drawing requires a VIEW_3D WINDOW region")
        if method == "editor":
            area = bpy.context.area
            if area.width <= 0 or area.height <= 0:
                raise NativeViewError("requested editor has no drawable area")
            if area.width * area.height > 8388608 or max(area.width, area.height) > 4096:
                raise NativeViewError("editor capture exceeds the bounded source pixel budget")
        apply_view(bpy, options)
        bpy.context.view_layer.update()
        if method == "viewport":
            width, height = _viewport_png(bpy, gpu, path, maximum)
            source_size = [width, height]
            redraw = "offscreen_draw_view3d"
        else:
            width, height, source_size = _editor_png(bpy, path, maximum)
            redraw = "window_draw_swap"
        with bpy.context.temp_override(**target):
            configuration = view_configuration(bpy, selector)
            state = context_summary(bpy)
            state["subframe"] = bpy.context.scene.frame_subframe
        encoded = json.dumps({"configuration": configuration, "context": state},
                             sort_keys=True, separators=(",", ":"), allow_nan=False).encode()
        shading = configuration.get("view", {}).get("shading")
        return {
            **state, "method": method, "width": width, "height": height,
            "source_size": source_size, "resized": source_size != [width, height],
            "view_configuration": configuration,
            "view_state": {"configuration_sha256": hashlib.sha256(encoded).hexdigest()},
            "fidelity": "editor_pixels" if method == "editor" else "native_viewport_draw",
            "overlay_fidelity": "editor_pixels" if method == "editor" else "not_guaranteed",
            "capture_source": "private_display" if method == "editor" else "gpu_offscreen",
            "occlusion": "visible_display_composition" if method == "editor" else "not_applicable",
            "freshness": {"dependency_evaluated": True, "redraw": redraw},
            "convergence": "not_progressive" if method == "viewport" and shading in {"WIREFRAME", "SOLID"} else "not_reported",
            "captured_at_unix_ms": round(time.time() * 1000),
            "elapsed_ms": round((time.monotonic() - started) * 1000),
            "graphics_backend": {"vendor": gpu.platform.vendor_get(), "renderer": gpu.platform.renderer_get()},
        }
