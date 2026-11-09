"""Prepare self-contained CAD GLB snapshots for Blender without changing geometry."""

from contextlib import contextmanager
import hashlib
import json
import os
from pathlib import Path
import shutil
import struct
import tempfile


def read_self_contained_glb(original):
    """Read a GLB header and reject file references outside its own bytes."""
    header = original.read(20)
    if len(header) != 20:
        raise ValueError("GLB header is incomplete")
    magic, version, total, json_size, kind = struct.unpack("<5I", header)
    if (magic != 0x46546C67 or version != 2 or total != os.fstat(original.fileno()).st_size
            or kind != 0x4E4F534A or json_size > total - 20 or json_size % 4):
        raise ValueError("model import requires a complete GLB 2.0 document")
    document = json.loads(original.read(json_size))
    if not isinstance(document, dict):
        raise ValueError("GLB document must be an object")
    for key in ("buffers", "images"):
        items = document.get(key, [])
        if not isinstance(items, list) or any(not isinstance(item, dict) for item in items):
            raise ValueError("GLB buffers and images must be arrays of objects")
        if any("uri" in item for item in items):
            raise ValueError("model import requires self-contained GLB buffers and images")
    return (magic, version, total, json_size, kind), document


@contextmanager
def prepare_glb(source: Path, inventory: Path):
    names = json.loads(inventory.read_bytes()).get("nodes")
    if not isinstance(names, dict):
        raise ValueError("CAD component inventory requires a node map")
    with source.open("rb") as original:
        digest = hashlib.file_digest(original, "sha256").hexdigest()
        original.seek(0)
        (magic, version, total, json_size, kind), document = read_self_contained_glb(original)
        nodes = document.get("nodes", [])
        if not isinstance(nodes, list):
            raise ValueError("CAD GLB nodes must be an array")
        roots = document["scenes"][document.get("scene", 0)]["nodes"]
        pending = [(index, "") for index in roots]
        seen = set()
        matched = set()
        while pending:
            index, parent = pending.pop()
            if type(index) is not int or index < 0 or index >= len(nodes) or index in seen:
                raise ValueError("CAD hierarchy contains an invalid or repeated node reference")
            seen.add(index)
            node = nodes[index]
            name = node.get("name")
            if name is not None and not isinstance(name, str):
                raise ValueError("CAD node names must be strings")
            path = (f"{parent}/{name}" if parent else name) if name else parent
            extras = node.setdefault("extras", {})
            if not isinstance(extras, dict):
                raise ValueError("CAD node metadata must be an object")
            if name:
                extras["printable_cad_node"] = path
            if name and path in names:
                record = names[path]
                for key in ("source_name", "occurrence_name", "product_name"):
                    value = record.get(key)
                    if value is not None:
                        if not isinstance(value, str):
                            raise ValueError("CAD source names must be strings")
                        extras[f"printable_cad_{key}"] = value
                matched.add(path)
            pending.extend((child, path) for child in node.get("children", []))
        encoded = json.dumps(document, allow_nan=False, separators=(",", ":")).encode()
        encoded += b" " * (-len(encoded) % 4)
        with tempfile.NamedTemporaryFile(suffix=".glb", dir=source.parent) as prepared:
            prepared.write(struct.pack("<5I", magic, version, total - json_size + len(encoded),
                                       len(encoded), kind))
            prepared.write(encoded)
            shutil.copyfileobj(original, prepared)
            prepared.flush()
            yield Path(prepared.name), {"source_sha256": digest, "node_count": len(seen),
                                       "named_nodes": len(matched), "unmatched_names": len(names) - len(matched)}
