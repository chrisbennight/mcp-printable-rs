from __future__ import annotations

import unittest
from unittest.mock import patch

import integration_smoke


class UiSmokeTests(unittest.TestCase):
    def probe(self, **overrides: object) -> dict:
        return {"result": {
            "background": False, "vendor": "NVIDIA Corporation",
            "renderer": "NVIDIA test device", "display": ":99",
            "width": 64, "height": 64, "pixel_count": 4096,
            "unique_rgb": 32, "max_rgb": 255, **overrides,
        }}

    def test_gpu_validation_repeats_after_restore(self) -> None:
        with patch.object(integration_smoke, "wait_until_ready"), patch.object(
            integration_smoke, "command",
            side_effect=[self.probe(), {}, {}, self.probe()],
        ) as command, patch("builtins.print"):
            integration_smoke.ui_smoke("unused", 9876, require_nvidia=True)
        self.assertEqual([call.args[2] for call in command.call_args_list], [
            "execute_code", "save_blend", "restore_checkpoint", "execute_code"
        ])

    def test_gpu_probe_rejects_software_background_and_external_display(self) -> None:
        for overrides in (
            {"vendor": "Mesa"}, {"background": True},
            {"display": "remote:0"}, {"width": 0},
            {"pixel_count": 0}, {"unique_rgb": 1}, {"max_rgb": 0},
        ):
            with self.subTest(overrides=overrides), patch.object(
                integration_smoke, "wait_until_ready"
            ), patch.object(
                integration_smoke, "command", return_value=self.probe(**overrides)
            ), self.assertRaises(RuntimeError):
                integration_smoke.ui_smoke("unused", 9876, require_nvidia=True)


if __name__ == "__main__":
    unittest.main()
