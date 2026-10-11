"""Load operator-supplied network checkpoints without executing their code.

Checkpoints are pickled PyTorch files. They are opened with PyTorch's
weights-only unpickler, which builds tensors and plain containers only.
Two of the published checkpoints also hold NumPy arrays, so the NumPy array
reconstruction functions are the only additions to that allowlist.
"""

from __future__ import annotations

from pathlib import Path
from typing import Any

import numpy
import torch
from numpy._core import multiarray

# NumPy arrays saved by older NumPy releases name these functions under
# numpy.core; the pairs map those names to the current objects.
_NUMPY_GLOBALS = [
    (multiarray._reconstruct, "numpy.core.multiarray._reconstruct"),
    (multiarray.scalar, "numpy.core.multiarray.scalar"),
    (numpy.ndarray, "numpy.ndarray"),
    (numpy.dtype, "numpy.dtype"),
    *(type(numpy.dtype(name)) for name in ("float32", "float64", "int32", "int64", "uint8", "bool")),
]


class CheckpointError(Exception):
    """A checkpoint is missing, unreadable, or not shaped as expected."""


def load(path: Path) -> dict[str, Any]:
    """Return the top-level dictionary of a checkpoint file."""
    if not path.is_file():
        raise CheckpointError(f"checkpoint {path.name} is missing")
    try:
        with torch.serialization.safe_globals(_NUMPY_GLOBALS):
            data = torch.load(path, map_location="cpu", weights_only=True)
    except Exception as error:  # the unpickler raises many unrelated types
        raise CheckpointError(f"checkpoint {path.name} could not be read: {error}") from error
    if not isinstance(data, dict):
        raise CheckpointError(f"checkpoint {path.name} does not hold a dictionary")
    return data


def section(data: dict[str, Any], prefix: str, name: str) -> dict[str, torch.Tensor]:
    """Tensors stored under `prefix`, either nested or as dotted keys."""
    nested = data.get(prefix)
    if isinstance(nested, dict):
        items = nested.items()
    else:
        start = f"{prefix}."
        items = ((key[len(start):], value) for key, value in data.items() if key.startswith(start))
    tensors = {key: value for key, value in items if torch.is_tensor(value)}
    if not tensors:
        raise CheckpointError(f"checkpoint {name} has no {prefix} weights")
    return tensors


def apply(module: torch.nn.Module, weights: dict[str, torch.Tensor], name: str) -> None:
    """Load weights that must match the module exactly."""
    try:
        result = module.load_state_dict(weights, strict=False)
    except RuntimeError as error:
        # Raised when a named tensor has the wrong shape.
        raise CheckpointError(f"checkpoint {name} does not match the network: {error}") from error
    missing = [key for key in result.missing_keys if not key.endswith("num_batches_tracked")]
    unexpected = [key for key in result.unexpected_keys if not key.endswith("num_batches_tracked")]
    if missing or unexpected:
        raise CheckpointError(
            f"checkpoint {name} does not match the network: "
            f"{len(missing)} missing and {len(unexpected)} unexpected weights"
        )
