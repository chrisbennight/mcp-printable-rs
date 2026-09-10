from pathlib import Path
from contextlib import contextmanager, nullcontext
from types import SimpleNamespace as NS
import unittest
from unittest.mock import Mock, patch

from printable_bridge.native_view import NativeViewError, _editor_png, _viewport_png, capture, view_options


class NativeViewTests(unittest.TestCase):
    def test_view_options_are_validated_before_application(self):
        for value in ({"rotation": [0, 0, 0, 0]}, {"distance": float("inf")},
                      {"axis": "FRONT", "rotation": [1, 0, 0, 0]},
                      {"overlays": "yes"}, {"shading": "unknown"}, {"location": [1, 2]}):
            with self.subTest(value=value), self.assertRaises(NativeViewError):
                view_options(value)
        source = {"rotation": [2, 0, 0, 0]}
        self.assertEqual(view_options(source)["rotation"], [1, 0, 0, 0])
        self.assertEqual(source["rotation"], [2, 0, 0, 0])

    def test_invalid_capture_target_does_not_change_persistent_view(self):
        for method, region_type, width in [("viewport", "HEADER", 800), ("editor", "WINDOW", 8192)]:
            context = NS(window=object(), area=NS(type="VIEW_3D", width=width, height=600),
                         region=NS(type=region_type, width=800, height=600), view_layer=Mock())
            bpy = NS(app=NS(background=False), context=context)
            with self.subTest(method=method), patch("printable_bridge.native_view.execution_context", return_value=nullcontext()), patch("printable_bridge.native_view.apply_view") as apply:
                with self.assertRaises(NativeViewError):
                    capture(bpy, Mock(), Path("unused.png"), {"method": method, "view": {"distance": 5}})
                apply.assert_not_called()
                context.view_layer.update.assert_not_called()

    def test_failed_gpu_draw_releases_offscreen_without_editor_fallback(self):
        offscreen = Mock()
        offscreen.draw_view3d.side_effect = RuntimeError("GPU draw failed")
        gpu = NS(types=NS(GPUOffScreen=Mock(return_value=offscreen)))
        space = NS(region_3d=NS(update=Mock(), view_matrix=object(), window_matrix=object()))
        bpy = NS(context=NS(area=NS(type="VIEW_3D"), region=NS(type="WINDOW", width=800, height=600),
                            space_data=space, scene=object(), view_layer=object()), data=NS(images=Mock()))
        with self.assertRaisesRegex(RuntimeError, "GPU draw failed"):
            _viewport_png(bpy, gpu, Path("image.png"), 400)
        gpu.types.GPUOffScreen.assert_called_once_with(400, 300)
        offscreen.free.assert_called_once_with()
        bpy.data.images.new.assert_not_called()

    def test_editor_capture_redraws_first_and_reports_resampling(self):
        events = []
        image = NS(pixels=NS(foreach_set=Mock()), scale=Mock(), save=Mock())
        images = NS(new=Mock(return_value=image), remove=Mock())
        area = NS(width=1600, height=1200)
        context = NS(area=area, window=object(), region=object())
        @contextmanager
        def override(**target):
            context.area = target["area"]
            try:
                yield
            finally:
                context.area = None
        context.temp_override = override
        def redraw(**params):
            events.append(("redraw", params))
            context.area = None
            return {"FINISHED"}
        def screenshot(window, selected_area):
            self.assertIs(context.area, area)
            self.assertIs(selected_area, area)
            self.assertIs(window, context.window)
            events.append(("capture", {}))
            return bytes([16, 32, 48]) * 1600 * 1200, 1600, 1200
        bpy = NS(context=context, data=NS(images=images), ops=NS(wm=NS(redraw_timer=redraw)))
        with patch("printable_bridge.native_view.capture_rgb", side_effect=screenshot):
            self.assertEqual(_editor_png(bpy, Path("editor.png"), 640), (640, 480, [1600, 1200]))
        self.assertEqual([event[0] for event in events], ["redraw", "capture"])
        self.assertEqual(events[0][1], {"type": "DRAW_WIN_SWAP", "iterations": 1})
        image.scale.assert_called_once_with(640, 480)
        image.save.assert_called_once_with()
        images.remove.assert_called_once_with(image)

    def test_oversized_editor_is_rejected_before_redraw_or_image_allocation(self):
        bpy = NS(context=NS(area=NS(width=8192, height=8192), window=object(), region=object()), ops=Mock(), data=Mock())
        with self.assertRaises(NativeViewError):
            _editor_png(bpy, Path("editor.png"), 1024)
        bpy.ops.wm.redraw_timer.assert_not_called()
        bpy.data.images.load.assert_not_called()


if __name__ == "__main__":
    unittest.main()
