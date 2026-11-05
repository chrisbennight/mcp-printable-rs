"""Build the pinned shape extensions against the reconstruction image's PyTorch."""

import ast
from pathlib import Path
import subprocess
import sys

import torch


class CompilerStandard(ast.NodeTransformer):
    def visit_Constant(self, node):
        if isinstance(node.value, str) and "c++17" in node.value:
            return ast.copy_location(ast.Constant(node.value.replace("c++17", "c++20")), node)
        return node


def build(root: Path, wheels: Path):
    destination = wheels / "requirements.txt"
    # Preserve the documented CPU-only image build for the other photograph
    # workflows. TRELLIS itself checks for CUDA before loading its extensions.
    if torch.version.cuda is None:
        destination.write_text("", encoding="utf-8")
        return
    for directory in (root / "flexgemm", root / "trellis2/o-voxel"):
        setup = directory / "setup.py"
        tree = CompilerStandard().visit(ast.parse(setup.read_text(encoding="utf-8")))
        setup.write_text(ast.unparse(ast.fix_missing_locations(tree)) + "\n", encoding="utf-8")
        subprocess.run(
            [sys.executable, "-m", "pip", "wheel", "--no-build-isolation", "--no-deps", "--wheel-dir", str(wheels), "."],
            cwd=directory, check=True,
        )
    destination.write_text("".join(str(path) + "\n" for path in sorted(wheels.glob("*.whl"))), encoding="utf-8")


if __name__ == "__main__":
    build(Path(sys.argv[1]), Path("/wheels"))
