"""Trusted command-line entrypoint for a disposable native preparation process."""

import json
from pathlib import Path
import sys

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from printable_bridge.project_packing import ProjectPackingError
from printable_bridge.project_preparation import prepare_staged_project


def run():
    import bpy

    arguments = sys.argv[sys.argv.index("--") + 1:]
    if len(arguments) != 2:
        raise ValueError("native preparation requires private request and result paths")
    request_path, result_path = map(Path, arguments)
    request = json.loads(request_path.read_text(encoding="utf-8"))
    try:
        result = prepare_staged_project(bpy, Path(request["original_root"]),
                                        Path(request["staged_root"]), request["files"],
                                        request["entrypoint"])
    except ProjectPackingError as error:
        # These messages are fixed application text, never native diagnostics
        # containing absolute paths or the contents of imported files.
        result = {"error": str(error)}
    result_path.write_text(json.dumps(result, allow_nan=False), encoding="utf-8")


if __name__ == "__main__":
    run()
