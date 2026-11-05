"""Foreground framing and real miniature DINOv3 conditioning, without model downloads."""

from pathlib import Path
import tempfile
import unittest

import numpy as np
from PIL import Image
from safetensors.torch import save_file
import torch
from transformers import DINOv3ViTConfig, DINOv3ViTModel

from printable_recon.trellis import _ImageFeatures, frame_subject


class FeatureTests(unittest.TestCase):
    def test_framing_preserves_transparency_input_and_returns_a_square(self):
        pixels = np.zeros((20, 30, 4), dtype=np.uint8)
        pixels[5:15, 10:20] = [200, 100, 50, 255]
        image = Image.fromarray(pixels)
        before = image.tobytes()
        framed = frame_subject(image)
        self.assertEqual(framed.mode, "RGB")
        self.assertEqual(framed.width, framed.height)
        self.assertEqual(image.tobytes(), before)
        self.assertGreater(np.asarray(framed).max(), 0)

    def test_an_empty_foreground_has_an_actionable_error(self):
        with self.assertRaisesRegex(ValueError, "no subject"):
            frame_subject(Image.new("RGBA", (32, 32)))

    def test_shared_dinov3_definitions_produce_repeatable_image_conditioning(self):
        torch.manual_seed(42)
        config = DINOv3ViTConfig(hidden_size=32, intermediate_size=64, num_hidden_layers=2,
                               num_attention_heads=2, num_register_tokens=4, patch_size=16, image_size=32)
        with tempfile.TemporaryDirectory() as directory, torch.inference_mode():
            root = Path(directory)
            config.to_json_file(str(root / "config.json"))
            model = DINOv3ViTModel(config)
            # Published checkpoints omit the installed encoder's model prefix.
            state = {name.removeprefix("model.") if name.startswith("model.layer.") else name: tensor
                     for name, tensor in model.state_dict().items()}
            save_file(state, root / "model.safetensors")
            adapter = _ImageFeatures(root)
            adapter.image_size = 32
            red, blue = Image.new("RGB", (32, 32), "red"), Image.new("RGB", (32, 32), "blue")
            result = adapter([red, red, blue])
            self.assertEqual(result.shape, (3, 9, 32))
            self.assertTrue(torch.isfinite(result).all())
            self.assertTrue(torch.equal(result[0], result[1]))
            self.assertFalse(torch.equal(result[0], result[2]))


if __name__ == "__main__":
    unittest.main()
