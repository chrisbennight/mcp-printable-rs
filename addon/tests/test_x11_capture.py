from types import SimpleNamespace as NS
import unittest
from printable_bridge.x11_capture import DisplayCaptureError, editor_rectangle, rgb_pixels

class DisplayCaptureTests(unittest.TestCase):
    def test_editor_crop_uses_bottom_left_blender_coordinates(self):
        window = NS(x=50, y=75, width=200, height=100)
        area = NS(x=10, y=20, width=150, height=70)
        self.assertEqual(editor_rectangle(window, area, 500, 400), (60, 235, 150, 70))
        window.y = 350
        with self.assertRaises(DisplayCaptureError):
            editor_rectangle(window, area, 500, 400)
        window.y = 75
        area.width = 201
        with self.assertRaises(DisplayCaptureError):
            editor_rectangle(window, area, 500, 400)

    def test_bgr_storage_and_row_padding_preserve_exact_rgb_pixels(self):
        raw = bytes([3, 2, 1, 0, 6, 5, 4, 0, 99, 99, 99, 99,
                     9, 8, 7, 0, 12, 11, 10, 0, 99, 99, 99, 99])
        self.assertEqual(rgb_pixels(raw, 2, 2, 12), bytes(range(1, 13)))
        with self.assertRaises(DisplayCaptureError):
            rgb_pixels(raw[:-1], 2, 2, 12)
        with self.assertRaises(DisplayCaptureError):
            rgb_pixels(raw, 4, 2, 12)

if __name__ == "__main__":
    unittest.main()
