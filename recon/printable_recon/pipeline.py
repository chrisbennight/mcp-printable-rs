"""Reconstruct FLAME 2020 parameters and surface detail from photographs.

Identity comes from MICA. Each photograph's ArcFace embedding is normalised,
the embeddings are averaged and normalised again, and MICA's mapping network
turns the result into 300 shape coefficients at true head scale. Expression,
jaw rotation and surface detail come from the primary photograph: SMIRK gives
the expression and jaw, and DECA gives the UV displacement map. DECA's detail
decoder is conditioned on DECA's own estimate of the expression and jaw, the
input it was trained with; SMIRK's coefficients are larger and give
exaggerated detail.
"""

from __future__ import annotations

import base64
import io
from dataclasses import dataclass, field, replace
from pathlib import Path

import numpy
import torch
import torch.nn.functional as functional
from PIL import Image

from . import checkpoints, faces, networks
from .files import FILES, missing_files

# Coefficients are in standard deviations; the head workflow accepts at most
# this magnitude.
COEFFICIENT_LIMIT = 5.0
METRES_TO_MILLIMETRES = 1000.0



@dataclass
class Reconstruction:
    identity: list[float]
    expression: list[float]
    jaw: list[float]
    head_rotation: list[float]
    displacement_mm: numpy.ndarray
    identity_agreement: list[float]
    clamped_coefficients: int
    faces: list[dict] = field(default_factory=list)
    # SMIRK's own fit to each photograph: the head and orthographic camera
    # that place FLAME's surface over the face, for projecting colour.
    views: list[dict] = field(default_factory=list)


class Reconstructor:
    def __init__(
        self, directory: Path, landmarker: Path, device: str = "cpu", segmenter: Path | None = None
    ) -> None:
        missing = missing_files(directory)
        if missing:
            raise checkpoints.CheckpointError(f"checkpoint files are missing: {', '.join(missing)}")
        self.device = torch.device(device)
        mica = checkpoints.load(directory / FILES["mica"])
        self.arcface = networks.ArcFace()
        checkpoints.apply(self.arcface, checkpoints.section(mica, "arcface", "MICA"), "MICA")
        self.mapping = networks.IdentityMapping()
        flame = checkpoints.section(mica, "flameModel", "MICA")
        checkpoints.apply(
            self.mapping,
            {key.removeprefix("regressor."): value for key, value in flame.items() if key.startswith("regressor.")},
            "MICA",
        )
        del mica
        self.smirk = networks.Smirk()
        checkpoints.apply(self.smirk, checkpoints.section(checkpoints.load(directory / FILES["smirk"]), "smirk_encoder", "SMIRK"), "SMIRK")
        deca = checkpoints.load(directory / FILES["deca"])
        self.deca = networks.Deca()
        for part in ("E_flame", "E_detail", "D_detail"):
            checkpoints.apply(getattr(self.deca, part), checkpoints.section(deca, part, "DECA"), "DECA")
        del deca
        for module in (self.arcface, self.mapping, self.smirk, self.deca):
            module.to(self.device).eval()
        self.face_eye_mask = _mask(directory / FILES["face_eye_mask"])
        self.landmarker = faces.Landmarker(landmarker)
        self.segmenter = faces.Segmenter(segmenter) if segmenter is not None else None

    def close(self) -> None:
        self.landmarker.close()
        if self.segmenter is not None:
            self.segmenter.close()

    @torch.inference_mode()
    def run(self, images: list[Path]) -> Reconstruction:
        if not images:
            raise faces.FaceError("at least one image is required")
        found: list[faces.Face] = []
        for path in images:
            pixels = faces.decode(path)
            try:
                face = faces.crops(pixels, self.landmarker(pixels))
                if self.segmenter is not None:
                    face = replace(face, segmentation=self.segmenter(pixels, face.landmarks))
                found.append(face)
            except faces.FaceError as error:
                raise faces.FaceError(f"{path.name}: {error}") from error
        return self.reconstruct(found)

    @torch.inference_mode()
    def reconstruct(self, found: list[faces.Face]) -> Reconstruction:
        arcface = _tensor(numpy.stack([face.arcface for face in found]), self.device) * 2.0 - 1.0
        embeddings = functional.normalize(self.arcface(arcface), dim=1)
        fused = functional.normalize(embeddings.mean(dim=0, keepdim=True), dim=1)
        agreement = (embeddings @ fused.T).flatten().tolist()
        identity = self.mapping(fused)[0]

        primary = found[0]
        smirk = self.smirk(_tensor(numpy.stack([face.smirk for face in found]), self.device))
        expression, jaw = smirk["expression"][:1], smirk["jaw"][:1]
        crop = _tensor(primary.deca[None], self.device)
        deca_jaw, deca_expression = self.deca.coarse_expression(crop)
        uv = self.deca.displacement(deca_jaw, deca_expression, self.deca.detail_code(crop))
        # DECA restricts learned detail to the face. Its renderer also adds a
        # fixed offset that follows FLAME's triangle layout to correct
        # shading; that is not surface shape, so it is left out.
        displacement = uv[0, 0].cpu().numpy() * self.face_eye_mask * METRES_TO_MILLIMETRES

        identity, clamped_identity = _clamp(identity)
        expression_values, clamped_expression = _clamp(expression[0])
        return Reconstruction(
            identity=identity,
            expression=expression_values,
            jaw=jaw[0].tolist(),
            head_rotation=smirk["head_rotation"][0].tolist(),
            displacement_mm=displacement.astype(numpy.float32),
            identity_agreement=agreement,
            clamped_coefficients=clamped_identity + clamped_expression,
            faces=[{"landmark_extent_px": _extent(face.landmarks)} for face in found],
            views=[_view(smirk, index, face) for index, face in enumerate(found)],
        )


def write_displacement(map_mm: numpy.ndarray, path: Path) -> dict:
    """Store a displacement map as a 16-bit greyscale PNG.

    A pixel value p means offset_mm + scale_mm * p / 65535 millimetres along
    the surface normal. Row 0 is v = 1 and column 0 is u = 0 in FLAME's UV
    layout.
    """
    low, high = float(map_mm.min()), float(map_mm.max())
    scale = max(high - low, 1e-6)
    levels = numpy.round((map_mm - low) / scale * 65535.0).astype(numpy.uint16)
    Image.fromarray(levels).save(path, format="PNG")
    return {"offset_mm": low, "scale_mm": scale, "width": int(map_mm.shape[1]), "height": int(map_mm.shape[0])}


def _tensor(images: numpy.ndarray, device: torch.device) -> torch.Tensor:
    """Batches of RGB images as float tensors in [0, 1], channels first."""
    return torch.from_numpy(images).to(device).permute(0, 3, 1, 2).float() / 255.0


def _clamp(values: torch.Tensor) -> tuple[list[float], int]:
    clamped = int((values.abs() > COEFFICIENT_LIMIT).sum())
    return values.clamp(-COEFFICIENT_LIMIT, COEFFICIENT_LIMIT).tolist(), clamped


def _view(smirk: dict[str, torch.Tensor], index: int, face: faces.Face) -> dict:
    """SMIRK's fit to one photograph and where its crop lies in the image."""
    def value(name: str) -> list[float]:
        return smirk[name][index].tolist()

    view = {
        "shape": value("shape"),
        "expression": value("expression"),
        "jaw": value("jaw"),
        "head_rotation": value("head_rotation"),
        "camera": value("camera"),
        "crop": {"left": face.smirk_crop.left, "top": face.smirk_crop.top, "step": face.smirk_crop.step},
        "image_size": list(face.image_size),
    }
    if face.segmentation is not None:
        view["segmentation"] = _segmentation(face.segmentation)
    return view


def _segmentation(segmentation: faces.Segmentation) -> dict:
    """The labels as a base64 greyscale PNG, where they lie in the image and
    the hair colour found."""
    buffer = io.BytesIO()
    Image.fromarray(segmentation.labels).save(buffer, format="PNG", optimize=True)
    crop = segmentation.crop
    return {
        "labels_png": base64.b64encode(buffer.getvalue()).decode("ascii"),
        "classes": list(faces.SEGMENT_CLASSES),
        "crop": {"left": crop.left, "top": crop.top, "step": crop.step},
        "hair_rgb": list(segmentation.hair_rgb) if segmentation.hair_rgb is not None else None,
        "hair_pixels": segmentation.hair_pixels,
    }


def _extent(landmarks: numpy.ndarray) -> list[float]:
    return [float(landmarks[:, 0].min()), float(landmarks[:, 1].min()), float(landmarks[:, 0].max()), float(landmarks[:, 1].max())]


def _mask(path: Path) -> numpy.ndarray:
    try:
        with Image.open(path) as image:
            if image.format != "PNG" or image.width * image.height > 4096 * 4096:
                raise checkpoints.CheckpointError(f"{path.name} is not a PNG mask")
            channel = numpy.asarray(image.convert("RGB"), dtype=numpy.float32)[..., 0] / 255.0
    except OSError as error:
        raise checkpoints.CheckpointError(f"{path.name} could not be read: {error}") from error
    tensor = torch.from_numpy(channel)[None, None]
    return functional.interpolate(tensor, size=(256, 256))[0, 0].numpy()
