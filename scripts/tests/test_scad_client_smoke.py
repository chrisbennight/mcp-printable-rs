"""The native regression must reject success metadata for the wrong geometry."""

import unittest

from scad_client_smoke import run


class DefaultGeometryClient:
    def call(self, tool, request):
        return {"definitions": {"variant_applied": True},
                "validation": {"bounds": {"dimensions_mm": [164, 122, 98.6]}}}


class ScadVariantSmokeTests(unittest.TestCase):
    def test_success_metadata_does_not_hide_default_geometry(self):
        with self.assertRaisesRegex(ValueError, "selected generated incorrect variant geometry"):
            run(DefaultGeometryClient())
