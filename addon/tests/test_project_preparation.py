"""Native preparation selects exact staged inputs and orders library packing."""

from pathlib import Path
import tempfile
from types import SimpleNamespace as NS
import unittest
from unittest.mock import patch

from printable_bridge.project_packing import ProjectPackingError
from printable_bridge.project_preparation import ProjectInputs, prepare_staged_project


class PreparationTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)
        self.original = self.root / "original"
        self.staged = self.root / "staged"
        self.staged.mkdir()
        self.files = ["model.blend", "libs/leaf.blend", "textures/leaf.png"]
        for name in self.files:
            destination = self.staged / name
            destination.parent.mkdir(parents=True, exist_ok=True)
            destination.write_bytes(b"fixture")

    def test_absolute_and_staged_references_map_to_exact_selected_files(self):
        inputs = ProjectInputs(self.original, self.staged, self.files)
        for root in (self.original, self.staged):
            self.assertEqual(inputs.path(str(root / "textures/leaf.png")),
                             self.staged / "textures/leaf.png")
            for name in ("other.png", "../other/leaf.png", ".hidden.png"):
                with self.subTest(root=root, name=name), self.assertRaises(ProjectPackingError):
                    inputs.path(str(root / name))
        with self.assertRaises(ProjectPackingError):
            inputs.path("//textures/leaf.png")

    def test_input_aliases_and_symlinked_directories_are_refused(self):
        (self.staged / "alias").symlink_to(self.staged / "textures", target_is_directory=True)
        for files in (["alias/leaf.png"], ["../staged/model.blend"], ["/model.blend"],
                      ["textures//leaf.png"], ["model.blend", "model.blend"]):
            with self.subTest(files=files), self.assertRaises(ProjectPackingError):
                ProjectInputs(self.original, self.staged, files)

    def test_dependency_order_and_cycle_detection_do_not_pack_early(self):
        opened = []
        packed = []
        graph = {"model.blend": ["libs/leaf.blend"], "libs/leaf.blend": []}
        bpy = NS(app=NS(background=True, version_string="fixture"),
                 context=NS(scene=NS(unit_settings=NS(system="METRIC", length_unit="METERS",
                                                     scale_length=1.0))))

        def load(_bpy, path):
            opened.append(path.relative_to(self.staged).as_posix())

        def pack(_bpy, root, path, *, library):
            packed.append((path.relative_to(root).as_posix(), library))

        module = "printable_bridge.project_preparation"
        with patch(f"{module}._open", side_effect=load), \
                patch(f"{module}._library_paths", side_effect=lambda *_: graph[opened[-1]]), \
                patch(f"{module}._rebase_loaded_file"), \
                patch(f"{module}.pack_loaded_file", side_effect=pack):
            result = prepare_staged_project(bpy, self.original, self.staged, self.files, "model.blend")
            self.assertEqual(packed, [("libs/leaf.blend", True), ("model.blend", False)])
            self.assertEqual(result["prepared_libraries"], 1)
            packed.clear()
            graph["libs/leaf.blend"] = ["model.blend"]
            with self.assertRaisesRegex(ProjectPackingError, "cyclic"):
                prepare_staged_project(bpy, self.original, self.staged, self.files, "model.blend")
            self.assertEqual(packed, [])

    def test_live_scene_is_refused_before_loading_any_file(self):
        with self.assertRaisesRegex(ProjectPackingError, "isolated background"):
            prepare_staged_project(NS(app=NS(background=False)), self.original,
                                   self.staged, self.files, "model.blend")
