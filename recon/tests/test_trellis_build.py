"""Native-build contracts without compilers, GPU access or downloads."""

import ast
import importlib.util
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("build_trellis", Path(__file__).resolve().parents[1] / "build_trellis.py")
builder = importlib.util.module_from_spec(spec)
spec.loader.exec_module(builder)


class BuildTests(unittest.TestCase):
    def test_cpu_only_images_keep_the_other_photograph_workflows_without_cuda_extensions(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            with patch.object(builder.torch.version, "cuda", None), patch.object(builder.subprocess, "run", side_effect=AssertionError("CPU build invoked a CUDA compiler")):
                builder.build(root / "absent-source", root)
            self.assertEqual((root / "requirements.txt").read_bytes(), b"")

    def test_compiler_flag_update_keeps_the_setup_program_valid(self):
        tree = ast.parse('flags = {"cxx": ["-O3", "-std=c++17"], "nvcc": ["-std=c++17", "--use_fast_math"]}')
        modified = ast.fix_missing_locations(builder.CompilerStandard().visit(tree))
        compile(modified, "setup.py", "exec")
        flags = ast.literal_eval(modified.body[0].value)
        self.assertEqual(flags, {"cxx": ["-O3", "-std=c++20"], "nvcc": ["-std=c++20", "--use_fast_math"]})


if __name__ == "__main__":
    unittest.main()
