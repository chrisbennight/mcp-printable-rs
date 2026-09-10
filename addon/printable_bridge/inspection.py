"""Read-only, paginated Blender modeling inspection."""

from itertools import islice


class InspectionError(ValueError):
    pass


def page_arguments(params):
    offset = params.get("offset", 0)
    limit = params.get("limit", 20)
    if type(offset) is not int or not 0 <= offset <= 1_000_000:
        raise InspectionError("offset must be between 0 and 1000000")
    if type(limit) is not int or not 1 <= limit <= 100:
        raise InspectionError("limit must be between 1 and 100")
    return offset, limit


def page(items, offset, limit, project):
    records = [project(item) for item in islice(items, offset, offset + limit)]
    end = offset + len(records)
    return {
        "items": records,
        "total": len(items),
        "next_offset": end if end < len(items) else None,
    }


def object_details(obj, section, offset, limit):
    if section == "materials":
        return page(obj.material_slots, offset, limit, lambda slot: {
            "slot": slot.name,
            "link": slot.link,
            "material": slot.material.name if slot.material else None,
            "uses_nodes": bool(slot.material and slot.material.use_nodes),
        })
    if section == "modifiers":
        return page(obj.modifiers, offset, limit, lambda modifier: {
            "name": modifier.name,
            "type": modifier.type,
            "show_viewport": modifier.show_viewport,
            "show_render": modifier.show_render,
            **({"node_group": modifier.node_group.name if modifier.node_group else None}
               if modifier.type == "NODES" else {}),
        })
    if section == "hierarchy":
        return {
            "parent": obj.parent.name if obj.parent else None,
            "children": page(obj.children, offset, limit, lambda child: child.name),
            "collections": page(obj.users_collection, offset, limit, lambda col: col.name),
        }
    raise InspectionError("section must be summary, materials, modifiers, or hierarchy")


def node_tree_info(bpy, params):
    if set(params) - {"name", "kind", "section", "offset", "limit"}:
        raise InspectionError("unknown node-tree inspection parameter")
    name = params.get("name")
    if not isinstance(name, str) or not name or len(name) > 255:
        raise InspectionError("name must contain between 1 and 255 characters")
    kind = params.get("kind", "material")
    section = params.get("section", "nodes")
    if kind not in ("material", "geometry") or section not in ("nodes", "links"):
        raise InspectionError("kind must be material or geometry; section must be nodes or links")
    offset, limit = page_arguments(params)
    if kind == "material":
        material = bpy.data.materials.get(name)
        if material is None:
            raise InspectionError(f"material not found: {name}")
        tree = material.node_tree
    else:
        tree = bpy.data.node_groups.get(name)
        if tree is None or tree.bl_idname != "GeometryNodeTree":
            raise InspectionError(f"geometry node group not found: {name}")
    if tree is None:
        return {"name": name, "kind": kind, "section": section, "tree": None,
                "items": [], "total": 0, "next_offset": None}
    if section == "nodes":
        result = page(tree.nodes, offset, limit, lambda node: {
            "name": node.name,
            "type": node.bl_idname,
            "mute": node.mute,
            **({"node_group": node.node_tree.name if node.node_tree else None}
               if node.type == "GROUP" else {}),
            **({"is_active_output": node.is_active_output}
               if node.type in {"OUTPUT_MATERIAL", "GROUP_OUTPUT"} else {}),
        })
    else:
        result = page(tree.links, offset, limit, lambda link: {
            "from_node": link.from_node.name,
            "from_socket": link.from_socket.identifier,
            "to_node": link.to_node.name,
            "to_socket": link.to_socket.identifier,
            "is_muted": link.is_muted,
            "is_valid": link.is_valid,
        })
    return {"name": name, "kind": kind, "section": section, "tree": tree.name, **result}
