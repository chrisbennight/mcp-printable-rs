"""Unit tests for the reconstruction package, without published checkpoints.

Networks are built with random weights; the face landmarker is replaced by
fixed landmarks. Checks against real checkpoints and photographs are opt-in:
set PRINTABLE_TEST_RECON_CHECKPOINTS to a checkpoint directory,
PRINTABLE_TEST_RECON_LANDMARKER to the face landmarker model and
PRINTABLE_TEST_RECON_IMAGE to a photograph of one face.
"""

import io
import os
import pickle
import tempfile
import unittest
from pathlib import Path

import numpy
import torch
from PIL import Image

from printable_recon import checkpoints, faces, networks, pipeline


def synthetic_landmarks(width: int = 400, height: int = 400) -> numpy.ndarray:
    """A plausible face layout: points spread over an oval with eyes, nose
    and mouth corners where the crops look for them."""
    angles = numpy.linspace(0, 2 * numpy.pi, 478, endpoint=False)
    points = numpy.stack([width / 2 + 90 * numpy.cos(angles), height / 2 + 120 * numpy.sin(angles)], axis=1)
    points[468] = [width / 2 - 35, height / 2 - 30]
    points[473] = [width / 2 + 35, height / 2 - 30]
    points[4] = [width / 2, height / 2 + 5]
    points[61] = [width / 2 - 30, height / 2 + 45]
    points[291] = [width / 2 + 30, height / 2 + 45]
    return points


class _Malicious:
    def __reduce__(self):
        return (os.system, ("true",))


class CheckpointTests(unittest.TestCase):
    def test_tensors_load_and_code_is_refused(self):
        with tempfile.TemporaryDirectory() as directory:
            good = Path(directory) / "good.pt"
            torch.save({"layer": {"weight": torch.ones(2)}}, good)
            self.assertEqual(checkpoints.section(checkpoints.load(good), "layer", "good")["weight"].sum(), 2)
            bad = Path(directory) / "bad.pt"
            bad.write_bytes(pickle.dumps({"payload": _Malicious()}))
            with self.assertRaises(checkpoints.CheckpointError):
                checkpoints.load(bad)
            with self.assertRaises(checkpoints.CheckpointError):
                checkpoints.load(Path(directory) / "absent.pt")

    def test_weights_must_match_the_network_exactly(self):
        module = torch.nn.Linear(3, 2)
        checkpoints.apply(module, {"weight": torch.zeros(2, 3), "bias": torch.zeros(2)}, "test")
        with self.assertRaises(checkpoints.CheckpointError):
            checkpoints.apply(module, {"weight": torch.zeros(2, 3)}, "test")
        with self.assertRaises(checkpoints.CheckpointError):
            checkpoints.apply(module, {"weight": torch.zeros(2, 3), "bias": torch.zeros(2), "extra": torch.zeros(1)}, "test")
        with self.assertRaises(checkpoints.CheckpointError):
            checkpoints.apply(module, {"weight": torch.zeros(3, 3), "bias": torch.zeros(2)}, "test")


class NetworkTests(unittest.TestCase):
    def test_output_shapes(self):
        torch.manual_seed(0)
        with torch.inference_mode():
            self.assertEqual(networks.ArcFace().eval()(torch.rand(1, 3, 112, 112)).shape, (1, 512))
            self.assertEqual(networks.IdentityMapping()(torch.rand(1, 512)).shape, (1, 300))
            smirk = networks.Smirk().eval()(torch.rand(1, 3, 224, 224))
            self.assertEqual(smirk["expression"].shape, (1, 50))
            self.assertEqual(smirk["jaw"].shape, (1, 3))
            self.assertGreaterEqual(float(smirk["jaw"][0, 0]), 0.0)
            deca = networks.Deca().eval()
            detail = deca.detail_code(torch.rand(1, 3, 224, 224))
            self.assertEqual(detail.shape, (1, 128))
            jaw, expression = deca.coarse_expression(torch.rand(1, 3, 224, 224))
            self.assertEqual((jaw.shape, expression.shape), ((1, 3), (1, 50)))
            uv = deca.displacement(jaw, expression, detail)
            self.assertEqual(uv.shape, (1, 1, 256, 256))
            self.assertLessEqual(float(uv.abs().max()), 0.01)


class FaceTests(unittest.TestCase):
    def test_similarity_recovers_a_known_transform(self):
        source = faces.ARCFACE_TEMPLATE
        angle, scale, shift = 0.3, 2.5, numpy.array([10.0, -4.0])
        rotation = numpy.array([[numpy.cos(angle), -numpy.sin(angle)], [numpy.sin(angle), numpy.cos(angle)]])
        target = scale * source @ rotation.T + shift
        matrix = faces.similarity(source, target)
        numpy.testing.assert_allclose(source @ matrix[:, :2].T + matrix[:, 2], target, atol=1e-9)

    def test_crops_have_the_network_sizes(self):
        image = numpy.random.default_rng(0).integers(0, 255, (400, 400, 3), dtype=numpy.uint8)
        face = faces.crops(image, synthetic_landmarks())
        self.assertEqual(face.smirk.shape, (224, 224, 3))
        self.assertEqual(face.deca.shape, (224, 224, 3))
        self.assertEqual(face.arcface.shape, (112, 112, 3))
        with self.assertRaises(faces.FaceError):
            faces.crops(image, synthetic_landmarks()[:100])

    def test_untrusted_images_are_checked_before_decoding(self):
        with tempfile.TemporaryDirectory() as directory:
            gif = Path(directory) / "face.gif"
            Image.new("RGB", (8, 8)).save(gif, format="GIF")
            with self.assertRaises(faces.FaceError):
                faces.decode(gif)
            text = Path(directory) / "face.png"
            text.write_text("not an image")
            with self.assertRaises(faces.FaceError):
                faces.decode(text)
            png = Path(directory) / "small.png"
            Image.new("RGB", (16, 12), (10, 20, 30)).save(png, format="PNG")
            self.assertEqual(faces.decode(png).shape, (12, 16, 3))


class _FixedLandmarker:
    def __call__(self, image):
        return synthetic_landmarks(image.shape[1], image.shape[0])

    def close(self):
        pass


class PipelineTests(unittest.TestCase):
    def reconstructor(self):
        torch.manual_seed(0)
        reconstructor = pipeline.Reconstructor.__new__(pipeline.Reconstructor)
        reconstructor.device = torch.device("cpu")
        reconstructor.arcface = networks.ArcFace().eval()
        reconstructor.mapping = networks.IdentityMapping().eval()
        reconstructor.smirk = networks.Smirk().eval()
        reconstructor.deca = networks.Deca().eval()
        reconstructor.face_eye_mask = numpy.ones((256, 256), numpy.float32)
        reconstructor.landmarker = _FixedLandmarker()
        return reconstructor

    def test_identity_is_fused_across_photographs(self):
        reconstructor = self.reconstructor()
        rng = numpy.random.default_rng(1)
        first, second = (
            faces.crops(rng.integers(0, 255, (400, 400, 3), dtype=numpy.uint8), synthetic_landmarks())
            for _ in range(2)
        )
        single = reconstructor.reconstruct([first])
        fused = reconstructor.reconstruct([first, second])
        with torch.inference_mode():
            crops = pipeline._tensor(numpy.stack([first.arcface, second.arcface]), reconstructor.device) * 2.0 - 1.0
            embeddings = torch.nn.functional.normalize(reconstructor.arcface(crops), dim=1)
            mean = torch.nn.functional.normalize(embeddings.mean(dim=0, keepdim=True), dim=1)
            expected = reconstructor.mapping(mean)[0].clamp(-pipeline.COEFFICIENT_LIMIT, pipeline.COEFFICIENT_LIMIT)
        numpy.testing.assert_allclose(fused.identity, expected.tolist(), atol=1e-5)
        self.assertGreater(numpy.abs(numpy.subtract(fused.identity, single.identity)).max(), 1e-3)
        # Two unit embeddings are equally close to their normalised mean.
        self.assertLess(fused.identity_agreement[0], 1.0 - 1e-6)
        numpy.testing.assert_allclose(fused.identity_agreement[0], fused.identity_agreement[1], atol=1e-6)
        numpy.testing.assert_allclose(single.identity_agreement, [1.0], atol=1e-5)
        self.assertEqual(len(single.identity), 300)
        self.assertEqual(len(single.expression), 50)
        self.assertTrue(all(abs(value) <= pipeline.COEFFICIENT_LIMIT for value in single.identity))
        self.assertEqual(single.displacement_mm.shape, (256, 256))

    def test_detail_is_conditioned_on_decas_own_expression(self):
        reconstructor = self.reconstructor()
        image = numpy.random.default_rng(2).integers(0, 255, (400, 400, 3), dtype=numpy.uint8)
        face = faces.crops(image, synthetic_landmarks())
        baseline = reconstructor.reconstruct([face])
        smirk = reconstructor.smirk

        def exaggerated(crop):
            output = smirk(crop)
            return {**output, "expression": output["expression"] * 4.0}

        reconstructor.smirk = exaggerated
        changed = reconstructor.reconstruct([face])
        self.assertGreater(numpy.abs(numpy.subtract(changed.expression, baseline.expression)).max(), 1e-3)
        numpy.testing.assert_array_equal(changed.displacement_mm, baseline.displacement_mm)
        with torch.inference_mode():
            crop = pipeline._tensor(face.deca[None], reconstructor.device)
            jaw, expression = reconstructor.deca.coarse_expression(crop)
            uv = reconstructor.deca.displacement(jaw, expression, reconstructor.deca.detail_code(crop))
        numpy.testing.assert_allclose(baseline.displacement_mm, uv[0, 0].numpy() * 1000.0, rtol=1e-6)

    def test_displacement_round_trips_through_sixteen_bit_png(self):
        values = numpy.linspace(-2.0, 3.0, 256 * 256, dtype=numpy.float32).reshape(256, 256)
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "map.png"
            info = pipeline.write_displacement(values, path)
            with Image.open(path) as image:
                levels = numpy.asarray(image, dtype=numpy.float64)
        decoded = info["offset_mm"] + info["scale_mm"] * levels / 65535.0
        self.assertLess(numpy.abs(decoded - values).max(), 5.0 / 65535.0)

    def test_missing_checkpoints_are_listed(self):
        with tempfile.TemporaryDirectory() as directory:
            self.assertEqual(sorted(pipeline.missing_files(Path(directory))), sorted(pipeline.FILES.values()))


@unittest.skipUnless(
    all(os.environ.get(name) for name in ("PRINTABLE_TEST_RECON_CHECKPOINTS", "PRINTABLE_TEST_RECON_LANDMARKER", "PRINTABLE_TEST_RECON_IMAGE")),
    "requires published checkpoints and a photograph",
)
class PublishedCheckpointTests(unittest.TestCase):
    def test_reconstruction_is_repeatable(self):
        reconstructor = pipeline.Reconstructor(
            Path(os.environ["PRINTABLE_TEST_RECON_CHECKPOINTS"]), Path(os.environ["PRINTABLE_TEST_RECON_LANDMARKER"])
        )
        try:
            image = Path(os.environ["PRINTABLE_TEST_RECON_IMAGE"])
            first = reconstructor.run([image])
            second = reconstructor.run([image])
        finally:
            reconstructor.close()
        self.assertEqual(first.identity, second.identity)
        self.assertEqual(first.expression, second.expression)
        numpy.testing.assert_array_equal(first.displacement_mm, second.displacement_mm)
        self.assertGreater(float(numpy.abs(first.displacement_mm).max()), 0.0)


if __name__ == "__main__":
    unittest.main()
