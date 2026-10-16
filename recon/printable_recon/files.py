"""The operator-supplied files the worker needs, without heavy imports."""

import json
from pathlib import Path

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
