"""The operator-supplied files the worker needs, without heavy imports."""

import json
from pathlib import Path, PurePosixPath

# Files expected in the operator's checkpoint directory.
FILES = {
    "mica": "mica.tar",
    "smirk": "SMIRK_em1.pt",
    "deca": "deca_model.tar",
    "face_eye_mask": "uv_face_eye_mask.png",
}


def missing_files(directory: Path) -> list[str]:
    """Checkpoint files absent from `directory`."""
    return [name for name in FILES.values() if not (directory / name).is_file()]


# A Marigold V2 directory written by convert_marigold.py.
MARIGOLD_FORMAT = "printable-marigold-v2 1"
MARIGOLD_TASKS = ("depth", "normals")
MARIGOLD_FILES = (
    "transformer/config.json",
    "transformer/base.safetensors",
    "vae/config.json",
    "vae/diffusion_pytorch_model.safetensors",
)


def block_file(index: int) -> str:
    """The file holding one block of the base transformer."""
    return f"transformer/block-{index:03d}.safetensors"


def task_file(task: str) -> str:
    """The file holding one task's adapter, VAE decoder and prompt."""
    return f"{task}.safetensors"


def missing_marigold_files(directory: Path) -> list[str]:
    """Files of a converted Marigold V2 directory that are absent."""
    try:
        manifest = json.loads((directory / "manifest.json").read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return ["manifest.json"]
    if not isinstance(manifest, dict):
        return [f"manifest.json in format {MARIGOLD_FORMAT}"]
    blocks = manifest.get("blocks")
    if (
        manifest.get("format") != MARIGOLD_FORMAT
        or not isinstance(blocks, int)
        or isinstance(blocks, bool)
        or blocks < 1
    ):
        return [f"manifest.json in format {MARIGOLD_FORMAT}"]
    names = [
        *MARIGOLD_FILES,
        *(block_file(index) for index in range(blocks)),
        *(task_file(task) for task in MARIGOLD_TASKS),
    ]
    return [name for name in names if not (directory / name).is_file()]


# A Pixal3D directory: the published pipeline description and the shape
# checkpoints it names, with the image models the stages use beside them.
PIXAL3D_PIPELINE = "pipeline.json"
PIXAL3D_STAGES = (
    "sparse_structure_flow_model",
    "sparse_structure_decoder",
    "shape_slat_flow_model_512",
    "shape_slat_flow_model_1024",
    "shape_slat_decoder",
)
PIXAL3D_FILES = (
    "dinov3/config.json",
    "dinov3/model.safetensors",
    "naf/naf_release.pth",
    "birefnet/model.safetensors",
)


def pixal3d_checkpoints(directory: Path) -> dict[str, str]:
    """Each shape stage's checkpoint, as a path without extension inside `directory`.

    Raises ValueError when the pipeline description is missing, malformed or
    names a checkpoint outside the directory.
    """
    try:
        pipeline = json.loads((directory / PIXAL3D_PIPELINE).read_text(encoding="utf-8"))
        models = pipeline["args"]["models"]
    except (OSError, ValueError, KeyError, TypeError) as error:
        raise ValueError(f"{PIXAL3D_PIPELINE} could not be read: {error}") from error
    if not isinstance(models, dict):
        raise ValueError(f"{PIXAL3D_PIPELINE} does not list the models")
    checkpoints = {}
    for stage in PIXAL3D_STAGES:
        name = models.get(stage)
        if not isinstance(name, str) or not name:
            raise ValueError(f"{PIXAL3D_PIPELINE} names no {stage}")
        parts = PurePosixPath(name).parts
        if PurePosixPath(name).is_absolute() or any(part in ("", ".", "..") for part in parts) or "\\" in name:
            raise ValueError(f"{PIXAL3D_PIPELINE} names {stage} outside the model directory")
        checkpoints[stage] = name
    return checkpoints


def missing_object_files(directory: Path) -> list[str]:
    """Files of a Pixal3D directory that are absent."""
    try:
        checkpoints = pixal3d_checkpoints(directory)
    except ValueError:
        return [PIXAL3D_PIPELINE]
    names = [f"{base}{suffix}" for base in checkpoints.values() for suffix in (".json", ".safetensors")]
    names.extend(PIXAL3D_FILES)
    return [name for name in names if not (directory / name).is_file()]
