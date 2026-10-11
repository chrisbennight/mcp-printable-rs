"""The operator-supplied files reconstruction needs, without heavy imports."""

from pathlib import Path

# Files expected in the operator's checkpoint directory.
FILES = {
    "mica": "mica.tar",
    "smirk": "SMIRK_em1.pt",
    "deca": "deca_model.tar",
    "fixed_displacement": "fixed_displacement_256.npy",
    "face_eye_mask": "uv_face_eye_mask.png",
}


def missing_files(directory: Path) -> list[str]:
    """Checkpoint files absent from `directory`."""
    return [name for name in FILES.values() if not (directory / name).is_file()]
