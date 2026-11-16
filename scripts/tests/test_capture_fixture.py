"""Small material-change fixtures preserve authored slots and bounded dimensions."""

import hashlib
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
import xml.etree.ElementTree as ET
import zipfile


source = Path(__file__).resolve().parents[2] / "slicer/capture_fixture.py"
spec = importlib.util.spec_from_file_location("printable_capture_fixture", source)
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class MaterialFixtureTests(unittest.TestCase):
    def template(self, path):
        vertices = [(0, 0, 0), (10, 0, 0), (0, 10, 0), (0, 0, 3)]
        model = ET.Element(f"{{{fixture.CORE}}}model")
        resources = ET.SubElement(model, f"{{{fixture.CORE}}}resources")
        obj = ET.SubElement(resources, f"{{{fixture.CORE}}}object", {"id": "1"})
        mesh = ET.SubElement(obj, f"{{{fixture.CORE}}}mesh")
        points = ET.SubElement(mesh, f"{{{fixture.CORE}}}vertices")
        for point in vertices:
            ET.SubElement(points, f"{{{fixture.CORE}}}vertex",
                          dict(zip("xyz", map(str, point))))
        triangles = ET.SubElement(mesh, f"{{{fixture.CORE}}}triangles")
        ET.SubElement(triangles, f"{{{fixture.CORE}}}triangle", {"v1": "0", "v2": "1", "v3": "2"})
        build = ET.SubElement(model, f"{{{fixture.CORE}}}build")
        ET.SubElement(build, f"{{{fixture.CORE}}}item", {"objectid": "1"})
        settings = b'<config><object id="1"><part id="1" subtype="normal_part"/></object></config>'
        config = {"printable_area": ["0x0", "256x0", "256x256", "0x256"],
                  "filament_colour": fixture.COLORS, "filament_settings_id": ["material one", "material two"]}
        with zipfile.ZipFile(path, "w") as package:
            package.writestr("3D/3dmodel.model", ET.tostring(model))
            package.writestr("Metadata/model_settings.config", settings)
            package.writestr("Metadata/project_settings.config", json.dumps(config))
            package.writestr("Metadata/authored-mask.bin", b"unchanged authored data")

    def test_small_two_material_multiplate_fixture_keeps_slots_and_source(self):
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / "template.3mf"
            destination = Path(directory) / "small.3mf"
            self.template(source)
            before = hashlib.sha256(source.read_bytes()).hexdigest()
            expected = fixture.create_fixture(source, destination, part_heights_mm=(2, 4), plate_count=2)
            self.assertEqual(expected["plates"], [1, 2])
            self.assertEqual(len(expected["objects"]), 4)
            self.assertEqual(expected["tower_heights_mm"], [4, 4])
            with zipfile.ZipFile(destination) as package:
                config = json.loads(package.read("Metadata/project_settings.config"))
                self.assertEqual(config["filament_settings_id"], ["material one", "material two"])
                self.assertEqual(len(config["wipe_tower_x"]), 2)
                self.assertEqual(package.read("Metadata/authored-mask.bin"), b"unchanged authored data")
                model = ET.fromstring(package.read("3D/3dmodel.model"))
                self.assertEqual(len(model.findall(f"{{{fixture.CORE}}}build/{{{fixture.CORE}}}item")), 4)
                settings = ET.fromstring(package.read("Metadata/model_settings.config"))
                self.assertEqual([obj.find("metadata[@key='extruder']").get("value")
                                  for obj in settings.findall("object")], ["1", "2"])
            self.assertEqual(hashlib.sha256(source.read_bytes()).hexdigest(), before)

    def test_invalid_fixture_dimensions_fail_before_opening_an_input(self):
        for options in ({"part_heights_mm": (0, 4)}, {"part_heights_mm": (2, float("nan"))},
                        {"part_heights_mm": (2,)}, {"plate_count": 4}):
            with self.subTest(options=options), self.assertRaises(ValueError):
                fixture.create_fixture("not-opened.3mf", "not-created.3mf", **options)

    def test_banded_assembly_keeps_elevated_material_bands_and_slot_identity(self):
        with tempfile.TemporaryDirectory() as directory:
            source=Path(directory)/"template.3mf";destination=Path(directory)/"banded.3mf"
            self.template(source)
            before=hashlib.sha256(source.read_bytes()).hexdigest()
            report=fixture.create_banded_fixture(source,destination)
            self.assertEqual(report["material_height_ranges_mm"],[[0,2,1],[2,10,2],[10,12,1]])
            with zipfile.ZipFile(destination) as package:
                model=ET.fromstring(package.read("3D/3dmodel.model"))
                objects=model.findall(f"{{{fixture.CORE}}}resources/{{{fixture.CORE}}}object")
                self.assertEqual(len(objects),4)
                self.assertEqual(len(objects[-1].findall(f"{{{fixture.CORE}}}components/{{{fixture.CORE}}}component")),3)
                for obj,(low,high,_) in zip(objects[:3],report["material_height_ranges_mm"]):
                    z=[float(vertex.get("z")) for vertex in obj.findall(f".//{{{fixture.CORE}}}vertex")]
                    self.assertEqual((min(z),max(z)),(low,high))
                    self.assertEqual(len(obj.findall(f".//{{{fixture.CORE}}}triangle")),12)
                settings=ET.fromstring(package.read("Metadata/model_settings.config"))
                self.assertEqual([part.find("metadata[@key='extruder']").get("value")
                                  for part in settings.findall("object/part")],["1","2","1"])
                self.assertEqual(package.read("Metadata/authored-mask.bin"),b"unchanged authored data")
            self.assertEqual(hashlib.sha256(source.read_bytes()).hexdigest(),before)
