"""Choose, prepare and report the photograph geometry models.

Usage: prepare_geometry.py --select <request.json>
       prepare_geometry.py prepare <request.json>
       prepare_geometry.py --status <cache>
       prepare_geometry.py --version

--select reads {"gpu_budget_mib": int | null, "ram_budget_mib": int | null}
and reports, for every analysis role, the registry option this worker would
run, or why none fits. It downloads nothing and loads no model code, so the
worker can call it for each readiness check.

prepare reads {"cache": path, "entries": [names]} and makes a verified
snapshot of each named registry entry in the cache, downloading pinned files
that are missing. An entry that needs conversion is converted once into
<cache>/converted/<manifest identifier>; that directory appears only when the
conversion is complete.

--status reports the last preparation attempt of every registry entry without
rereading model files.

One JSON object is printed. --select and prepare print {"result": ...} or
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
REGISTRY = ROOT / "models/geometry.json"
VERSION = "printable-geometry-prepare 1"
# The Marigold V2 conversion reads these two downloads from the entry's snapshot.
MARIGOLD_SOURCES = ("qwen-image-edit-2509", "marigold-v2-0")


class RequestError(ValueError):
    """The request does not have the documented shape."""


def _budget(request: dict, key: str) -> int | None:
    value = request.get(key)
    if value is None:
        return None
    if isinstance(value, bool) or not isinstance(value, int) or value < 1:
        raise RequestError(f"{key} must be a positive integer or null")
    return value


def select(request: object, registry) -> dict:
    """The route each role would take on a host with these budgets."""
    from printable_recon.registry import ROLES, Host, NoRoute

    if not isinstance(request, dict):
        raise RequestError("the request must be an object")
    host = Host(_budget(request, "gpu_budget_mib"), _budget(request, "ram_budget_mib"))
    roles = {}
    for role in ROLES:
        try:
            chosen = registry.select(role, host)
        except NoRoute as error:
            roles[role] = {"error": str(error)}
            continue
        roles[role] = {"entry": chosen.option.entry, "device": chosen.option.device, "side": chosen.option.side,
                       "fallback": chosen.fallback, "rejected": list(chosen.rejected)}
    return {"roles": roles}


def converted_path(cache: Path, entry) -> Path:
    """Where the converted form of `entry` lives once complete."""
    from printable_recon.model_cache import _inside

    return _inside(cache, f"converted/{entry.manifest.identifier}")


@contextmanager
def _conversion_lock(cache: Path):
    from printable_recon.model_cache import _inside

    with _inside(cache, "convert.lock").open("a+b") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        try:
            yield
        finally:
            fcntl.flock(lock, fcntl.LOCK_UN)


def _convert(cache: Path, entry, snapshot: Path) -> Path:
    output = converted_path(cache, entry)
    # Conversion has its own lock, so a long conversion does not hold up
    # downloads of other entries, and two workers never convert at once.
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


def prepare(request: object, registry, **options) -> dict:
    """Prepare each named entry; `options` pass through to model_cache.prepare."""
    from printable_recon.model_cache import prepare as prepare_cache

    if not isinstance(request, dict) or not isinstance(request.get("cache"), str) or not request["cache"]:
        raise RequestError("the request needs a cache directory")
    names = request.get("entries")
    if not isinstance(names, list) or not all(isinstance(name, str) for name in names):
        raise RequestError("entries must be a list of registry entry names")
    unknown = sorted({name for name in names if name not in registry.entries})
    if unknown:
        raise RequestError(f"unknown registry entries: {', '.join(unknown)}")
    cache = Path(request["cache"])
    prepared = {}
    for name in dict.fromkeys(names):
        entry = registry.entries[name]
        snapshot = prepare_cache(cache, entry.manifest, **options)
        prepared[name] = {"snapshot": str(snapshot)}
        if entry.conversion == "marigold-v2":
            prepared[name]["converted"] = str(_convert(cache, entry, snapshot))
    return {"entries": prepared}


def status(cache: Path, registry) -> dict:
    """The last preparation attempt of each entry.

    An entry that needs conversion counts as prepared only once its converted
    model is complete, since the worker cannot run it before then.
    """
    from printable_recon.model_cache import PreparationError, preparation_status

    entries = {}
    for name, entry in registry.entries.items():
        state = preparation_status(cache, entry.manifest)
        if entry.conversion is not None and state["state"] == "prepared":
            try:
                complete = converted_path(cache, entry).is_dir()
            except PreparationError:
                complete = False
            if not complete:
                state = {"state": "not_prepared", "last_verified_unix_ms": None}
        entries[name] = state
    return {"entries": entries}


def _answer(code: str, message: str) -> int:
    print(json.dumps({"error": {"code": code, "message": message}}))
    return 2


def main(argv: list[str]) -> int:
    if argv == ["--version"]:
        print(VERSION)
        return 0
    sys.path.insert(0, str(ROOT))
    from convert_marigold import ConversionError
    from printable_recon.model_cache import PreparationError
    from printable_recon.registry import Registry, RegistryError

    if len(argv) != 2 or argv[0] not in ("--select", "prepare", "--status"):
        return _answer("validation", "expected --select <request>, prepare <request> or --status <cache>")
    try:
        registry = Registry.read(REGISTRY)
    except (OSError, ValueError, RegistryError):
        return _answer("unavailable", "the geometry model registry could not be read")
    if argv[0] == "--status":
        print(json.dumps(status(Path(argv[1]), registry)))
        return 0
    try:
        request = json.loads(Path(argv[1]).read_text(encoding="utf-8"))
        if argv[0] == "--select":
            result = select(request, registry)
        else:
            # Upstream libraries may print progress or endpoint details; only
            # the final answer goes to stdout.
            with open(os.devnull, "w") as diagnostics, redirect_stdout(diagnostics), redirect_stderr(diagnostics):
                result = prepare(request, registry)
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
