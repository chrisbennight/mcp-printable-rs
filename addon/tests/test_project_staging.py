"""Input preparation preserves hierarchy and detects changes before publication."""

import hashlib
import os
from pathlib import Path
import tempfile
import unittest

from printable_bridge.project_staging import stage_project_inputs
from printable_bridge.workspace import SecureWorkspace, WorkspaceError


class StagingTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.project = self.root / "projects" / "organic"
        (self.project / "textures").mkdir(parents=True)
        (self.project / "model.blend").write_bytes(b"model fixture")
        (self.project / "textures/leaf.png").write_bytes(b"texture fixture")
        self.workspace = SecureWorkspace(self.root, self.root / "runtime")
        self.addCleanup(self.workspace.close)

    def test_staged_copies_preserve_paths_and_digests_without_changing_sources(self):
        with stage_project_inputs(self.workspace, "organic", ["model.blend", "textures/leaf.png"]) as staged:
            destination = staged.root
            self.assertEqual((destination / "textures/leaf.png").read_bytes(), b"texture fixture")
            record = next(entry for entry in staged.files if entry["path"] == "model.blend")
            self.assertEqual(record["sha256"], hashlib.sha256(b"model fixture").hexdigest())
            (destination / "model.blend").write_bytes(b"prepared copy")
            self.assertEqual((self.project / "model.blend").read_bytes(), b"model fixture")
        self.assertFalse(destination.exists())

    def test_changed_or_replaced_source_is_rejected_after_preparation(self):
        for replacement in [False, True]:
            with self.subTest(replacement=replacement):
                with self.assertRaisesRegex(WorkspaceError, "changed during preparation"):
                    with stage_project_inputs(self.workspace, "organic", ["model.blend"]) as staged:
                        destination = staged.root
                        original = self.project / "model.blend"
                        if replacement:
                            other = self.project / "replacement.blend"
                            other.write_bytes(original.read_bytes())
                            other.replace(original)
                        else:
                            original.write_bytes(b"changed fixture")
                self.assertFalse(destination.exists())

    def test_invalid_missing_symlink_and_cross_project_inputs_are_refused(self):
        (self.project / "link.png").symlink_to(self.project / "textures/leaf.png")
        for selection in [[], ["../other/model.blend"], [".hidden.json"], ["credentials.json"],
                          ["/model.blend"], ["model.blend", "model.blend"],
                          ["model.blend", "missing.png"], ["link.png"]]:
            with self.subTest(selection=selection), self.assertRaises(WorkspaceError):
                with stage_project_inputs(self.workspace, "organic", selection):
                    self.fail("invalid input reached native preparation")

    def test_fifo_replacement_cannot_block_native_preparation(self):
        request = self.workspace.validate("projects/organic/model.blend", ".blend")
        self.workspace.input_identity(request)
        original = self.project / "model.blend"
        original.unlink()
        os.mkfifo(original)
        keeper = os.open(original, os.O_RDWR | os.O_NONBLOCK)
        self.addCleanup(os.close, keeper)
        # Keep a writer connected so a missing O_NONBLOCK cannot hang the test;
        # inspect the opening flags as well as the regular-file rejection.
        from unittest.mock import patch
        with patch("printable_bridge.workspace.os.open", wraps=os.open) as opened:
            with self.assertRaisesRegex(WorkspaceError, "regular file"):
                self.workspace.input_identity(request)
            self.assertTrue(opened.call_args.args[1] & os.O_NONBLOCK)
            with self.assertRaisesRegex(WorkspaceError, "regular file"):
                with self.workspace.stage_input(request):
                    self.fail("FIFO reached native preparation")
            self.assertTrue(opened.call_args.args[1] & os.O_NONBLOCK)
