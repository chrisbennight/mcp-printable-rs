"""Explicit UI targeting and bounded inspection of Blender editing state."""

from contextlib import contextmanager, nullcontext
from itertools import islice

from .inspection import InspectionError, page_arguments


class EditorContextError(ValueError):
    pass


def _index(params, name, maximum):
    value = params.get(name, 0)
    if type(value) is not int or not 0 <= value <= maximum:
        raise EditorContextError(f"{name} must be between 0 and {maximum}")
    return value


def _name(params, name, default=None):
    value = params.get(name, default)
    if not isinstance(value, str) or not 1 <= len(value) <= 64:
        raise EditorContextError(f"{name} must contain between 1 and 64 characters")
    return value


def resolve_editor(bpy, selector):
    if not isinstance(selector, dict) or set(selector) - {
        "window", "area_type", "area_index", "region_type", "expected_mode"
    }:
        raise EditorContextError("context requires a typed editor selector")
    window_index = _index(selector, "window", 63)
    area_index = _index(selector, "area_index", 255)
    area_type = _name(selector, "area_type")
    region_type = _name(selector, "region_type", "WINDOW")
    if "expected_mode" in selector:
        _name(selector, "expected_mode")
    windows = bpy.context.window_manager.windows
    if window_index >= len(windows):
        raise EditorContextError("requested Blender window is unavailable")
    window = windows[window_index]
    area = next(islice((a for a in window.screen.areas if a.type == area_type),
                      area_index, area_index + 1), None)
    if area is None:
        raise EditorContextError("requested Blender editor is unavailable")
    region = next((r for r in area.regions if r.type == region_type), None)
    if region is None:
        raise EditorContextError("requested Blender region is unavailable")
    return {"window": window, "area": area, "region": region}


def context_summary(bpy):
    context = bpy.context
    active = context.view_layer.objects.active
    return {
        "scene": context.scene.name,
        "view_layer": context.view_layer.name,
        "frame": context.scene.frame_current,
        "mode": context.mode,
        "active_object": active.name if active else None,
        "area_type": context.area.type if context.area else None,
        "region_type": context.region.type if context.region else None,
    }


@contextmanager
def execution_context(bpy, selector):
    manager = nullcontext() if selector is None else bpy.context.temp_override(
        **resolve_editor(bpy, selector)
    )
    with manager:
        if selector is not None and "expected_mode" in selector:
            if bpy.context.mode != selector["expected_mode"]:
                raise EditorContextError("requested editing mode does not match current context")
        yield


def editing_state(bpy, params):
    if set(params) - {"section", "offset", "limit"}:
        raise InspectionError("unknown editing-state parameter")
    offset, limit = page_arguments(params)
    section = params.get("section", "editors")
    summary = context_summary(bpy)
    if section == "selection":
        objects = bpy.context.selected_objects
        items = [obj.name for obj in islice(objects, offset, offset + limit)]
        total = len(objects)
    elif section == "editors":
        items = []
        total = 0
        for window_index, window in enumerate(bpy.context.window_manager.windows):
            counts = {}
            for area in window.screen.areas:
                area_index = counts.get(area.type, 0)
                counts[area.type] = area_index + 1
                if offset <= total < offset + limit:
                    items.append({
                        "window": window_index, "area_type": area.type,
                        "area_index": area_index, "ui_type": area.ui_type,
                        "width": area.width, "height": area.height,
                        "regions": [region.type for region in area.regions],
                    })
                total += 1
    else:
        raise InspectionError("section must be editors or selection")
    end = offset + len(items)
    return {**summary, "section": section, "items": items, "total": total,
            "next_offset": end if end < total else None}
