import json
from pathlib import Path
import struct
import tempfile
import unittest

from printable_bridge.cad_import import prepare_glb
from project_scene_smoke import write_fixture


class CadImportTests(unittest.TestCase):
    def test_metadata_preserves_geometry_bytes_and_original_parent_names(self):
        with tempfile.TemporaryDirectory() as directory:
            write_fixture(directory)
            output = Path(directory, "projects/smoke-alpha/cad/output")
            source = output / "model.glb"
            data = source.read_bytes()
            size = struct.unpack_from("<I", data, 12)[0]
            with prepare_glb(source, output / "components.json") as (prepared, evidence):
                converted = prepared.read_bytes()
                new_size = struct.unpack_from("<I", converted, 12)[0]
                document = json.loads(converted[20:20 + new_size])
                self.assertEqual(converted[20 + new_size:], data[20 + size:])
                self.assertEqual(document["nodes"][0]["extras"]["printable_cad_product_name"], "Assembly product")
                self.assertEqual(document["nodes"][1]["extras"]["printable_cad_occurrence_name"], "Occurrence")
                self.assertEqual(evidence["unmatched_names"], 0)
            self.assertFalse(prepared.exists())
            self.assertEqual(source.read_bytes(), data)

    def test_external_buffer_is_rejected_before_blender_receives_it(self):
        with tempfile.TemporaryDirectory() as directory:
            write_fixture(directory)
            output = Path(directory, "projects/smoke-alpha/cad/output")
            source = output / "model.glb"
            data = source.read_bytes()
            size = struct.unpack_from("<I", data, 12)[0]
            document = json.loads(data[20:20 + size])
            document["buffers"][0]["uri"] = "../../outside.bin"
            encoded = json.dumps(document).encode()
            encoded += b" " * (-len(encoded) % 4)
            source.write_bytes(struct.pack("<5I", 0x46546C67, 2, len(data) - size + len(encoded),
                                           len(encoded), 0x4E4F534A) + encoded + data[20 + size:])
            with self.assertRaisesRegex(ValueError, "self-contained"):
                with prepare_glb(source, output / "components.json"):
                    self.fail("unsafe GLB reached the importer")
