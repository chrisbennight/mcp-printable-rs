"""Native bundles publish only verified source sets and preserve original files."""

import hashlib
import json
from pathlib import Path
import tempfile
from types import SimpleNamespace as NS
import unittest
from unittest.mock import patch
import zipfile

from printable_bridge.project_bundle import export_blender_bundle
from printable_bridge.handlers import BlenderHandlers
from printable_bridge.state import SceneState
from printable_bridge.watchdog import NoopExecutionWatchdog
from printable_bridge.workspace import SecureWorkspace, WorkspaceError


class BundleTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.project = self.root / "projects/organic"
        self.project.mkdir(parents=True)
        (self.project / "model.blend").write_bytes(b"original source")
        (self.project / "settings.json").write_bytes(b'{"quality": "final"}')
        self.workspace = SecureWorkspace(self.root, self.root / "runtime")
        self.addCleanup(self.workspace.close)

    def prepare(self, _binary, _original, staged, _files, entrypoint, _timeout, _cancelled):
        (staged / entrypoint).write_bytes(b"packed editable project")
        return {"engine": {"name": "blender", "version": "fixture"},
                "entrypoint": entrypoint, "prepared_libraries": 0,
                "registered_external_files": 0,
                "units": {"system": "METRIC", "length_unit": "MILLIMETERS", "scale_length": 0.001},
                "limitations": ["Scripts are not inspected."]}

    def export(self):
        return export_blender_bundle(self.workspace, self.root, "/opt/blender/blender",
                                     project_id="organic", files=["model.blend", "settings.json"],
                                     entrypoint="model.blend", output_path="exports/project.zip",
                                     timeout_seconds=30)

    def test_original_and_prepared_files_metadata_and_hashes_survive(self):
        with patch("printable_bridge.project_bundle.prepare_in_child", side_effect=self.prepare):
            result = self.export()
        target = self.root / result["path"]
        self.assertEqual(hashlib.sha256(target.read_bytes()).hexdigest(), result["sha256"])
        self.assertEqual((self.project / "model.blend").read_bytes(), b"original source")
        with zipfile.ZipFile(target) as archive:
            self.assertEqual(archive.read("sources/model.blend"), b"original source")
            self.assertEqual(archive.read("prepared/model.blend"), b"packed editable project")
            self.assertEqual(json.loads(archive.read("manifest.json")), result["manifest"])
            for record in result["manifest"]["files"]:
                data = archive.read(record["path"])
                self.assertEqual(len(data), record["size_bytes"])
                self.assertEqual(hashlib.sha256(data).hexdigest(), record["sha256"])
        self.assertEqual(result["manifest"]["preparation"]["units"]["scale_length"], 0.001)

    def test_changed_source_or_destination_race_never_publishes_over_existing_data(self):
        target = self.project / "exports/project.zip"
        for source_changed in (True, False):
            def prepare(*arguments):
                result = self.prepare(*arguments)
                if source_changed:
                    (self.project / "model.blend").write_bytes(b"changed original")
                else:
                    target.write_bytes(b"another writer")
                return result
            with self.subTest(source_changed=source_changed), \
                    patch("printable_bridge.project_bundle.prepare_in_child", side_effect=prepare), \
                    self.assertRaises(WorkspaceError):
                self.export()
            if source_changed:
                self.assertFalse(target.exists())
            else:
                self.assertEqual(target.read_bytes(), b"another writer")

    def test_byte_budget_failure_leaves_no_bundle(self):
        with patch("printable_bridge.project_bundle.MAX_ARTIFACT_BYTES", 1024 * 1024 + 2), \
                patch("printable_bridge.project_bundle.prepare_in_child") as prepare, \
                self.assertRaisesRegex(WorkspaceError, "artifact limit"):
            self.export()
        prepare.assert_not_called()
        self.assertFalse((self.project / "exports/project.zip").exists())

    def test_handler_exports_saved_files_without_rebinding_or_changing_the_live_scene(self):
        handler = BlenderHandlers.__new__(BlenderHandlers)
        handler._config = NS(workspace_root=self.root)
        handler._bpy = NS(app=NS(binary_path="/opt/blender/blender"))
        handler._workspace = self.workspace
        handler._shutdown_requested = lambda: False
        handler._execution_watchdog = NoopExecutionWatchdog()
        handler._scene_state = SceneState()
        handler._scene_state.bind_project("another-live-project")
        handler._state_callbacks = []
        handler._registry = {"export_project_blender": handler._export_project_blender}
        before = handler.scene_state
        with patch("printable_bridge.project_bundle.prepare_in_child", side_effect=self.prepare):
            result = handler.dispatch("export_project_blender", {
                "project_id":"organic", "files":["model.blend", "settings.json"],
                "entrypoint":"model.blend", "output_path":"exports/project.zip", "timeout_seconds":30,
            })
        self.assertEqual(result["manifest"]["scope"], "native_blender")
        self.assertEqual(handler.scene_state, before)
