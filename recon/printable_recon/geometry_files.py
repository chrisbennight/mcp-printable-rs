"""The pinned files of each photograph geometry model, from models/geometry.json.

Each model names what it outputs and its files: a Hub repository at a commit
or an HTTPS URL, each with SHA-256 and size. Marigold V2 is listed by the
inputs of its conversion.
"""

from __future__ import annotations

from dataclasses import dataclass
import json
from pathlib import Path

from .model_cache import Manifest, ModelFile

FORMAT = "printable-geometry-models 1"
OUTPUTS = {"depth": ("metric", "relative"), "focal": ("pixels",), "mask": ("valid_pixels",), "normals": ("camera",)}
CONVERSIONS = ("marigold-v2",)


class GeometryFilesError(ValueError):
    """The model list is malformed."""


@dataclass(frozen=True)
class Model:
    name: str
    model: str
    outputs: dict[str, str]
    manifest: Manifest
    conversion: str | None = None


def read(path: Path) -> dict[str, Model]:
    return parse(json.loads(path.read_text(encoding="utf-8")))


def parse(document: object) -> dict[str, Model]:
    if not isinstance(document, dict) or document.get("format") != FORMAT:
        raise GeometryFilesError("unsupported geometry model list format")
    models = document.get("models")
    if not isinstance(models, dict) or not models:
        raise GeometryFilesError("the model list needs a non-empty models object")
    return {name: _model(name, value) for name, value in models.items()}


def _model(name: str, value: object) -> Model:
    if not isinstance(value, dict):
        raise GeometryFilesError(f"model {name} must be an object")
    outputs = value.get("outputs")
    if not isinstance(outputs, dict) or not outputs:
        raise GeometryFilesError(f"model {name} must declare its outputs")
    for output, convention in outputs.items():
        if convention not in OUTPUTS.get(output, ()):
            raise GeometryFilesError(f"model {name} has an unknown {output} output {convention!r}")
    conversion = value.get("conversion")
    if conversion is not None and conversion not in CONVERSIONS:
        raise GeometryFilesError(f"model {name} has an unknown conversion {conversion!r}")
    files = value.get("files")
    if not isinstance(files, list):
        raise GeometryFilesError(f"model {name} must list its files")
    try:
        manifest = Manifest(tuple(ModelFile.from_document(item) for item in files))
    except (TypeError, ValueError) as error:
        raise GeometryFilesError(f"model {name} has an invalid file: {error}") from None
    label = value.get("model")
    if not isinstance(label, str) or not label:
        raise GeometryFilesError(f"model {name} must name its model")
    return Model(name, label, dict(outputs), manifest, conversion)
