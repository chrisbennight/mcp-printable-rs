"""Prepare and report the photograph geometry models.

Usage: prepare_geometry.py prepare <request.json>
       prepare_geometry.py --status <cache>
       prepare_geometry.py --version

prepare reads {"cache": path, "models": [names]} and makes a verified
snapshot of each named model from models/geometry.json in the cache,
downloading pinned files that are missing. A model that needs conversion is
converted once into <cache>/converted/<manifest identifier>; that directory
appears only when the conversion is complete.

--status reports the last preparation attempt of every model without
rereading model files or loading model code, so the worker can call it for
each readiness check.

One JSON object is printed. prepare prints {"result": ...} or
{"error": {"code", "message"}}.
"""

from contextlib import contextmanager, redirect_stderr, redirect_stdout
import errno
import fcntl
import json
import os
from pathlib import Path
import shutil
import sys

ROOT = Path(__file__).resolve().parent
MODELS = ROOT / "models/geometry.json"
VERSION = "printable-geometry-prepare 1"
# The Marigold V2 conversion reads these two downloads from the model's snapshot.
MARIGOLD_SOURCES = ("qwen-image-edit-2509", "marigold-v2-0")


class RequestError(ValueError):
    """The request does not have the documented shape."""


def converted_path(cache: Path, model) -> Path:
    """Where the converted form of `model` lives once complete."""
    from printable_recon.model_cache import _inside

    return _inside(cache, f"converted/{model.manifest.identifier}")


@contextmanager
def _conversion_lock(cache: Path):
    from printable_recon.model_cache import _inside

    with _inside(cache, "convert.lock").open("a+b") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        try:
            yield
        finally:
            fcntl.flock(lock, fcntl.LOCK_UN)


def _convert(cache: Path, model, snapshot: Path) -> Path:
    output = converted_path(cache, model)
    # Conversion has its own lock, so a long conversion does not hold up
    # downloads of other models, and two workers never convert at once.
    with _conversion_lock(cache):
        if output.is_dir():
            return output
        # Holding the lock, a partial directory can only be left by a
        # conversion that stopped; the converter refuses to start beside one.
        partial = output.with_name(f"{output.name}.partial")
        if partial.exists():
            shutil.rmtree(partial)
        output.parent.mkdir(parents=True, exist_ok=True)
        from convert_marigold import convert

        convert(*(snapshot / name for name in MARIGOLD_SOURCES), output)
    return output


def prepare(request: object, models: dict, **options) -> dict:
    """Prepare each named model; `options` pass through to model_cache.prepare."""
    from printable_recon.model_cache import prepare as prepare_cache

    if not isinstance(request, dict) or not isinstance(request.get("cache"), str) or not request["cache"]:
        raise RequestError("the request needs a cache directory")
    names = request.get("models")
    if not isinstance(names, list) or not all(isinstance(name, str) for name in names):
        raise RequestError("models must be a list of geometry model names")
    unknown = sorted({name for name in names if name not in models})
    if unknown:
        raise RequestError(f"unknown geometry models: {', '.join(unknown)}")
    cache = Path(request["cache"])
    prepared = {}
    for name in dict.fromkeys(names):
        model = models[name]
        snapshot = prepare_cache(cache, model.manifest, **options)
        prepared[name] = {"snapshot": str(snapshot)}
        if model.conversion == "marigold-v2":
            prepared[name]["converted"] = str(_convert(cache, model, snapshot))
    return {"models": prepared}


def status(cache: Path, models: dict) -> dict:
    """The last preparation attempt of each model.

    A model that needs conversion counts as prepared only once its converted
    form is complete, since the worker cannot run it before then.
    """
    from printable_recon.model_cache import PreparationError, preparation_status

    states = {}
    for name, model in models.items():
        state = preparation_status(cache, model.manifest)
        if model.conversion is not None and state["state"] == "prepared":
            try:
                complete = converted_path(cache, model).is_dir()
            except PreparationError:
                complete = False
            if not complete:
                state = {"state": "not_prepared", "last_verified_unix_ms": None}
        states[name] = state
    return {"models": states}


def _answer(code: str, message: str) -> int:
    print(json.dumps({"error": {"code": code, "message": message}}))
    return 2


def main(argv: list[str]) -> int:
    if argv == ["--version"]:
        print(VERSION)
        return 0
    sys.path.insert(0, str(ROOT))
    from convert_marigold import ConversionError
    from printable_recon import geometry_files
    from printable_recon.model_cache import PreparationError

    if len(argv) != 2 or argv[0] not in ("prepare", "--status"):
        return _answer("validation", "expected prepare <request> or --status <cache>")
    try:
        models = geometry_files.read(MODELS)
    except (OSError, ValueError):
        return _answer("unavailable", "the geometry model list could not be read")
    if argv[0] == "--status":
        print(json.dumps(status(Path(argv[1]), models)))
        return 0
    try:
        request = json.loads(Path(argv[1]).read_text(encoding="utf-8"))
        # Upstream libraries may print progress or endpoint details; only
        # the final answer goes to stdout.
        with open(os.devnull, "w") as diagnostics, redirect_stdout(diagnostics), redirect_stderr(diagnostics):
            result = prepare(request, models)
    except (RequestError, json.JSONDecodeError) as error:
        return _answer("validation", f"the request is invalid: {error}")
    except (PreparationError, ConversionError) as error:
        return _answer("checkpoints", str(error))
    except OSError as error:
        if error.errno in (errno.ENOSPC, errno.EDQUOT):
            return _answer("resource_limit", "insufficient disk space for the geometry models")
        return _answer("checkpoints", "the geometry models could not be prepared; check the cache directory")
    print(json.dumps({"result": result}))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
