"""Disk-backed parameter lifecycle, using a tiny network rather than model weights."""

from pathlib import Path
import tempfile
import unittest

import torch
from safetensors.torch import save_file

from printable_recon.trellis_memory import stream_parameters


class Network(torch.nn.Module):
    def __init__(self):
        super().__init__()
        self.input = torch.nn.Linear(8, 8)
        self.blocks = torch.nn.ModuleList([
            torch.nn.Sequential(torch.nn.Linear(8, 16), torch.nn.ReLU(), torch.nn.Linear(16, 8))
            for _ in range(2)
        ])
        self.output = torch.nn.Linear(8, 8)

    def forward(self, x):
        x = self.input(x)
        for block in self.blocks:
            x = block(x)
        return self.output(x)


class ParameterTests(unittest.TestCase):
    def test_inactive_parameters_have_no_cpu_backup(self):
        with tempfile.TemporaryDirectory() as directory:
            model = Network().eval()
            path = Path(directory) / "weights.safetensors"
            save_file(model.state_dict(), path)
            stream_parameters(model, path, blocks=True, evict=False)
            self.assertTrue(all(parameter.numel() == 0 for parameter in model.parameters()))
            self.assertTrue(all(parameter.device.type == "cpu" for parameter in model.parameters()))
            model.cpu()
            self.assertTrue(all(parameter.numel() == 0 for parameter in model.parameters()))

    def test_missing_parameters_fail_before_releasing_network_state(self):
        with tempfile.TemporaryDirectory() as directory:
            model = Network().eval()
            path = Path(directory) / "weights.safetensors"
            state = model.state_dict()
            state.pop("output.bias")
            save_file(state, path)
            with self.assertRaisesRegex(ValueError, "does not match"):
                stream_parameters(model, path, blocks=True)
            self.assertTrue(all(parameter.numel() > 0 for parameter in model.parameters()))

    @unittest.skipUnless(torch.cuda.is_available(), "CUDA is required to qualify disk-to-GPU parameter streaming")
    def test_streaming_preserves_results_and_releases_weights_after_use(self):
        with tempfile.TemporaryDirectory() as directory, torch.inference_mode():
            torch.manual_seed(42)
            model = Network().eval()
            path = Path(directory) / "weights.safetensors"
            save_file(model.state_dict(), path)
            x = torch.randn(31, 8, device="cuda")
            expected = model.cuda()(x).clone()
            model.cpu()
            stats = stream_parameters(model, path, blocks=True)
            model.cuda()
            self.assertTrue(torch.equal(model(x), expected))
            self.assertEqual(stats["block_loads"], len(model.blocks))
            self.assertTrue(all(parameter.numel() == 0 for block in model.blocks for parameter in block.parameters()))
            model.cpu()
            self.assertTrue(all(parameter.numel() == 0 for parameter in model.parameters()))


if __name__ == "__main__":
    unittest.main()
