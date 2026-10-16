"""Unit tests for Marigold V2 depth estimation, without the published model.

A miniature Qwen image transformer and VAE with random weights stand in for
Qwen-Image-Edit-2509, laid out as its download is, with miniature Marigold V2
adapters and prompts beside them. They exercise conversion, NF4 storage,
block streaming and the worker's request handling on the processor.
"""

import json
import math
import os
import pickle
import struct
import subprocess
import sys
import tempfile
import unittest
import zlib
from pathlib import Path

import numpy
import torch
from diffusers import AutoencoderKLQwenImage, QwenImageTransformer2DModel
from PIL import Image
from safetensors.torch import save_file

import convert_marigold
import estimate_depth
from printable_recon import files, marigold, nf4

RECON = Path(__file__).resolve().parent.parent

TRANSFORMER = {
    "attention_head_dim": 32,
    "axes_dims_rope": [8, 12, 12],
    "guidance_embeds": False,
    "in_channels": 64,
    "joint_attention_dim": 32,
    "num_attention_heads": 2,
    "num_layers": 3,
    "out_channels": 16,
    "patch_size": 2,
}
VAE = {
    "attn_scales": [],
    "base_dim": 8,
    "dim_mult": [1, 2, 4, 4],
    "dropout": 0.0,
    "latents_mean": [0.1 * index - 0.8 for index in range(16)],
    "latents_std": [1.0 + 0.05 * index for index in range(16)],
    "num_res_blocks": 1,
    "temperal_downsample": [False, True, True],
    "z_dim": 16,
}
# Layers given adapters in the miniature checkpoints, as Marigold V2 adapts
# both the outer layers and those inside every block.
ADAPTED = ["img_in", "txt_in", "transformer_blocks.0.attn.to_q", "transformer_blocks.2.img_mlp.net.2"]
RANK = 4


class _Malicious:
    """Unpickling creates the directory `marker`, which shows the code ran."""

    def __init__(self, marker: Path):
        self.marker = marker

    def __reduce__(self):
        return (os.mkdir, (str(self.marker),))


def _write_json(path: Path, value: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value), encoding="utf-8")


def make_sources(root: Path, shards: int = 2) -> tuple[Path, Path]:
    """Miniature downloads of Qwen-Image-Edit-2509 and Marigold V2."""
    torch.manual_seed(1)
    qwen = root / "qwen"
    transformer = QwenImageTransformer2DModel.from_config(TRANSFORMER).to(torch.bfloat16)
    state = {name: tensor.contiguous() for name, tensor in transformer.state_dict().items()}
    _write_json(qwen / "transformer" / "config.json", TRANSFORMER)
    names = sorted(state)
    weight_map = {}
    for shard in range(shards):
        file = f"diffusion_pytorch_model-{shard + 1:05d}-of-{shards:05d}.safetensors"
        part = names[shard::shards]
        save_file({name: state[name] for name in part}, str(qwen / "transformer" / file))
        weight_map.update({name: file for name in part})
    _write_json(qwen / "transformer" / "diffusion_pytorch_model.safetensors.index.json", {"weight_map": weight_map})
    vae = AutoencoderKLQwenImage.from_config(VAE).to(torch.bfloat16)
    _write_json(qwen / "vae" / "config.json", VAE)
    save_file({name: tensor.contiguous() for name, tensor in vae.state_dict().items()}, str(qwen / "vae" / "diffusion_pytorch_model.safetensors"))

    marigold_root = root / "marigold"
    shapes = {name: tensor.shape for name, tensor in state.items()}
    decoder = {name: tensor for name, tensor in vae.state_dict().items() if name.startswith("decoder.conv_out")}
    for task, (trainables, prompt) in convert_marigold.TASKS.items():
        checkpoint = {}
        for layer in ADAPTED:
            out_features, in_features = shapes[f"{layer}.weight"]
            checkpoint[f"Diffuser.{layer}.lora_A.default.weight"] = (torch.randn(RANK, in_features) * 0.1).to(torch.bfloat16)
            checkpoint[f"Diffuser.{layer}.lora_B.default.weight"] = (torch.randn(out_features, RANK) * 0.1).to(torch.bfloat16)
        for name, tensor in decoder.items():
            checkpoint[f"VAE.{name}"] = (tensor.float() + 0.01).to(torch.bfloat16)
        checkpoint["iREPAStudentProjector.out.weight"] = torch.zeros(4, 4)
        path = marigold_root / trainables
        path.parent.mkdir(parents=True, exist_ok=True)
        save_file(checkpoint, str(path))
        embeddings = marigold_root / "qwen_text_embeddings"
        embeddings.mkdir(parents=True, exist_ok=True)
        torch.save(torch.randn(2, 7, TRANSFORMER["joint_attention_dim"]).to(torch.bfloat16), embeddings / f"{prompt}_prompt_embeds.pt")
        torch.save(torch.ones(2, 7, dtype=torch.int64), embeddings / f"{prompt}_prompt_mask.pt")
    return qwen, marigold_root


def reference_transformer(qwen: Path, marigold_root: Path, task: str) -> QwenImageTransformer2DModel:
    """The miniature transformer as Marigold V2 runs it, held whole in float32."""
    from safetensors.torch import load_file

    index = json.loads((qwen / "transformer" / "diffusion_pytorch_model.safetensors.index.json").read_text())
    state = {}
    for file in sorted(set(index["weight_map"].values())):
        state.update(load_file(str(qwen / "transformer" / file)))
    for name, tensor in state.items():
        if marigold.quantized(name, tuple(tensor.shape)):
            packed, scales = nf4.quantize(tensor)
            state[name] = nf4.dequantize(packed, scales, tensor.shape, torch.float32)
    checkpoint = load_file(str(marigold_root / convert_marigold.TASKS[task][0]))
    for layer in ADAPTED:
        down = checkpoint[f"Diffuser.{layer}.lora_A.default.weight"].float()
        up = checkpoint[f"Diffuser.{layer}.lora_B.default.weight"].float()
        state[f"{layer}.weight"] = state[f"{layer}.weight"].float() + convert_marigold.LORA_ALPHA / RANK * up @ down
    model = QwenImageTransformer2DModel.from_config(TRANSFORMER)
    model.load_state_dict({name: tensor.float() for name, tensor in state.items()})
    return model.eval()


def read_png16(path: Path) -> numpy.ndarray:
    """The pixels of an unfiltered 16-bit RGB PNG as written by write_png16."""
    data = path.read_bytes()
    position, chunks = 8, {}
    while position < len(data):
        length = struct.unpack(">I", data[position:position + 4])[0]
        kind = data[position + 4:position + 8]
        chunks[kind] = data[position + 8:position + 8 + length]
        position += 12 + length
    width, height, depth, colour = struct.unpack(">IIBB", chunks[b"IHDR"][:10])
    assert (depth, colour) == (16, 2)
    rows = numpy.frombuffer(zlib.decompress(chunks[b"IDAT"]), dtype=numpy.uint8).reshape(height, -1)
    assert (rows[:, 0] == 0).all()
    return rows[:, 1:].copy().view(">u2").reshape(height, width, 3).astype(numpy.int64)


class NF4Test(unittest.TestCase):
    def test_storage_matches_bitsandbytes(self):
        # Packed indices and scales bitsandbytes 0.49.2 produced for these
        # values with blocksize 64 and quant_type "nf4".
        values = torch.tensor(
            [math.sin(index * 0.37) * 0.1 * (1 + index % 5) for index in range(128)], dtype=torch.float32
        ).to(torch.bfloat16)
        packed, scales = nf4.quantize(values)
        self.assertEqual(
            bytes(packed.tolist()).hex(),
            "79cef9bb9564210657be9cddd76410533478adef9a8525321167adf9bcc9752105457b9bdee886315322479cef9bb8564210667be9cddc76410533488adef998",
        )
        self.assertEqual(scales.tolist(), [0.498046875, 0.5])

    def test_values_round_to_the_nearest_level(self):
        torch.manual_seed(0)
        values = torch.randn(64 * 50) * 3.0
        values[:64] = 0.0
        packed, scales = nf4.quantize(values)
        decoded = nf4.dequantize(packed, scales, values.shape, torch.float32)
        levels = torch.tensor(nf4.LEVELS)
        blocks = values.reshape(-1, 64)
        scale = blocks.abs().amax(dim=1, keepdim=True)
        expected = levels[((blocks / scale.clamp_min(1e-30)).unsqueeze(-1) - levels).abs().argmin(dim=-1)] * scale
        self.assertTrue(torch.equal(decoded, expected.reshape(-1)))
        self.assertEqual(decoded[:64].abs().max().item(), 0.0)
        with self.assertRaises(ValueError):
            nf4.quantize(torch.ones(65))


class ConversionTest(unittest.TestCase):
    def setUp(self):
        self.root = Path(self.enterContext(tempfile.TemporaryDirectory()))
        self.qwen, self.marigold = make_sources(self.root)

    def test_layout_is_complete_and_checked(self):
        output = self.root / "converted"
        summary = convert_marigold.convert(self.qwen, self.marigold, output)
        self.assertEqual(summary["blocks"], TRANSFORMER["num_layers"])
        self.assertEqual(files.missing_marigold_files(output), [])
        block = marigold.read(output / files.block_file(1))
        self.assertEqual(block["attn.to_q.weight.nf4"].dtype, torch.uint8)
        self.assertNotIn("attn.to_q.weight", block)
        # The first block's image modulation keeps its full precision.
        first = marigold.read(output / files.block_file(0))
        self.assertIn("img_mod.1.weight", first)
        task = marigold.read(output / files.task_file("depth"))
        self.assertNotIn("iREPAStudentProjector.out.weight", task)
        self.assertEqual(task["prompt.mask"].dtype, torch.bool)
        self.assertIn("vae.decoder.conv_out.weight", task)
        (output / files.block_file(2)).unlink()
        self.assertEqual(files.missing_marigold_files(output), [files.block_file(2)])
        with self.assertRaises(convert_marigold.ConversionError):
            convert_marigold.convert(self.qwen, self.marigold, output)

    def test_missing_manifest_or_wrong_format_is_reported(self):
        output = self.root / "converted"
        output.mkdir()
        self.assertEqual(files.missing_marigold_files(output), ["manifest.json"])
        (output / "manifest.json").write_text(json.dumps({"format": "other", "blocks": 3}))
        self.assertEqual(files.missing_marigold_files(output), [f"manifest.json in format {files.MARIGOLD_FORMAT}"])
        (output / "manifest.json").write_text("[1]")
        self.assertEqual(files.missing_marigold_files(output), [f"manifest.json in format {files.MARIGOLD_FORMAT}"])

    def test_pickled_code_in_a_prompt_is_never_run(self):
        prompt = convert_marigold.TASKS["normals"][1]
        marker = self.root / "ran"
        path = self.marigold / "qwen_text_embeddings" / f"{prompt}_prompt_embeds.pt"
        path.write_bytes(pickle.dumps(_Malicious(marker)))
        output = self.root / "converted"
        with self.assertRaises(convert_marigold.ConversionError):
            convert_marigold.convert(self.qwen, self.marigold, output)
        self.assertFalse(marker.exists())
        self.assertFalse(output.exists())
        self.assertFalse(output.with_name("converted.partial").exists())


class StreamingTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.temporary = tempfile.TemporaryDirectory()
        root = Path(cls.temporary.name)
        cls.qwen, cls.marigold = make_sources(root)
        cls.converted = root / "converted"
        convert_marigold.convert(cls.qwen, cls.marigold, cls.converted)

    @classmethod
    def tearDownClass(cls):
        cls.temporary.cleanup()

    def test_streamed_blocks_give_the_whole_models_answer_one_block_at_a_time(self):
        torch.manual_seed(3)
        latents = torch.randn(1, 4 * 6, 64)
        prompt = torch.randn(1, 7, TRANSFORMER["joint_attention_dim"])
        mask = torch.ones(1, 7, dtype=torch.bool)
        streamed = marigold.StreamedTransformer(
            self.converted, self.converted / files.task_file("depth"), torch.device("cpu"), torch.float32
        )
        resident = []

        def count(_module, _args):
            resident.append(sum(any(p.device.type != "meta" for p in block.parameters()) for block in streamed.blocks))

        # Pre-hooks run in the order registered, so this one sees each block
        # just after its own weights were loaded.
        for block in streamed.blocks:
            block.register_forward_pre_hook(count)
        with torch.no_grad():
            answer = streamed(latents, prompt, mask, 8, 12)
            reference = reference_transformer(self.qwen, self.marigold, "depth")
            timestep = torch.full((1,), marigold.TIMESTEP, dtype=torch.bfloat16) / 1000.0
            expected = reference(
                hidden_states=latents,
                encoder_hidden_states=prompt,
                encoder_hidden_states_mask=mask,
                timestep=timestep,
                img_shapes=[[(1, 4, 6)]],
                return_dict=False,
            )[0]
        torch.testing.assert_close(answer, expected, rtol=1e-4, atol=1e-5)
        self.assertEqual(resident, [1] * TRANSFORMER["num_layers"])
        self.assertTrue(all(p.device.type == "meta" for p in streamed.blocks.parameters()))

    def test_tasks_use_their_own_adapters(self):
        pixels = marigold.prepare(Image.new("RGB", (48, 32), (120, 80, 40)), 256)
        self.assertEqual(tuple(pixels.shape), (1, 3, 32, 48))
        depth = marigold.predict(pixels, self.converted, "depth", torch.device("cpu"), torch.float32)
        normals = marigold.predict(pixels, self.converted, "normals", torch.device("cpu"), torch.float32)
        self.assertEqual(tuple(depth.shape), (3, 32, 48))
        self.assertFalse(torch.equal(depth, normals))
        again = marigold.predict(pixels, self.converted, "depth", torch.device("cpu"), torch.float32)
        self.assertTrue(torch.equal(depth, again))

    def test_photographs_are_scaled_down_to_multiples_of_sixteen(self):
        self.assertEqual(tuple(marigold.prepare(Image.new("RGB", (40, 24)), 256).shape), (1, 3, 32, 48))
        self.assertEqual(tuple(marigold.prepare(Image.new("RGB", (4000, 3000)), 1024).shape), (1, 3, 768, 1024))
        self.assertEqual(tuple(marigold.prepare(Image.new("RGB", (10, 300)), 256).shape), (1, 3, 256, 16))

    def test_pack_and_unpack_are_inverse(self):
        latents = torch.randn(1, 16, 6, 10)
        packed = marigold.pack(latents)
        self.assertEqual(tuple(packed.shape), (1, 15, 64))
        self.assertTrue(torch.equal(marigold.unpack(packed, 6, 10)[:, :, 0], latents))

    def test_the_worker_writes_decodable_maps(self):
        work = Path(self.enterContext(tempfile.TemporaryDirectory()))
        photo = work / "photo.png"
        gradient = numpy.linspace(0, 255, 48 * 32 * 3).reshape(32, 48, 3).astype(numpy.uint8)
        Image.fromarray(gradient).save(photo)
        request = {
            "image": str(photo),
            "model": str(self.converted),
            "depth": str(work / "depth.png"),
            "normals": str(work / "normals.png"),
            "device": "cpu",
            "seed": 0,
        }
        (work / "request.json").write_text(json.dumps(request))
        first = _run(work / "request.json")
        result = first["result"]
        self.assertEqual((result["width"], result["height"]), (48, 32))
        self.assertLess(result["depth"]["near"], result["depth"]["far"])
        depth = numpy.asarray(Image.open(work / "depth.png"))
        self.assertEqual(depth.shape, (32, 48))
        self.assertEqual((int(depth.min()), int(depth.max())), (0, 65535))
        normals = read_png16(work / "normals.png") / 65535.0 * 2.0 - 1.0
        self.assertEqual(normals.shape, (32, 48, 3))
        numpy.testing.assert_allclose(numpy.linalg.norm(normals, axis=2), 1.0, atol=1e-3)
        depth_bytes = (work / "depth.png").read_bytes()
        _run(work / "request.json")
        self.assertEqual((work / "depth.png").read_bytes(), depth_bytes)

    def test_bad_requests_are_refused(self):
        work = Path(self.enterContext(tempfile.TemporaryDirectory()))
        request = work / "request.json"
        request.write_text(json.dumps({"image": "x.png", "model": str(self.converted), "depth": "d.png", "max_side": 100}))
        self.assertEqual(_run(request)["error"]["code"], "validation")
        request.write_text(json.dumps({"image": str(work / "absent.png"), "model": str(self.converted), "depth": "d.png", "device": "cpu"}))
        self.assertEqual(_run(request)["error"]["code"], "validation")
        Image.new("RGB", (32, 32)).save(work / "photo.png")
        request.write_text(json.dumps({"image": str(work / "photo.png"), "model": str(work), "depth": str(work / "d.png"), "device": "cpu"}))
        self.assertEqual(_run(request)["error"]["code"], "checkpoints")
        if not torch.cuda.is_available():
            request.write_text(json.dumps({"image": str(work / "photo.png"), "model": str(self.converted), "depth": "d.png"}))
            self.assertEqual(_run(request)["error"]["code"], "unavailable")
        check = subprocess.run(
            [sys.executable, "-I", str(RECON / "estimate_depth.py"), "--check", str(self.converted)],
            capture_output=True, text=True, check=True,
        )
        self.assertEqual(json.loads(check.stdout), {"missing": []})


def _run(request: Path) -> dict:
    completed = subprocess.run(
        [sys.executable, "-I", str(RECON / "estimate_depth.py"), str(request)],
        capture_output=True, text=True, timeout=600,
    )
    return json.loads(completed.stdout)


class PngTest(unittest.TestCase):
    def test_sixteen_bit_colour_round_trips(self):
        path = Path(self.enterContext(tempfile.TemporaryDirectory())) / "image.png"
        pixels = numpy.arange(5 * 3 * 3, dtype=numpy.uint16).reshape(5, 3, 3) * 1000
        estimate_depth.write_png16(path, pixels.astype(">u2"))
        numpy.testing.assert_array_equal(read_png16(path), pixels)


if __name__ == "__main__":
    unittest.main()
