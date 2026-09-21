"""Dependency metadata contracts without Blender or external file access."""

import json
from pathlib import Path
from types import SimpleNamespace as NS
import unittest

from printable_bridge.handlers import BlenderHandlers, HandlerError
from printable_bridge.inspection import InspectionError
from printable_bridge.project_dependencies import inspect_dependencies
from printable_bridge.state import SceneState


class DependencyTests(unittest.TestCase):
    def blender(self, paths):
        def blend_paths(**kwargs):
            self.assertEqual(kwargs, {"absolute": True, "packed": False, "local": False})
            return paths
        return NS(utils=NS(blend_paths=blend_paths), app=NS(version_string="4.5.0"),
                  context=NS(scene=NS(unit_settings=NS(system="METRIC", length_unit="MILLIMETERS", scale_length=0.001))))

    def test_project_paths_external_redaction_and_pagination(self):
        bpy = self.blender(["/workspace/projects/organic/textures/leaf.png",
                            "/private/assets/library.blend",
                            "/workspace/projects/organic/../other/source.blend",
                            "/workspace/projects/organic/.hidden/state.json"])
        first = inspect_dependencies(bpy, Path("/workspace"), "organic", {"limit": 2})
        self.assertEqual(first["items"], [
            {"index": 0, "project_path": "textures/leaf.png", "state": "requires_snapshot"},
            {"index": 1, "project_path": None, "state": "external"}])
        self.assertEqual(first["next_offset"], 2)
        self.assertEqual(first["units"]["scale_length"], 0.001)
        self.assertEqual(first["engine"], {"name": "blender", "version": "4.5.0"})
        self.assertNotIn("/private", json.dumps(first))
        second = inspect_dependencies(bpy, Path("/workspace"), "organic", {"offset": 2})
        self.assertIsNone(second["next_offset"])
        self.assertTrue(all(item["state"] == "external" for item in second["items"]))

    def test_limits_and_registered_path_scope_are_explicit(self):
        result = inspect_dependencies(self.blender([]), Path("/workspace"), "organic", {})
        self.assertEqual(result["total"], 0)
        self.assertEqual(result["scope"], "blender_registered_external_files")
        self.assertTrue(result["limitations"])
        for params in [{"limit": 0}, {"offset": -1}, {"limit": True}]:
            with self.assertRaises(InspectionError):
                inspect_dependencies(self.blender([]), Path("/workspace"), "organic", params)
        with self.assertRaises(InspectionError):
            inspect_dependencies(self.blender(["/file"] * 10001), Path("/workspace"), "organic", {})

    def test_handler_requires_bound_project_and_does_not_mutate_scene_revision(self):
        handler = BlenderHandlers.__new__(BlenderHandlers)
        handler._bpy = self.blender([])
        handler._config = NS(workspace_root=Path("/workspace"))
        handler._scene_state = SceneState()
        handler._scene_state.bind_project("organic")
        handler._state_callbacks = []
        handler._registry = {"get_project_dependencies": handler._get_project_dependencies}
        before = handler.scene_state
        result = handler.dispatch("get_project_dependencies", {"project_id": "organic", "expected_scene": before})
        self.assertEqual(result["total"], 0)
        self.assertEqual(handler.scene_state, before)
        with self.assertRaises(HandlerError):
            handler.dispatch("get_project_dependencies", {"project_id": "other", "expected_scene": before})
        self.assertEqual(handler.scene_state, before)


if __name__ == "__main__":
    unittest.main()
