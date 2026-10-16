"""Image features for Pixal3D: the cut-out subject, its camera, and the
DINOv3 features each voxel sees.

Pixal3D conditions every stage on the photograph twice: on DINOv3's global
tokens, and on the patch features found where each latent voxel projects into
the picture through a pinhole camera looking at the unit cube. The finer
stages also sample features that NAF has upsampled with the photograph as a
guide.
"""

from __future__ import annotations

import importlib
import math
import sys
import types
from pathlib import Path

import numpy
import torch
import torch.nn.functional as F
from PIL import Image, ImageOps
from torch import nn

from .pixal3d import ModelError, load

# The longest side a photograph is reduced to before the subject is found.
MAX_PHOTO_SIDE = 1024
# The square around the subject is this much larger than its extent.
FRAME_MARGIN = 1.1
# Pixels at least this opaque count towards the subject's extent.
SUBJECT_ALPHA = 0.8
# The horizontal field of view assumed for the framed subject when neither
# the request nor the photograph's metadata gives one: that of the example
# camera rig Pixal3D ships.
DEFAULT_FOV_DEGREES = 20.0
FOV_RANGE_DEGREES = (5.0, 90.0)
# The 35 mm film frame, whose diagonal defines an equivalent focal length.
FILM_DIAGONAL_MM = math.hypot(36.0, 24.0)
# Exif tags: the Exif sub-directory and the 35 mm equivalent focal length.
EXIF_IFD = 0x8769
EXIF_FOCAL_35MM = 0xA405
IMAGENET_MEAN = (0.485, 0.456, 0.406)
IMAGENET_STD = (0.229, 0.224, 0.225)
SEGMENTATION_SIDE = 1024


def open_photograph(path: Path) -> tuple[Image.Image, float | None]:
    """The photograph upright, RGBA if it carries real transparency and RGB
    otherwise, with the horizontal field of view its metadata implies, if any."""
    with Image.open(path) as opened:
        focal = _focal_35mm(opened)
        image = ImageOps.exif_transpose(opened)
        image.load()
    if image.mode in ("RGBA", "LA", "PA") or "transparency" in image.info:
        image = image.convert("RGBA")
        if numpy.asarray(image)[:, :, 3].min() == 255:
            image = image.convert("RGB")
    else:
        image = image.convert("RGB")
    fov = None
    if focal is not None:
        width, height = image.size
        film_width = FILM_DIAGONAL_MM * width / math.hypot(width, height)
        fov = 2.0 * math.atan(film_width / (2.0 * focal))
    return image, fov


def _focal_35mm(image: Image.Image) -> float | None:
    try:
        value = image.getexif().get_ifd(EXIF_IFD).get(EXIF_FOCAL_35MM)
        focal = float(value)
    # Exif in the wild is often malformed; a photograph without a usable
    # value simply has no field of view of its own.
    except (TypeError, ValueError, KeyError, AttributeError, OSError, SyntaxError):
        return None
    return focal if 8.0 <= focal <= 1200.0 else None


def reduce(image: Image.Image) -> Image.Image:
    """The photograph with its longest side at most MAX_PHOTO_SIDE."""
    scale = min(1, MAX_PHOTO_SIDE / max(image.size))
    if scale < 1:
        image = image.resize((int(image.width * scale), int(image.height * scale)), Image.Resampling.LANCZOS)
    return image


def frame(cutout: Image.Image) -> tuple[Image.Image, float]:
    """The subject of an RGBA cut-out, centred in a square on black.

    Returns the framed RGB image and the square's side as a fraction of the
    cut-out's width, which scales the field of view.
    """
    alpha = numpy.asarray(cutout)[:, :, 3]
    solid = numpy.argwhere(alpha > SUBJECT_ALPHA * 255)
    if solid.shape[0] == 0:
        raise ValueError("no subject was found in the photograph")
    left, top = int(solid[:, 1].min()), int(solid[:, 0].min())
    right, bottom = int(solid[:, 1].max()), int(solid[:, 0].max())
    centre = (left + right) / 2, (top + bottom) / 2
    side = int(max(right - left, bottom - top) * FRAME_MARGIN)
    if side < 1:
        raise ValueError("the subject is too small in the photograph")
    box = centre[0] - side // 2, centre[1] - side // 2, centre[0] + side // 2, centre[1] + side // 2
    cropped = numpy.asarray(cutout.crop(box)).astype(numpy.float32) / 255
    framed = cropped[:, :, :3] * cropped[:, :, 3:4]
    image = Image.fromarray((numpy.clip(framed, 0, 1) * 255).astype(numpy.uint8))
    return image, side / cutout.width


def framed_fov(photo_fov: float | None, fraction: float) -> float:
    """The horizontal field of view of the framed square, in radians."""
    if photo_fov is None:
        return math.radians(DEFAULT_FOV_DEGREES)
    low, high = (math.radians(limit) for limit in FOV_RANGE_DEGREES)
    return min(high, max(low, 2.0 * math.atan(fraction * math.tan(photo_fov / 2.0))))


def camera_distance(fov: float) -> float:
    """The camera distance at which the unit cube's width fills the frame."""
    resolution = 512
    focal = float((16.0 / torch.tan(torch.tensor(fov / 2.0))) * resolution / 32.0)
    return focal * -0.5 / (0 - resolution / 2.0) - 0.0


def load_segmenter(code: Path, directory: Path, device: torch.device) -> nn.Module:
    """BiRefNet from the image's pinned code and the operator's weights.

    The pinned code imports kornia's Laplacian filter for its training loss
    only, so a stand-in that refuses to run takes its place.
    """
    if "kornia.filters" not in sys.modules:
        kornia = types.ModuleType("kornia")
        filters = types.ModuleType("kornia.filters")

        def laplacian(*_args, **_kwargs):
            raise RuntimeError("BiRefNet's training loss is not available")

        filters.laplacian = laplacian
        kornia.filters = filters
        sys.modules.setdefault("kornia", kornia)
        sys.modules["kornia.filters"] = filters
    if str(code.parent) not in sys.path:
        sys.path.insert(0, str(code.parent))
    package = code.name
    try:
        settings = importlib.import_module(f"{package}.BiRefNet_config")
        network = importlib.import_module(f"{package}.birefnet")
    except ImportError as error:
        raise ModelError(f"the background removal code could not be loaded: {error}") from error
    with torch.device("meta"):
        model = network.BiRefNet(config=settings.BiRefNetConfig(bb_pretrained=False))
    return load(model, directory / "model.safetensors", device)


def remove_background(image: Image.Image, segmenter: nn.Module, device: torch.device) -> Image.Image:
    """The photograph with BiRefNet's estimate of the subject as its alpha."""
    resized = image.resize((SEGMENTATION_SIDE, SEGMENTATION_SIDE), Image.Resampling.BILINEAR)
    pixels = torch.from_numpy(numpy.asarray(resized, dtype=numpy.float32) / 255).permute(2, 0, 1)
    pixels = _normalize(pixels.unsqueeze(0)).to(device)
    with torch.no_grad():
        mask = segmenter(pixels)[-1].sigmoid()[0, 0].float().cpu()
    mask = Image.fromarray(mask.mul(255).byte().numpy()).resize(image.size, Image.Resampling.BICUBIC)
    cutout = image.convert("RGBA")
    cutout.putalpha(mask)
    return cutout


def _normalize(pixels: torch.Tensor) -> torch.Tensor:
    mean = torch.tensor(IMAGENET_MEAN, device=pixels.device).view(1, 3, 1, 1)
    std = torch.tensor(IMAGENET_STD, device=pixels.device).view(1, 3, 1, 1)
    return (pixels - mean) / std


def pixels_at(image: Image.Image, side: int, device: torch.device) -> torch.Tensor:
    """The framed image resized to side x side, as 1 x 3 x side x side in [0, 1]."""
    resized = numpy.asarray(image.resize((side, side), Image.Resampling.LANCZOS), dtype=numpy.float32) / 255
    return torch.from_numpy(resized).permute(2, 0, 1).unsqueeze(0).to(device)


def load_dinov3(directory: Path, device: torch.device) -> nn.Module:
    """DINOv3 in float32, read straight onto `device`."""
    from transformers import DINOv3ViTConfig, DINOv3ViTModel
    from transformers.models.dinov3_vit.modeling_dinov3_vit import DINOv3ViTRopePositionEmbedding

    try:
        config = DINOv3ViTConfig.from_json_file(str(directory / "config.json"))
    except (OSError, ValueError) as error:
        raise ModelError(f"the DINOv3 configuration could not be read: {error}") from error
    config._attn_implementation = "sdpa"
    with torch.device("meta"):
        model = DINOv3ViTModel(config)
    # The rotary embedding's frequencies are computed, not stored.
    model.rope_embeddings = DINOv3ViTRopePositionEmbedding(config).to(device)
    # Checkpoints name the encoder's layers without the encoder's own prefix.
    model = load(
        model,
        directory / "model.safetensors",
        device,
        rename=lambda name: f"model.{name}" if name.startswith("layer.") else name,
    )
    return model


def dinov3_tokens(model: nn.Module, pixels: torch.Tensor) -> tuple[torch.Tensor, torch.Tensor]:
    """DINOv3's global tokens (class and registers, T x C) and patch features (1 x C x P x P).

    Features are taken before the model's own final norm and normalized
    without its learned scale, as Pixal3D was trained.
    """
    normalized = _normalize(pixels).to(model.embeddings.patch_embeddings.weight.dtype)
    hidden = model.embeddings(normalized, bool_masked_pos=None)
    positions = model.rope_embeddings(normalized)
    for layer in model.model.layer:
        hidden = layer(hidden, position_embeddings=positions)
    hidden = F.layer_norm(hidden, hidden.shape[-1:])
    registers = model.config.num_register_tokens
    side = pixels.shape[-1] // model.config.patch_size
    patches = hidden[0, 1 + registers :].reshape(side, side, -1).permute(2, 0, 1).unsqueeze(0)
    return hidden[0, : 1 + registers], patches


class _EncoderBlock(nn.Module):
    def __init__(self, channels: int, kernel: int, groups: int = 8):
        super().__init__()
        self.norm1 = nn.GroupNorm(groups, channels)
        self.conv1 = nn.Conv2d(channels, channels, kernel, padding=kernel // 2, padding_mode="reflect")
        self.norm2 = nn.GroupNorm(groups, channels)
        self.conv2 = nn.Conv2d(channels, channels, kernel, padding=kernel // 2, padding_mode="reflect")

    def forward(self, x: torch.Tensor) -> torch.Tensor:
        return self.conv2(F.silu(self.norm2(self.conv1(F.silu(self.norm1(x))))))


def _encoder(channels: int, kernel: int, layers: int = 2) -> nn.Sequential:
    return nn.Sequential(
        nn.Conv2d(3, channels, kernel, padding=kernel // 2, padding_mode="reflect"),
        *(_EncoderBlock(channels, kernel) for _ in range(layers)),
    )


class _ImageEncoder(nn.Module):
    def __init__(self, channels: int, rope_heads: int):
        super().__init__()
        self.encoder = _encoder(channels // 2, 1)
        self.sem_encoder = _encoder(channels // 2, 3)
        self.rope = nn.Module()
        self.rope.register_buffer("periods", torch.zeros(channels // rope_heads // 4))
        self.rope_heads = rope_heads


class NAF(nn.Module):
    """NAF, which upsamples feature maps by attending from each output pixel
    to the low-resolution features around it, with keys and queries drawn
    from the guiding photograph.

    The attention covers the kernel x kernel low-resolution features nearest
    each output pixel, as NATTEN's dilated neighbourhood attention does over
    features enlarged to the output size by nearest-neighbour sampling.
    """

    def __init__(self, channels: int = 256, heads: int = 4, rope_heads: int = 4, kernel: int = 9):
        super().__init__()
        self.heads = heads
        self.kernel = kernel
        self.image_encoder = _ImageEncoder(channels, rope_heads)

    def _guide(self, image: torch.Tensor, size: int) -> torch.Tensor:
        if image.shape[-2] > 4 * size or image.shape[-1] > 4 * size:
            image = F.interpolate(
                image, size=(min(image.shape[-2], 4 * size), min(image.shape[-1], 4 * size)), mode="bilinear"
            )
        encoder = self.image_encoder
        x = torch.cat([encoder.encoder(image), encoder.sem_encoder(image)], dim=1)
        x = F.adaptive_avg_pool2d(x, output_size=(size, size))
        return self._rotate(x)

    def _rotate(self, x: torch.Tensor) -> torch.Tensor:
        periods = self.image_encoder.rope.periods
        heads = self.image_encoder.rope_heads
        batch, channels, height, width = x.shape
        coords_h = torch.arange(0.5, height, device=x.device) / height
        coords_w = torch.arange(0.5, width, device=x.device) / width
        coords = torch.stack(torch.meshgrid(coords_h, coords_w, indexing="ij"), dim=-1).flatten(0, 1) * 2.0 - 1.0
        angles = (2 * math.pi * coords[:, :, None] / periods[None, None, :]).flatten(1, 2).tile(2)
        flat = x.reshape(batch, heads, channels // heads, height * width).transpose(2, 3)
        first, second = flat.chunk(2, dim=-1)
        rotated = flat * torch.cos(angles) + torch.cat([-second, first], dim=-1) * torch.sin(angles)
        return rotated.transpose(2, 3).reshape(batch, channels, height, width)

    def forward(self, image: torch.Tensor, features: torch.Tensor, size: int) -> torch.Tensor:
        """`features` (1 x C x P x P) upsampled to 1 x C x size x size."""
        low = features.shape[-1]
        if size % low or low < self.kernel:
            raise ValueError("the upsampled size must be a multiple of the feature map's, at least the kernel")
        queries = self._guide(image, size)
        keys = F.adaptive_avg_pool2d(queries, output_size=features.shape[-2:])
        return _neighbourhood_attention(queries, keys, features, self.heads, self.kernel)


def _neighbourhood_attention(
    queries: torch.Tensor, keys: torch.Tensor, values: torch.Tensor, heads: int, kernel: int
) -> torch.Tensor:
    size, low = queries.shape[-1], keys.shape[-1]
    cell = size // low
    q = queries[0].reshape(heads, -1, size, size)
    k = keys[0].reshape(heads, -1, low, low)
    v = values[0].reshape(heads, -1, low, low)
    scale = q.shape[1] ** -0.5
    start = (torch.arange(low, device=q.device) - kernel // 2).clamp(0, low - kernel)
    window = start[:, None] + torch.arange(kernel, device=q.device)  # low x kernel
    out = torch.empty(heads, v.shape[1], size, size, dtype=values.dtype, device=values.device)
    for row in range(low):
        rows = window[row]
        # Each output pixel in this row of cells attends to the same kernel x
        # kernel features: heads x cells x kernel^2 x dim.
        kw = k[:, :, rows][:, :, :, window].permute(0, 3, 2, 4, 1).reshape(heads, low, kernel * kernel, -1)
        vw = v[:, :, rows][:, :, :, window].permute(0, 3, 2, 4, 1).reshape(heads, low, kernel * kernel, -1)
        qr = q[:, :, row * cell : (row + 1) * cell].reshape(heads, -1, cell, low, cell)
        qr = qr.permute(0, 3, 2, 4, 1).reshape(heads, low, cell * cell, -1)
        weights = (qr.float() @ kw.float().transpose(-1, -2) * scale).softmax(dim=-1)
        result = (weights @ vw.float()).to(values.dtype)  # heads x cells x cell^2 x dim
        result = result.reshape(heads, low, cell, cell, -1).permute(0, 4, 2, 1, 3)
        out[:, :, row * cell : (row + 1) * cell] = result.reshape(heads, -1, cell, size)
    return out.reshape(1, -1, size, size)


def load_naf(path: Path, device: torch.device) -> NAF:
    """NAF with the width its weights give."""
    try:
        weights = torch.load(path, map_location="cpu", weights_only=True)
        channels = int(weights["image_encoder.encoder.0.weight"].shape[0]) * 2
    except (OSError, RuntimeError, ValueError, KeyError, TypeError, AttributeError, IndexError) as error:
        raise ModelError(f"{path.name} does not hold the NAF upsampler's weights: {error}") from error
    if channels % 16:
        raise ModelError(f"{path.name} gives the NAF upsampler a width it cannot have")
    model = NAF(channels=channels)
    if set(weights) != set(model.state_dict()):
        raise ModelError(f"{path.name} does not hold the NAF upsampler's weights")
    try:
        model.load_state_dict(weights)
    except RuntimeError as error:
        raise ModelError(f"{path.name} does not match the NAF upsampler: {error}") from error
    return model.to(device).requires_grad_(False).eval()


# Rotates the cube's grid into the camera's world, z up, and back.
_GRID_TO_WORLD = torch.tensor([[1.0, 0.0, 0.0], [0.0, 0.0, -1.0], [0.0, 1.0, 0.0]])


def project(coords: torch.Tensor, grid: int, fov: float, distance: float, side: int) -> torch.Tensor:
    """Where the centres of voxels `coords` (N x 3) of a grid^3 cube fall in a
    side x side image, as N x 2 sampling positions in [-1, 1].

    The cube spans -0.5 to 0.5 with its voxel centres from edge to edge; the
    camera sits `distance` in front of it and sees `fov` radians across.
    """
    device = coords.device
    # The grid is laid out on the processor, as Pixal3D's training did.
    axis = torch.linspace(-1, 1, grid).to(device)
    points = torch.stack([axis[coords[:, 0].long()], axis[coords[:, 1].long()], axis[coords[:, 2].long()]], dim=-1)
    points = torch.matmul(points, _GRID_TO_WORLD.to(device).T) / 1.0 / 2
    camera = torch.tensor(
        [[1.0, 0.0, 0.0, 0.0], [0.0, 0.0, -1.0, -distance], [0.0, 1.0, 0.0, 0.0], [0.0, 0.0, 0.0, 1.0]], device=device
    )
    homogeneous = torch.cat([points, torch.ones(points.shape[0], 1, device=device)], dim=-1)
    viewed = (homogeneous @ torch.linalg.inv(camera).T)[:, :3]
    focal = 16.0 / torch.tan(torch.tensor([fov], device=device) / 2.0) * side / 32.0
    x = focal * viewed[:, 0] / (-viewed[:, 2] + 1e-8) + side / 2.0
    y = -(focal * viewed[:, 1] / (-viewed[:, 2] + 1e-8)) + side / 2.0
    return (torch.stack([x, y], dim=-1) + 0.5) / side * 2 - 1


def sample(features: torch.Tensor, positions: torch.Tensor) -> torch.Tensor:
    """Bilinear samples of 1 x C x H x W features at N x 2 positions, as N x C."""
    grid = positions.view(1, -1, 1, 2).to(features.dtype)
    sampled = F.grid_sample(features, grid, mode="bilinear", align_corners=False, padding_mode="border")
    return sampled[0, :, :, 0].t()
