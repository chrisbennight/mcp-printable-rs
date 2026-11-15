"""Public-safe multi-plate layout derived from the smoke's own native cube package."""

from copy import deepcopy
import json
import math
from pathlib import PurePosixPath
import uuid
import xml.etree.ElementTree as ET
import zipfile


CORE = "http://schemas.microsoft.com/3dmanufacturing/core/2015/02"
PRODUCTION = "http://schemas.microsoft.com/3dmanufacturing/production/2015/06"
ET.register_namespace("", CORE)
ET.register_namespace("p", PRODUCTION)
COLORS = ["#E74C3C", "#3498DB"]


def metadata(parent, key, value):
    item = next((item for item in parent.findall("metadata") if item.get("key") == key), None)
    if item is None:
        item = ET.SubElement(parent, "metadata", {"key": key})
    item.set("value", str(value))


def transform(point, values):
    return [sum(point[column] * values[column * 3 + row] for column in range(3))
            + values[9 + row] for row in range(3)]


def object_vertices(package, model_path, object_id):
    model = ET.fromstring(package.read(model_path))
    obj = next(obj for obj in model.findall(f"{{{CORE}}}resources/{{{CORE}}}object")
               if obj.get("id") == str(object_id))
    vertices = obj.findall(f"{{{CORE}}}mesh/{{{CORE}}}vertices/{{{CORE}}}vertex")
    if vertices:
        return [[float(vertex.get(axis)) for axis in "xyz"] for vertex in vertices]
    points = []
    for component in obj.findall(f"{{{CORE}}}components/{{{CORE}}}component"):
        target = component.get(f"{{{PRODUCTION}}}path")
        if target is None:
            target = model_path
        elif target.startswith("/"):
            target = target.lstrip("/")
        else:
            target = str(PurePosixPath(model_path).parent / target)
        values = [float(value) for value in component.get(
            "transform", "1 0 0 0 1 0 0 0 1 0 0 0").split()]
        points.extend(transform(point, values)
                      for point in object_vertices(package, target, component.get("objectid")))
    return points


def create_fixture(base_package, destination, print_sequence="by layer"):
    """Keep the packaged printer/material definitions and replace the simple layout."""
    ET.register_namespace("", CORE)
    with zipfile.ZipFile(base_package) as package:
        model_path = "3D/3dmodel.model"
        model = ET.fromstring(package.read(model_path))
        resources = model.find(f"{{{CORE}}}resources")
        build = model.find(f"{{{CORE}}}build")
        original_item = build.find(f"{{{CORE}}}item")
        original_id = original_item.get("objectid")
        original_object = next(obj for obj in resources.findall(f"{{{CORE}}}object")
                               if obj.get("id") == original_id)
        points = object_vertices(package, model_path, original_id)
        assert points, "native fixture template has no vertices"
        center_x, center_y = [(min(point[axis] for point in points)
                              + max(point[axis] for point in points)) / 2 for axis in range(2)]
        floor = min(point[2] for point in points)
        center_z = (floor + max(point[2] for point in points)) / 2
        second_id = str(max(int(obj.get("id")) for obj in resources.findall(f"{{{CORE}}}object")) + 1)
        second_object = deepcopy(original_object)
        second_object.set("id", second_id)
        resources.append(second_object)
        for obj in [original_object, second_object]:
            for node_index, node in enumerate(obj.iter()):
                for key in list(node.attrib):
                    if key.endswith("UUID"):
                        node.set(key, str(uuid.uuid5(uuid.NAMESPACE_URL, f"printable-capture-object-{obj.get('id')}-{node_index}-{key}")))
        settings = ET.fromstring(package.read("Metadata/model_settings.config"))
        original_settings = next(obj for obj in settings.findall("object") if obj.get("id") == original_id)
        second_settings = deepcopy(original_settings)
        second_settings.set("id", second_id)
        settings.append(second_settings)
        for slot, obj in enumerate([original_settings, second_settings], start=1):
            metadata(obj, "name", f"Fixture part {slot}")
            metadata(obj, "extruder", slot)
            for part in obj.findall("part"):
                metadata(part, "extruder", slot)
        for item in list(build):
            build.remove(item)
        for node in list(settings):
            if node.tag in {"plate", "assemble"}:
                settings.remove(node)
        config = json.loads(package.read("Metadata/project_settings.config"))
        area = [[float(value) for value in point.split("x")] for point in config["printable_area"]]
        width = max(point[0] for point in area) - min(point[0] for point in area)
        depth = max(point[1] for point in area) - min(point[1] for point in area)
        # Native three-plate placement uses two columns and a one-fifth-bed gap.
        origins = [(0.0, 0.0), (width * 1.2, 0.0), (0.0, -depth * 1.2)]
        expected = []
        for index, (origin_x, origin_y) in enumerate(origins):
            plate = ET.SubElement(settings, "plate")
            metadata(plate, "plater_id", index + 1)
            metadata(plate, "plater_name", f"Fixture plate {index + 1}")
            if index == 1:
                metadata(plate, "plater_name", "Fixture plate 2 - rotated red and blue parts and a native prime tower, with a deliberately long plate name that must remain fully visible when automatic framing fits the capture")
            metadata(plate, "locked", "false")
            metadata(plate, "bed_type", "Textured PEI Plate")
            for slot, object_id in enumerate([original_id, second_id]):
                angle = math.radians([0, 35, 80][index] + slot * 15)
                cosine, sine = math.cos(angle), math.sin(angle)
                z_scale = 2 if slot == 0 else [250, 250, 160][index] / (max(point[2] for point in points) - floor)
                flat = slot == 1 and index == 1
                pitch_cosine, pitch_sine = (0, 1) if flat else (1, 0)
                values = [1.8 * cosine, 1.8 * sine, 0,
                          -sine * pitch_cosine, cosine * pitch_cosine, pitch_sine,
                          sine * pitch_sine * z_scale, -cosine * pitch_sine * z_scale, pitch_cosine * z_scale,
                          0, 0, 0]
                center = [center_x, center_y, center_z]
                values[9] = origin_x + width * (0.27 + slot * 0.28) - sum(values[column * 3] * center[column] for column in range(3))
                values[10] = origin_y + depth * (0.35 + slot * 0.14) - sum(values[column * 3 + 1] * center[column] for column in range(3))
                values[11] = -min(transform(point, values)[2] for point in points)
                item = deepcopy(original_item)
                item.set("objectid", object_id)
                item.set("transform", " ".join(map(str, values)))
                for key in list(item.attrib):
                    if key.endswith("UUID"):
                        item.set(key, str(uuid.uuid5(uuid.NAMESPACE_URL, f"printable-capture-{index}-{slot}")))
                build.append(item)
                instance = ET.SubElement(plate, "model_instance")
                metadata(instance, "object_id", object_id)
                metadata(instance, "instance_id", index)
                metadata(instance, "identify_id", 100 + index * 2 + slot)
                transformed = [transform(point, values) for point in points]
                expected.append({"object": slot + 1, "instance": index + 1, "plate": index + 1,
                                 "bounds_mm": {"min": [min(point[axis] for point in transformed) for axis in range(3)],
                                               "max": [max(point[axis] for point in transformed) for axis in range(3)]},
                                 "color": COLORS[slot]})
        assert len(config["filament_colour"]) >= 2, "fixture template must retain two native material slots"
        # A single-nozzle, two-material template makes each plate need a tower.
        config["filament_colour"] = COLORS
        config["print_sequence"] = print_sequence
        config["enable_prime_tower"] = "1"
        config["prime_tower_width"] = "35"
        tower_x = [width * fraction for fraction in (0.75, 0.73, 0.71)]
        tower_y = [depth * fraction for fraction in (0.64, 0.62, 0.60)]
        config["wipe_tower_x"] = list(map(str, tower_x))
        config["wipe_tower_y"] = list(map(str, tower_y))
        config["wipe_tower_rotation_angle"] = "15"
        replacements = {model_path: ET.tostring(model, encoding="utf-8", xml_declaration=True),
                        "Metadata/model_settings.config": ET.tostring(settings, encoding="utf-8", xml_declaration=True),
                        "Metadata/project_settings.config": json.dumps(config).encode()}
        with zipfile.ZipFile(destination, "w", compression=zipfile.ZIP_DEFLATED) as output:
            for name in package.namelist():
                if name.endswith(".gcode") or name == "Metadata/slice_info.config":
                    continue
                data = replacements.get(name, package.read(name))
                if name.endswith(".rels"):
                    relationships = ET.fromstring(data)
                    for relationship in list(relationships):
                        if relationship.get("Target", "").endswith(".gcode"):
                            relationships.remove(relationship)
                    if relationships.tag.startswith("{"):
                        ET.register_namespace("", relationships.tag[1:].split("}")[0])
                    data = ET.tostring(relationships, encoding="utf-8", xml_declaration=True)
                output.writestr(name, data)
    return {"objects": expected, "tower_positions_mm": [
        [origin[0] + x, origin[1] + y] for origin, x, y in zip(origins, tower_x, tower_y)],
        "tower_heights_mm": [max(obj["bounds_mm"]["max"][2] - obj["bounds_mm"]["min"][2]
                                 for obj in expected if obj["plate"] == plate) for plate in (1, 2, 3)],
        "plates": [1,2,3]}


def verify_camera_replay(actual, expected):
    """Allow floating-point roundoff when native matrices resolve the camera."""
    assert actual.keys() == expected.keys()
    for key in ("preset", "projection"):
        assert actual[key] == expected[key], "resolved native camera did not replay"
    for key in ("margin", "zoom"):
        assert math.isclose(actual[key], expected[key], rel_tol=1e-12, abs_tol=1e-9), "resolved native camera did not replay"
    for key in ("orientation", "target"):
        assert len(actual[key]) == len(expected[key])
        assert all(math.isclose(a, b, rel_tol=1e-12, abs_tol=1e-9)
                   for a, b in zip(actual[key], expected[key])), "resolved native camera did not replay"


def verify_bed_surface(workspace, response, points_mm):
    """Require filled native bed regions rather than grid lines over the background."""
    from pathlib import Path
    from PIL import Image

    workspace = Path(workspace).resolve()
    image_path = (workspace / response["image"]["path"]).resolve(strict=True)
    metadata_path = (workspace / response["native_metadata"]["path"]).resolve(strict=True)
    assert image_path.is_relative_to(workspace) and metadata_path.is_relative_to(workspace)
    native = json.loads(metadata_path.read_bytes())
    with Image.open(image_path) as decoded:
        image = decoded.convert("RGB")
    background = image.getpixel((0, 0))
    for point in points_mm:
        vector = [*point, 1]
        view = [sum(row[i] * vector[i] for i in range(4)) for row in native["view_matrix"]]
        clip = [sum(row[i] * view[i] for i in range(4)) for row in native["projection_matrix"]]
        assert clip[3] > 0
        x = round((clip[0] / clip[3] + 1) * image.width / 2)
        y = round((1 - clip[1] / clip[3]) * image.height / 2)
        assert 8 <= x < image.width - 8 and 8 <= y < image.height - 8
        pixels = list(image.crop((x - 8, y - 8, x + 9, y + 9)).getdata())
        filled = sum(max(abs(a - b) for a, b in zip(pixel, background)) > 5 for pixel in pixels)
        assert filled >= len(pixels) * 0.9, "native current-plate bed surface is missing"


def verify_capture(workspace, response, fixture, plates=None):
    """Check geometry identity and actual colored pixels in a native observation."""
    import hashlib
    import io
    import itertools
    from pathlib import Path
    from PIL import Image

    workspace = Path(workspace).resolve()

    def read(path):
        resolved = (workspace / path).resolve(strict=True)
        assert resolved.is_relative_to(workspace), "capture artifact escapes the fixture workspace"
        return resolved.read_bytes()

    selected = fixture["plates"] if plates is None else plates
    assert response["kind"] == "native_orca_layout"
    assert response["representation"] == "model_placement"
    assert response["prime_towers"] == "native_estimated"
    assert sorted(response["plates"]) == sorted(selected)
    pixels = read(response["image"]["path"])
    assert hashlib.sha256(pixels).hexdigest() == response["image_sha256"]
    native_bytes = read(response["native_metadata"]["path"])
    assert hashlib.sha256(native_bytes).hexdigest() == response["native_metadata"]["sha256"]
    native = json.loads(native_bytes)
    with Image.open(io.BytesIO(pixels)) as decoded:
        decoded.load()
        assert decoded.format == "PNG"
        assert decoded.size == (response["width"], response["height"])
        image = decoded.convert("RGB")
    background = image.getpixel((0, 0))
    for edge in [(0, 0, 1, image.height), (image.width - 1, 0, image.width, image.height),
                 (0, 0, image.width, 1), (0, image.height - 1, image.width, image.height)]:
        assert all(pixel == background for pixel in image.crop(edge).getdata()), "automatic framing clips native geometry or plate identifiers at the image edge"

    def projected(point):
        vector = [*point, 1]
        view = [sum(row[i] * vector[i] for i in range(4)) for row in native["view_matrix"]]
        clip = [sum(row[i] * view[i] for i in range(4)) for row in native["projection_matrix"]]
        assert clip[3] > 0, "captured content is behind the camera"
        return [coordinate / clip[3] for coordinate in clip[:3]]

    def corners(bounds):
        return list(itertools.product(*zip(bounds["min"], bounds["max"])))

    for point in corners(native["bounds_mm"]):
        assert all(-1.00001 <= value <= 1.00001 for value in projected(point)), "automatic framing clips selected content"

    def colored_pixels(bounds, color=None):
        points = [projected(point) for point in corners(bounds)]
        xs = [(point[0] + 1) * image.width / 2 for point in points]
        ys = [(1 - point[1]) * image.height / 2 for point in points]
        box = (max(0, int(min(xs)) - 2), max(0, int(min(ys)) - 2),
               min(image.width, math.ceil(max(xs)) + 2), min(image.height, math.ceil(max(ys)) + 2))
        assert box[2] > box[0] and box[3] > box[1], "native part is outside the image"
        count = 0
        for red, green, blue in image.crop(box).getdata():
            saturated = max(red, green, blue) - min(red, green, blue) > 50
            red_part = red > 1.3 * green and red > 1.2 * blue and red > 40
            blue_part = blue > 1.4 * red and blue > 1.15 * green and blue > 40
            if saturated and ((color == COLORS[0] and red_part) or (color == COLORS[1] and blue_part)
                              or (color is None and (red_part or blue_part))):
                count += 1
        assert count >= 5, "native model/tower metadata has no corresponding colored image pixels"

    models = {(volume["object"], volume["instance"]): volume for volume in native["volumes"]
              if not volume["prime_tower"]}
    expected_objects = [obj for obj in fixture["objects"] if obj["plate"] in selected
                        and (not response["objects"] or obj["object"] in response["objects"])]
    assert len(models) == len(expected_objects), "native capture changed model visibility or assignments"
    for expected in expected_objects:
        actual = models[(expected["object"], expected["instance"])]
        for side in ["min", "max"]:
            assert all(abs(left - right) < 0.005 for left, right in zip(
                actual["bounds_mm"][side], expected["bounds_mm"][side])), "native capture changed a source transform"
        rgb = [int(expected["color"][start:start+2], 16) / 255 for start in (1,3,5)]
        assert all(abs(left - right) < 0.005 for left, right in zip(actual["material_color"], rgb)), "native capture changed material colors"
        colored_pixels(expected["bounds_mm"], expected["color"])
    towers = [volume for volume in native["volumes"] if volume["prime_tower"]]
    assert len(towers) == (len(selected) if response["visibility"]["prime_towers"] else 0), "native capture changed prime tower visibility"
    for tower in towers:
        plate = tower["object"] - 1000
        assert plate in selected
        actual = [tower["world_matrix"][axis][3] for axis in range(2)]
        expected = fixture["tower_positions_mm"][plate - 1]
        assert all(abs(left - right) < 0.005 for left, right in zip(actual, expected)), "native prime tower position differs from source"
        height = tower["bounds_mm"]["max"][2] - tower["bounds_mm"]["min"][2]
        assert abs(height - fixture["tower_heights_mm"][plate - 1]) < 0.005, "native prime tower height includes instances on another plate"
        colored_pixels(tower["bounds_mm"])
    return native
