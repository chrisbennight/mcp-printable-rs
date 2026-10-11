"""Run one FLAME reconstruction for the reconstruction worker.

Usage: reconstruct.py <request.json>
       reconstruct.py --check <checkpoint-directory>
       reconstruct.py --version

The request names the input images, the checkpoint directory, the face
landmarker model, the displacement map to write and the device. One JSON
object is printed: {"result": ...} or {"error": {"code", "message"}}.
"""

import json
import os
import sys
from pathlib import Path

# Deterministic CUDA matrix products need this before CUDA starts.
os.environ.setdefault("CUBLAS_WORKSPACE_CONFIG", ":4096:8")

VERSION = "printable-recon 1"


def main(argv: list[str]) -> int:
    if argv == ["--version"]:
        print(VERSION)
        return 0
    sys.path.insert(0, str(Path(__file__).resolve().parent))
    if len(argv) == 2 and argv[0] == "--check":
        from printable_recon.files import missing_files

        print(json.dumps({"missing": missing_files(Path(argv[1]))}))
        return 0
    from printable_recon import checkpoints, faces, pipeline

    if len(argv) != 1:
        return _fail("validation", "expected one request file")
    try:
        request = json.loads(Path(argv[0]).read_text(encoding="utf-8"))
        images = [Path(path) for path in request["images"]]
        directory = Path(request["checkpoints"])
        landmarker = Path(request["landmarker"])
        output = Path(request["displacement"])
        device = request.get("device", "auto")
        seed = int(request.get("seed", 0))
    except (OSError, ValueError, KeyError, TypeError) as error:
        return _fail("validation", f"the request is invalid: {error}")

    import torch

    if device == "auto":
        device = "cuda" if torch.cuda.is_available() else "cpu"
    torch.manual_seed(seed)
    torch.use_deterministic_algorithms(True)
    try:
        reconstructor = pipeline.Reconstructor(directory, landmarker, device)
        try:
            result = reconstructor.run(images)
        finally:
            reconstructor.close()
        displacement = pipeline.write_displacement(result.displacement_mm, output)
    except checkpoints.CheckpointError as error:
        return _fail("checkpoints", str(error))
    except faces.FaceError as error:
        return _fail("no_face", str(error))
    print(
        json.dumps(
            {
                "result": {
                    "variant_family": "flame2020",
                    "identity": result.identity,
                    "expression": result.expression,
                    "jaw": result.jaw,
                    "head_rotation": result.head_rotation,
                    "identity_agreement": result.identity_agreement,
                    "clamped_coefficients": result.clamped_coefficients,
                    "faces": result.faces,
                    "displacement": displacement,
                    "device": device,
                }
            }
        )
    )
    return 0


def _fail(code: str, message: str) -> int:
    print(json.dumps({"error": {"code": code, "message": message}}))
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
