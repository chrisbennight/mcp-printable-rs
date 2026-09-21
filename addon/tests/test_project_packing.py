"""Packing refusal contracts; successful native behavior has a Blender smoke."""

from pathlib import Path
import tempfile
from types import SimpleNamespace as NS
import unittest

from printable_bridge.project_packing import ProjectPackingError, pack_loaded_file


class PackingTests(unittest.TestCase):
    def blender(self, references, *, remaining=(), background=True):
        calls = []
        inventories = iter([references, remaining])

        def pack():
            calls.append("pack")
            return {"FINISHED"}

        return NS(app=NS(background=background),
                  utils=NS(blend_paths=lambda **_: next(inventories)),
                  data=NS(user_map=lambda: {}),
                  ops=NS(file=NS(pack_all=pack, pack_libraries=pack))), calls

    def test_external_missing_and_symlink_dependencies_are_not_packed(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            project = root / "project"
            project.mkdir()
            outside = root / "outside.png"
            outside.write_bytes(b"fixture")
            link = project / "linked.png"
            link.symlink_to(outside)
            for references in [[str(outside)], [str(project / "missing.png")], [str(link)]]:
                bpy, calls = self.blender(references)
                with self.assertRaises(ProjectPackingError):
                    pack_loaded_file(bpy, project, project / "result.blend", library=False)
                self.assertEqual(calls, [])
                self.assertFalse((project / "result.blend").exists())

    def test_unsupported_remaining_dependency_prevents_project_save(self):
        with tempfile.TemporaryDirectory() as temporary:
            project = Path(temporary)
            dependency = project / "cache.bin"
            dependency.write_bytes(b"fixture")
            bpy, calls = self.blender([str(dependency)], remaining=[str(dependency)])
            with self.assertRaisesRegex(ProjectPackingError, "still has external dependencies"):
                pack_loaded_file(bpy, project, project / "result.blend", library=False)
            self.assertEqual(calls, ["pack", "pack"])
            self.assertFalse((project / "result.blend").exists())

    def test_non_background_and_escaping_output_are_refused_before_packing(self):
        with tempfile.TemporaryDirectory() as temporary:
            project = Path(temporary)
            bpy, calls = self.blender([], background=False)
            with self.assertRaises(ProjectPackingError):
                pack_loaded_file(bpy, project, project / "result.blend", library=False)
            self.assertEqual(calls, [])
            for target in [project.parent / "outside.blend", project / "wrong.json"]:
                bpy, calls = self.blender([])
                with self.assertRaises(ProjectPackingError):
                    pack_loaded_file(bpy, project, target, library=False)
                self.assertEqual(calls, [])
