"""Unit tests for object generation with Pixal3D, without the published models.

Miniature networks with random weights stand in for Pixal3D, DINOv3 and NAF,
laid out as the operator's model directory is. They exercise the sparse
voxel operations, the networks' loading, the stages and the worker's request
handling on the processor.
"""

import contextlib
import io
import json
import math
import struct
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

import numpy
import torch
import torch.nn.functional as F
from PIL import Image
from safetensors.torch import save_file

import generate_object
from printable_recon import features, files, objects, pixal3d, voxels

RECON = Path(__file__).resolve().parent.parent

HIDDEN = 32  # DINOv3's width in the miniature models
LATENT = 8
DINOV3 = {
    "hidden_size": HIDDEN,
    "intermediate_size": 64,
    "num_hidden_layers": 2,
    "num_attention_heads": 2,
    "num_register_tokens": 4,
    "patch_size": 16,
    "image_size": 224,
    "rope_theta": 100.0,
    "layerscale_value": 1.0,
    "use_gated_mlp": False,
}
FLOW = {
    "model_channels": 32,
    "cond_channels": HIDDEN,
    "num_blocks": 1,
    "num_heads": 2,
    "mlp_ratio": 2.0,
    "pe_mode": "rope",
    "share_mod": True,
    "initialization": "scaled",
    "qk_rms_norm": True,
    "qk_rms_norm_cross": True,
    "image_attn_mode": "proj",
    "dtype": "float32",
}
CHECKPOINTS = {
    "sparse_structure_flow_model": (
        "ckpts/ss_flow",
        "SparseStructureFlowModel",
        {**FLOW, "resolution": 16, "in_channels": LATENT, "out_channels": LATENT},
    ),
    "sparse_structure_decoder": (
        "ckpts/ss_dec",
        "SparseStructureDecoder",
        {"out_channels": 1, "latent_channels": LATENT, "num_res_blocks": 1, "channels": [16, 8, 4],
         "num_res_blocks_middle": 1, "use_fp16": False},
    ),
    "shape_slat_flow_model_512": (
        "ckpts/slat_512",
        "ElasticSLatFlowModel",
        {**FLOW, "resolution": 32, "in_channels": LATENT, "out_channels": LATENT, "proj_in_channels": 2 * HIDDEN},
    ),
    "shape_slat_flow_model_1024": (
        "ckpts/slat_1024",
        "ElasticSLatFlowModel",
        {**FLOW, "resolution": 64, "in_channels": LATENT, "out_channels": LATENT, "proj_in_channels": 2 * HIDDEN},
    ),
    "shape_slat_decoder": (
        "ckpts/shape_dec",
        "FlexiDualGridVaeDecoder",
        {"resolution": 256, "model_channels": [16, 16, 8, 8, 8], "latent_channels": LATENT,
         "num_blocks": [1, 1, 1, 1, 0], "block_type": ["SparseConvNeXtBlock3d"] * 5,
         "up_block_type": ["SparseResBlockC2S3d"] * 4, "block_args": [{}] * 5, "use_fp16": False},
    ),
}
# Shifts the miniature structure decoder's logits so a few hundred voxels are occupied.
OCCUPANCY_BIAS = -1.0
# The miniature shape decoder keeps the four children of each voxel on its
# low-x side, a 2 x 2 block around an x-axis edge, so its surface has quads.
SUBDIVISION_LOGITS = [1.0, -1.0, 1.0, -1.0, 1.0, -1.0, 1.0, -1.0]
SAMPLER = {
    "name": "FlowEulerGuidanceIntervalSampler",
    "args": {"sigma_min": 1e-5},
    "params": {"steps": 2, "guidance_strength": 3.0, "guidance_rescale": 0.5, "guidance_interval": [0.6, 1.0],
               "rescale_t": 3.0},
}


def _randomize(module: torch.nn.Module, scale: float = 0.2) -> torch.nn.Module:
    with torch.no_grad():
        for parameter in module.parameters():
            parameter.copy_(torch.randn_like(parameter) * scale)
    return module


def _save(module: torch.nn.Module, path: Path) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    save_file({name: tensor.contiguous() for name, tensor in module.state_dict().items()}, str(path))


def _build(name: str, config_path: Path) -> torch.nn.Module:
    """A network of the checkpoint's kind with real, random weights."""
    builders = {
        "SparseStructureFlowModel": pixal3d.build_flow,
        "ElasticSLatFlowModel": pixal3d.build_flow,
        "SparseStructureDecoder": pixal3d.build_structure_decoder,
        "FlexiDualGridVaeDecoder": pixal3d.build_shape_decoder,
    }
    model = builders[name](config_path).to_empty(device="cpu")
    return _randomize(model)


def make_models(root: Path) -> Path:
    """A miniature Pixal3D model directory."""
    from transformers import DINOv3ViTConfig, DINOv3ViTModel

    torch.manual_seed(3)
    directory = root / "pixal3d"
    for stage, (base, name, args) in CHECKPOINTS.items():
        config = directory / f"{base}.json"
        config.parent.mkdir(parents=True, exist_ok=True)
        config.write_text(json.dumps({"name": name, "args": args}), encoding="utf-8")
        model = _build(name, config)
        with torch.no_grad():
            if name == "SparseStructureDecoder":
                # A few occupied voxels, so the shape stages stay small.
                model.out_layer[2].weight.mul_(10)
                model.out_layer[2].bias.fill_(OCCUPANCY_BIAS)
            if name == "FlexiDualGridVaeDecoder":
                for level in model.blocks[:-1]:
                    level[-1].to_subdiv.weight.zero_()
                    level[-1].to_subdiv.bias.copy_(torch.tensor(SUBDIVISION_LOGITS))
        _save(model, directory / f"{base}.safetensors")
    pipeline = {
        "name": "Trellis2ImageTo3DPipeline",
        "args": {
            "models": {stage: base for stage, (base, _, _) in CHECKPOINTS.items()},
            "sparse_structure_sampler": SAMPLER,
            "shape_slat_sampler": SAMPLER,
            "shape_slat_normalization": {"mean": [0.1] * LATENT, "std": [1.5] * LATENT},
        },
    }
    (directory / "pipeline.json").write_text(json.dumps(pipeline), encoding="utf-8")
    config = DINOv3ViTConfig(**DINOV3)
    dino = _randomize(DINOv3ViTModel(config), 0.05)
    (directory / "dinov3").mkdir()
    config.to_json_file(str(directory / "dinov3" / "config.json"))
    # Published checkpoints name the encoder's layers without its prefix.
    state = {
        name.removeprefix("model."): tensor.contiguous()
        for name, tensor in dino.state_dict().items()
    }
    save_file(state, str(directory / "dinov3" / "model.safetensors"))
    (directory / "naf").mkdir()
    naf = _randomize(features.NAF(channels=16), 0.05)
    # The rotary periods are fixed in NAF, from a base of 100.
    periods = naf.image_encoder.rope.periods
    periods.copy_(100.0 ** (2 * torch.arange(periods.shape[0]) / (2 * periods.shape[0])))
    torch.save(naf.state_dict(), directory / "naf" / "naf_release.pth")
    (directory / "birefnet").mkdir()
    (directory / "birefnet" / "model.safetensors").write_bytes(b"")
    return directory


def cutout_photo(path: Path, size: int = 96) -> None:
    """A transparent photograph of a red disc."""
    yy, xx = numpy.mgrid[0:size, 0:size]
    inside = (xx - size * 0.55) ** 2 + (yy - size * 0.5) ** 2 < (size * 0.3) ** 2
    pixels = numpy.zeros((size, size, 4), dtype=numpy.uint8)
    pixels[inside] = (200, 40, 30, 255)
    Image.fromarray(pixels).save(path)


class VoxelTest(unittest.TestCase):
    def test_lookup_finds_rows_and_misses_absent_and_outside_voxels(self):
        coords = torch.tensor([[1, 2, 3], [0, 0, 0], [3, 3, 3]], dtype=torch.int32)
        lookup = voxels.Lookup(coords, 4)
        query = torch.tensor([[3, 3, 3], [1, 2, 3], [0, 0, 1], [4, 0, 0], [-1, 0, 0]])
        self.assertEqual(lookup.find(query).tolist(), [2, 0, -1, -1, -1])

    def test_convolution_matches_a_dense_convolution_on_the_voxels_present(self):
        torch.manual_seed(0)
        size, channels, out = 6, 3, 5
        occupied = torch.rand(size, size, size) < 0.4
        coords = torch.argwhere(occupied).int()
        feats = torch.randn(coords.shape[0], channels)
        weight = torch.randn(out, 3, 3, 3, channels)
        bias = torch.randn(out)
        dense = torch.zeros(1, channels, size, size, size)
        dense[0, :, coords[:, 0], coords[:, 1], coords[:, 2]] = feats.t()
        expected = F.conv3d(dense, weight.permute(0, 4, 1, 2, 3), bias, padding=1)
        expected = expected[0, :, coords[:, 0], coords[:, 1], coords[:, 2]].t()
        got = voxels.submanifold_conv(feats, voxels.neighbours(coords, size), weight, bias, rows_per_chunk=7)
        torch.testing.assert_close(got, expected, rtol=1e-5, atol=1e-5)

    def test_subdivision_places_each_child_in_its_octant(self):
        coords = torch.tensor([[1, 0, 2], [0, 3, 0]], dtype=torch.int32)
        children = torch.zeros(2, 8, dtype=torch.bool)
        children[0, [0, 7]] = True
        children[1, 3] = True
        fine, parent, child = voxels.subdivide(coords, children)
        self.assertEqual(fine.tolist(), [[2, 0, 4], [3, 1, 5], [1, 7, 0]])
        self.assertEqual(parent.tolist(), [0, 0, 1])
        self.assertEqual(child.tolist(), [0, 7, 3])

    def test_each_crossed_edge_with_four_voxels_becomes_a_quad(self):
        # Four voxels around the x-axis edge at the lower corner of voxel 0.
        coords = torch.tensor([[1, 1, 1], [1, 1, 2], [1, 2, 2], [1, 2, 1], [3, 3, 3]], dtype=torch.int32)
        vertices = torch.full((5, 3), 0.5)
        intersected = torch.zeros(5, 3, dtype=torch.bool)
        intersected[0, 0] = True
        intersected[4, 0] = True  # its neighbours are absent: no quad
        split = torch.ones(5, 1)
        split[1] = 2.0  # favours the diagonal through voxels 1 and 3
        points, triangles = voxels.dual_grid_mesh(coords, vertices, intersected, split, 4)
        self.assertEqual(triangles.tolist(), [[0, 1, 3], [3, 1, 2]])
        torch.testing.assert_close(points[0], torch.tensor([-0.125, -0.125, -0.125]))
        split[1] = 0.5
        _, triangles = voxels.dual_grid_mesh(coords, vertices, intersected, split, 4)
        self.assertEqual(triangles.tolist(), [[0, 1, 2], [0, 2, 3]])


class NetworkTest(unittest.TestCase):
    def setUp(self):
        self.root = Path(self.enterContext(tempfile.TemporaryDirectory()))

    def _checkpoint(self, stage: str) -> tuple[Path, torch.nn.Module]:
        base, name, args = CHECKPOINTS[stage]
        config = self.root / f"{base}.json"
        config.parent.mkdir(parents=True, exist_ok=True)
        config.write_text(json.dumps({"name": name, "args": args}), encoding="utf-8")
        model = _build(name, config)
        _save(model, self.root / f"{base}.safetensors")
        return config, model

    def test_weights_load_exactly_and_mismatches_are_refused(self):
        config, model = self._checkpoint("shape_slat_flow_model_512")
        weights = config.with_suffix(".safetensors")
        loaded = pixal3d.load(pixal3d.build_flow(config), weights, torch.device("cpu"))
        for name, tensor in model.state_dict().items():
            self.assertTrue(torch.equal(loaded.state_dict()[name], tensor), name)
        # Phases a checkpoint stores for itself are recomputed, not required.
        state = model.state_dict()
        state["rope_phases"] = torch.ones(3, 2, dtype=torch.complex64)
        save_file({name: tensor.contiguous() for name, tensor in state.items()}, str(weights))
        pixal3d.load(pixal3d.build_flow(config), weights, torch.device("cpu"))
        del state["rope_phases"], state["out_layer.bias"]
        save_file({name: tensor.contiguous() for name, tensor in state.items()}, str(weights))
        with self.assertRaisesRegex(pixal3d.ModelError, "1 missing"):
            pixal3d.load(pixal3d.build_flow(config), weights, torch.device("cpu"))
        state["out_layer.bias"] = torch.zeros(3)
        save_file({name: tensor.contiguous() for name, tensor in state.items()}, str(weights))
        with self.assertRaisesRegex(pixal3d.ModelError, "shape"):
            pixal3d.load(pixal3d.build_flow(config), weights, torch.device("cpu"))
        weights.write_bytes(b"\xff" * 16)
        with self.assertRaisesRegex(pixal3d.ModelError, "not a safetensors file"):
            pixal3d.load(pixal3d.build_flow(config), weights, torch.device("cpu"))

    def test_unsupported_configurations_are_refused(self):
        config, _ = self._checkpoint("sparse_structure_flow_model")
        settings = json.loads(config.read_text())
        settings["args"]["image_attn_mode"] = "cross"
        config.write_text(json.dumps(settings))
        with self.assertRaisesRegex(pixal3d.ModelError, "image_attn_mode"):
            pixal3d.build_flow(config)
        settings["name"] = "SomethingElse"
        config.write_text(json.dumps(settings))
        with self.assertRaisesRegex(pixal3d.ModelError, "flow transformer"):
            pixal3d.build_flow(config)

    def test_tokens_attend_by_position_not_order(self):
        torch.manual_seed(1)
        _, model = self._checkpoint("shape_slat_flow_model_512")
        coords = torch.argwhere(torch.rand(8, 8, 8) < 0.2).int()
        x = torch.randn(coords.shape[0], LATENT)
        projected = torch.randn(coords.shape[0], 2 * HIDDEN)
        context = torch.randn(5, HIDDEN)
        t = torch.tensor([500.0])
        order = torch.randperm(coords.shape[0])
        with torch.no_grad():
            straight = model(x, t, coords, context, projected)
            shuffled = model(x[order], t, coords[order], context, projected[order])
        torch.testing.assert_close(shuffled, straight[order], rtol=1e-4, atol=1e-5)

    def test_sampling_integrates_the_flow_and_guides_inside_the_interval(self):
        settings = pixal3d.Sampler(steps=4, rescale_t=1.0, guidance_strength=3.0, guidance_rescale=0.0,
                                   guidance_interval=(0.5, 1.0), sigma_min=1e-5)
        calls = []

        def model(x, t, positive):
            calls.append((round(float(t) / 1000, 3), positive))
            return torch.ones_like(x) * (2.0 if positive else 1.0)

        noise = torch.zeros(3, 2)
        result = pixal3d.sample(model, noise, settings, pixal3d.token_std)
        # Guided steps, from t = 1, 0.75 and 0.5, move by 3 * 2 - 2 * 1 = 4
        # per unit time; the last, unguided, by 2.
        torch.testing.assert_close(result, torch.full((3, 2), -(0.75 * 4 + 0.25 * 2)))
        self.assertEqual(calls, [(1.0, True), (1.0, False), (0.75, True), (0.75, False), (0.5, True), (0.5, False),
                                 (0.25, True)])

    def test_work_in_small_chunks_matches_work_in_one(self):
        torch.manual_seed(5)
        _, flow = self._checkpoint("shape_slat_flow_model_512")
        _, decoder = self._checkpoint("shape_slat_decoder")
        coords = torch.argwhere(torch.rand(6, 6, 6) < 0.5).int()
        tokens = torch.randn(coords.shape[0], LATENT)
        inputs = (tokens, torch.tensor([300.0]), coords, torch.randn(5, HIDDEN), torch.randn(coords.shape[0], 2 * HIDDEN))
        latent = pixal3d.Voxels(tokens, coords, 6)
        with torch.no_grad():
            whole = flow(*inputs), decoder(latent)
            size = voxels.CHUNK_BYTES
            voxels.CHUNK_BYTES = 512
            try:
                chunked = flow(*inputs), decoder(pixal3d.Voxels(tokens, coords, 6))
            finally:
                voxels.CHUNK_BYTES = size
        torch.testing.assert_close(chunked[0], whole[0], rtol=1e-5, atol=1e-6)
        torch.testing.assert_close(chunked[1][0], whole[1][0], rtol=1e-5, atol=1e-6)
        self.assertTrue(torch.equal(chunked[1][1], whole[1][1]))

    def test_the_shape_decoder_subdivides_to_its_surface(self):
        torch.manual_seed(2)
        config, model = self._checkpoint("shape_slat_decoder")
        coords = torch.argwhere(torch.rand(4, 4, 4) < 0.5).int()
        latent = pixal3d.Voxels(torch.randn(coords.shape[0], LATENT), coords, 4)
        with torch.no_grad():
            fine = model.upsample(latent, 2)
            self.assertEqual(int(fine.max()) < 16, True)
            points, triangles = model(latent)
        self.assertEqual(points.shape[1], 3)
        self.assertTrue(bool(((points >= -0.5 - 1 / 64) & (points <= 0.5 + 1 / 64)).all()))
        if triangles.shape[0]:
            self.assertLess(int(triangles.max()), points.shape[0])


class FeatureTest(unittest.TestCase):
    def test_framing_centres_the_subject_on_black(self):
        root = Path(self.enterContext(tempfile.TemporaryDirectory()))
        cutout_photo(root / "disc.png", 100)
        image, fov = features.open_photograph(root / "disc.png")
        self.assertEqual((image.mode, fov), ("RGBA", None))
        framed, fraction = features.frame(image)
        self.assertEqual(framed.mode, "RGB")
        self.assertEqual(framed.width, framed.height)
        self.assertAlmostEqual(fraction, framed.width / 100)
        self.assertAlmostEqual(framed.width, 60 * features.FRAME_MARGIN, delta=2)
        pixels = numpy.asarray(framed)
        self.assertEqual(pixels[0, 0].tolist(), [0, 0, 0])
        self.assertEqual(pixels[framed.height // 2, framed.width // 2].tolist(), [200, 40, 30])
        empty = Image.new("RGBA", (10, 10))
        with self.assertRaisesRegex(ValueError, "no subject"):
            features.frame(empty)

    def test_opaque_photographs_are_rgb_and_their_lens_sets_the_view(self):
        root = Path(self.enterContext(tempfile.TemporaryDirectory()))
        photo = Image.new("RGBA", (400, 300), (10, 20, 30, 255))
        exif = Image.Exif()
        exif.get_ifd(features.EXIF_IFD)[features.EXIF_FOCAL_35MM] = 26
        photo.convert("RGB").save(root / "phone.jpg", exif=exif)
        image, fov = features.open_photograph(root / "phone.jpg")
        self.assertEqual(image.mode, "RGB")
        width_mm = features.FILM_DIAGONAL_MM * 400 / 500
        self.assertAlmostEqual(fov, 2 * math.atan(width_mm / 52))
        # The framed square sees the share of that view its width takes.
        framed = features.framed_fov(fov, 0.25)
        self.assertAlmostEqual(math.tan(framed / 2), 0.25 * math.tan(fov / 2))
        self.assertAlmostEqual(features.framed_fov(None, 0.25), math.radians(features.DEFAULT_FOV_DEGREES))
        self.assertAlmostEqual(features.framed_fov(fov, 0.001), math.radians(features.FOV_RANGE_DEGREES[0]))
        # A fully opaque alpha channel marks no subject.
        photo.save(root / "opaque.png")
        image, fov = features.open_photograph(root / "opaque.png")
        self.assertEqual((image.mode, fov), ("RGB", None))

    def test_the_cube_fills_the_view_at_the_camera_distance(self):
        fov = math.radians(30)
        distance = features.camera_distance(fov)
        self.assertAlmostEqual(distance, 0.5 / math.tan(fov / 2), places=5)
        grid, side = 33, 512
        coords = torch.tensor([[16, 16, 16], [0, 16, 16], [32, 16, 16], [16, 16, 32]], dtype=torch.int32)
        positions = features.project(coords, grid, fov, distance, side)
        self.assertTrue(torch.allclose(positions[0], torch.tensor([1 / side, 1 / side]), atol=1e-6))
        self.assertAlmostEqual(float(positions[1, 0]), (0 + 0.5) / side * 2 - 1, places=4)
        self.assertAlmostEqual(float(positions[2, 0]), -float(positions[1, 0]) + 2 / side, places=4)
        # Grid z rises towards the viewer and up in the picture is grid y; a
        # voxel nearer the camera spreads out from the centre.
        self.assertAlmostEqual(float(positions[3, 0]), float(positions[0, 0]), places=6)
        far = features.project(torch.tensor([[0, 16, 0]], dtype=torch.int32), grid, fov, distance, side)
        near = features.project(torch.tensor([[0, 16, 32]], dtype=torch.int32), grid, fov, distance, side)
        self.assertLess(float(near[0, 0]), float(far[0, 0]))

    def test_upsampling_attends_to_the_nearest_low_resolution_features(self):
        torch.manual_seed(4)
        heads, kernel, low, size = 2, 3, 5, 15
        queries = torch.randn(1, heads * 4, size, size)
        keys = torch.randn(1, heads * 4, low, low)
        values = torch.randn(1, heads * 6, low, low)
        got = features._neighbourhood_attention(queries, keys, values, heads, kernel)
        # Each output pixel attends to the kernel x kernel features around
        # the low-resolution cell it lies in, clamped at the borders.
        cell = size // low
        q = queries[0].reshape(heads, 4, size, size)
        k = keys[0].reshape(heads, 4, low, low)
        v = values[0].reshape(heads, 6, low, low)
        expected = torch.empty(heads, 6, size, size)
        for y in range(size):
            for x in range(size):
                top = min(max(y // cell - kernel // 2, 0), low - kernel)
                left = min(max(x // cell - kernel // 2, 0), low - kernel)
                kw = k[:, :, top : top + kernel, left : left + kernel].reshape(heads, 4, -1)
                vw = v[:, :, top : top + kernel, left : left + kernel].reshape(heads, 6, -1)
                weights = torch.einsum("hd,hdn->hn", q[:, :, y, x], kw).mul(4**-0.5).softmax(-1)
                expected[:, :, y, x] = torch.einsum("hn,hdn->hd", weights, vw)
        torch.testing.assert_close(got, expected.reshape(1, -1, size, size), rtol=1e-5, atol=1e-5)

    def test_model_files_are_checked(self):
        root = Path(self.enterContext(tempfile.TemporaryDirectory()))
        self.assertEqual(files.missing_object_files(root), ["pipeline.json"])
        directory = make_models(root)
        self.assertEqual(files.missing_object_files(directory), [])
        (directory / "naf" / "naf_release.pth").unlink()
        self.assertEqual(files.missing_object_files(directory), ["naf/naf_release.pth"])
        pipeline = json.loads((directory / "pipeline.json").read_text())
        pipeline["args"]["models"]["shape_slat_decoder"] = "../outside"
        (directory / "pipeline.json").write_text(json.dumps(pipeline))
        self.assertEqual(files.missing_object_files(directory), ["pipeline.json"])
        with self.assertRaisesRegex(ValueError, "outside the model directory"):
            files.pixal3d_checkpoints(directory)

    def test_dinov3_loads_under_published_names(self):
        root = Path(self.enterContext(tempfile.TemporaryDirectory()))
        directory = make_models(root)
        model = features.load_dinov3(directory / "dinov3", torch.device("cpu"))
        pixels = torch.rand(1, 3, 64, 64)
        with torch.no_grad():
            global_tokens, patches = features.dinov3_tokens(model, pixels)
        self.assertEqual(tuple(global_tokens.shape), (5, HIDDEN))
        self.assertEqual(tuple(patches.shape), (1, HIDDEN, 4, 4))
        torch.save({"wrong": torch.zeros(1)}, directory / "naf" / "naf_release.pth")
        with self.assertRaisesRegex(pixal3d.ModelError, "NAF"):
            features.load_naf(directory / "naf" / "naf_release.pth", torch.device("cpu"))


class MeshFileTest(unittest.TestCase):
    def test_the_stl_stands_up_facing_the_camera(self):
        root = Path(self.enterContext(tempfile.TemporaryDirectory()))
        # The generator's cube: x right, y up, z towards the camera.
        vertices = numpy.array([[0.0, 0.25, 0.0], [-0.1, -0.25, 0.0], [0.1, -0.25, 0.0], [0.0, -0.25, 0.2]])
        triangles = numpy.array([[0, 1, 2], [0, 2, 3], [0, 3, 1], [1, 3, 2]])
        extent = generate_object.write_stl(vertices, triangles, 50.0, root / "shape.stl")
        data = (root / "shape.stl").read_bytes()
        rows = numpy.frombuffer(data[84:], dtype=numpy.dtype(
            [("normal", "<f4", 3), ("corners", "<f4", (3, 3)), ("attribute", "<u2")]))
        top = rows["corners"][0, 0]
        self.assertEqual(extent, [20.0, 20.0, 50.0])
        numpy.testing.assert_allclose(top, [0.0, 10.0, 50.0], atol=1e-5)
        # The point nearest the camera lies towards -Y.
        numpy.testing.assert_allclose(rows["corners"][1, 2], [0.0, -10.0, 0.0], atol=1e-5)
        normals = rows["normal"]
        numpy.testing.assert_allclose(numpy.linalg.norm(normals, axis=1), 1.0, atol=1e-5)


class GenerationTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.temporary = tempfile.TemporaryDirectory()
        cls.root = Path(cls.temporary.name)
        cls.models = make_models(cls.root)
        cutout_photo(cls.root / "disc.png")
        # Smaller pictures keep the miniature runs quick; the stages are unchanged.
        cls.sizes = (objects.COARSE_IMAGE, objects.FINE_IMAGE, objects.NAF_SIZE)
        objects.COARSE_IMAGE, objects.FINE_IMAGE, objects.NAF_SIZE = 160, 320, 160

    @classmethod
    def tearDownClass(cls):
        objects.COARSE_IMAGE, objects.FINE_IMAGE, objects.NAF_SIZE = cls.sizes
        cls.temporary.cleanup()

    def test_generation_runs_every_stage_and_reports_them(self):
        photo, photo_fov = features.open_photograph(self.root / "disc.png")
        shape = objects.generate(photo, photo_fov, self.models, RECON / "absent", torch.device("cpu"),
                                 resolution=1024, steps=1, seed=5, fov=math.radians(25))
        again = objects.generate(photo, photo_fov, self.models, RECON / "absent", torch.device("cpu"),
                                 resolution=1024, steps=1, seed=5, fov=math.radians(25))
        self.assertEqual(shape.resolution, 1024)
        self.assertEqual(shape.steps, {"structure": 1, "shape": 1})
        self.assertEqual((shape.fov_source, shape.background), ("request", "transparency"))
        self.assertEqual([stage["stage"] for stage in shape.report.stages],
                         ["background", "image features", "structure", "shape at 512", "subdivision", "shape at 1024",
                          "surface"])
        self.assertGreater(shape.tokens["structure"], 0)
        self.assertTrue(numpy.array_equal(shape.triangles, again.triangles))
        self.assertTrue(numpy.array_equal(shape.vertices, again.vertices))

    def test_the_worker_script_writes_a_scaled_stl(self):
        mesh = self.root / "disc.stl"
        request = {"image": str(self.root / "disc.png"), "model": str(self.models), "mesh": str(mesh),
                   "resolution": 512, "steps": 1, "size_mm": 40, "device": "cpu"}
        answer = self._run(request)
        self.assertIn("result", answer, answer)
        result = answer["result"]
        self.assertEqual(result["resolution"], 512)
        self.assertEqual((result["fov_source"], result["fov_degrees"]), ("assumed", features.DEFAULT_FOV_DEGREES))
        data = mesh.read_bytes()
        (count,) = struct.unpack("<I", data[80:84])
        self.assertEqual(count, result["triangles"])
        self.assertEqual(len(data), 84 + 50 * count)
        rows = numpy.frombuffer(data[84:], dtype=numpy.dtype(
            [("normal", "<f4", 3), ("corners", "<f4", (3, 3)), ("attribute", "<u2")]))
        corners = rows["corners"].reshape(-1, 3)
        extent = corners.max(axis=0) - corners.min(axis=0)
        self.assertAlmostEqual(float(extent.max()), 40.0, places=3)
        self.assertAlmostEqual(float(corners[:, 2].min()), 0.0, places=4)
        numpy.testing.assert_allclose(extent, result["size_mm"], atol=1e-3)

    def test_requests_and_models_are_checked(self):
        mesh = self.root / "refused.stl"
        base = {"image": str(self.root / "disc.png"), "model": str(self.models), "mesh": str(mesh), "device": "cpu"}
        for change, message in [
            ({"resolution": 768}, "resolution"),
            ({"steps": 0}, "steps"),
            ({"fov_degrees": 1}, "fov_degrees"),
            ({"size_mm": 1}, "size_mm"),
            ({"seed": True}, "invalid"),
        ]:
            answer = self._run({**base, **change})
            self.assertEqual(answer["error"]["code"], "validation", change)
            self.assertIn(message, answer["error"]["message"])
        answer = self._run({**base, "model": str(self.root / "absent")})
        self.assertEqual(answer["error"]["code"], "checkpoints")
        self.assertFalse(mesh.exists())
        # The script answers on its own, as the worker runs it.
        check = subprocess.run([sys.executable, str(RECON / "generate_object.py"), "--check", str(self.models)],
                               capture_output=True, text=True, check=True)
        self.assertEqual(json.loads(check.stdout), {"missing": []})
        path = self.root / "refused.json"
        path.write_text(json.dumps({**base, "resolution": 64}), encoding="utf-8")
        run = subprocess.run([sys.executable, str(RECON / "generate_object.py"), str(path)],
                             capture_output=True, text=True)
        self.assertEqual(run.returncode, 2)
        self.assertEqual(json.loads(run.stdout)["error"]["code"], "validation")

    def _run(self, request: dict) -> dict:
        path = self.root / "request.json"
        path.write_text(json.dumps(request), encoding="utf-8")
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            generate_object.main([str(path)])
        return json.loads(output.getvalue().strip().splitlines()[-1])


if __name__ == "__main__":
    unittest.main()
