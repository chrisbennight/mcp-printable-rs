from contextlib import contextmanager
from types import SimpleNamespace as NS
import unittest

from printable_bridge.editor_context import (
    EditorContextError, editing_state, execution_context, resolve_editor,
)


class EditorContextTests(unittest.TestCase):
    def setUp(self):
        self.areas = [NS(type=kind, ui_type=kind, width=800, height=600,
                         regions=[NS(type="WINDOW")])
                      for kind in ("VIEW_3D", "NODE_EDITOR", "VIEW_3D")]
        context = NS(
            window_manager=NS(windows=[NS(screen=NS(areas=self.areas))]),
            view_layer=NS(name="Layer", objects=NS(active=NS(name="Part"))),
            scene=NS(name="Scene", frame_current=7), mode="OBJECT",
            area=None, region=None, selected_objects=[NS(name=f"Part{i}") for i in range(5)],
        )
        @contextmanager
        def override(**values):
            before = {key: getattr(context, key, None) for key in values}
            for key, value in values.items():
                setattr(context, key, value)
            try:
                yield
            finally:
                for key, value in before.items():
                    setattr(context, key, value)
        context.temp_override = override
        self.bpy = NS(context=context)

    def test_editor_page_returns_reusable_type_relative_selectors(self):
        observed = editing_state(self.bpy, {"offset": 1, "limit": 1})
        self.assertEqual(observed["total"], 3)
        self.assertEqual(observed["next_offset"], 2)
        self.assertEqual(observed["frame"], 7)
        self.assertEqual(observed["active_object"], "Part")
        selected = resolve_editor(self.bpy, {"area_type": "VIEW_3D", "area_index": 1})
        self.assertIs(selected["area"], self.areas[2])

    def test_selection_is_bounded_and_paginated(self):
        observed = editing_state(self.bpy, {"section": "selection", "offset": 2, "limit": 2})
        self.assertEqual(observed["items"], ["Part2", "Part3"])
        self.assertEqual(observed["next_offset"], 4)

    def test_context_restores_after_failure(self):
        with self.assertRaisesRegex(ValueError, "source failed"):
            with execution_context(self.bpy, {"area_type": "NODE_EDITOR"}):
                self.assertIs(self.bpy.context.area, self.areas[1])
                raise ValueError("source failed")
        self.assertIsNone(self.bpy.context.area)

    def test_missing_editor_and_mode_mismatch_never_enter_source(self):
        for selector in ({"area_type": "CONSOLE"},
                         {"area_type": "VIEW_3D", "expected_mode": "EDIT_MESH"}):
            with self.subTest(selector=selector), self.assertRaises(EditorContextError):
                with execution_context(self.bpy, selector):
                    self.fail("invalid context entered source")

    def test_wrong_selector_shapes_are_rejected(self):
        for selector in ([], {}, {"area_type": "VIEW_3D", "window": True},
                         {"area_type": "VIEW_3D", "window": 64},
                         {"area_type": "VIEW_3D", "active_object": "Part"}):
            with self.subTest(selector=selector), self.assertRaises(EditorContextError):
                resolve_editor(self.bpy, selector)


if __name__ == "__main__":
    unittest.main()
