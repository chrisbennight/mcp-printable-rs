"""The photograph geometry model registry and the policy that picks a route.

The registry (models/geometry.json) names each model with its pinned files
and, for every analysis role, an ordered list of options: a model, a device
and an input size, best measured quality first, with the cost measured for
that option. Selection walks the list and takes the first option the host can
run within its GPU and RAM budgets. Anything after the first option is a
fallback and is reported as one; when nothing fits, selection refuses with the
reason each option was rejected.
"""

from __future__ import annotations

from dataclasses import dataclass
import json
from pathlib import Path

from .model_cache import Manifest, ModelFile

FORMAT = "printable-geometry 1"
ROLES = ("depth.shallow", "depth.deep", "mask", "focal", "normals")
DEVICES = ("cuda", "cpu")
OUTPUTS = {"depth": ("metric", "relative"), "focal": ("pixels",), "mask": ("valid_pixels",), "normals": ("camera",)}
CONVERSIONS = ("marigold-v2",)
SIDES = (1024, 2048)


class RegistryError(ValueError):
    """The registry document is malformed."""


class NoRoute(Exception):
    """No option for a role can run on this host."""


@dataclass(frozen=True)
class Entry:
    name: str
    model: str
    outputs: dict[str, str]
    manifest: Manifest
    conversion: str | None = None


@dataclass(frozen=True)
class Option:
    entry: str
    device: str
    side: int
    seconds: float
    gpu_peak_mib: int | None
    ram_peak_mib: int


@dataclass(frozen=True)
class Host:
    """What the worker may use: a GPU budget (None when no GPU may be used) and, optionally, a RAM budget."""

    gpu_budget_mib: int | None
    ram_budget_mib: int | None = None


@dataclass(frozen=True)
class Selection:
    role: str
    option: Option
    rank: int
    rejected: tuple[str, ...]

    @property
    def fallback(self) -> bool:
        return self.rank > 0


@dataclass(frozen=True)
class Registry:
    entries: dict[str, Entry]
    routes: dict[str, tuple[Option, ...]]

    @classmethod
    def read(cls, path: Path) -> Registry:
        return cls.parse(json.loads(path.read_text(encoding="utf-8")))

    @classmethod
    def parse(cls, document: object) -> Registry:
        if not isinstance(document, dict) or document.get("format") != FORMAT:
            raise RegistryError("unsupported geometry registry format")
        entries = {name: _entry(name, value) for name, value in _mapping(document, "entries").items()}
        routes = {}
        for role, options in _mapping(document, "routes").items():
            if role not in ROLES:
                raise RegistryError(f"unknown role {role!r}")
            if not isinstance(options, list) or not options:
                raise RegistryError(f"role {role} needs at least one option")
            routes[role] = tuple(_option(role, item, entries) for item in options)
        missing = [role for role in ROLES if role not in routes]
        if missing:
            raise RegistryError(f"roles without options: {', '.join(missing)}")
        return cls(entries, routes)

    def select(self, role: str, host: Host) -> Selection:
        """The best option for `role` that this host can run."""
        if role not in self.routes:
            raise NoRoute(f"no analysis role named {role!r}")
        rejected = []
        for rank, option in enumerate(self.routes[role]):
            reason = _unfit(option, host)
            if reason is None:
                return Selection(role, option, rank, tuple(rejected))
            rejected.append(f"{option.entry} on {option.device} at {option.side} px: {reason}")
        raise NoRoute(f"no {role} model fits this worker: " + "; ".join(rejected))


def _unfit(option: Option, host: Host) -> str | None:
    if host.ram_budget_mib is not None and option.ram_peak_mib > host.ram_budget_mib:
        return f"needs {option.ram_peak_mib} MiB of memory, the budget is {host.ram_budget_mib} MiB"
    if option.device == "cpu":
        return None
    if host.gpu_budget_mib is None:
        return "no GPU is available to this worker"
    if option.gpu_peak_mib > host.gpu_budget_mib:
        return f"needs {option.gpu_peak_mib} MiB of GPU memory, the budget is {host.gpu_budget_mib} MiB"
    return None


def _mapping(document: dict, key: str) -> dict:
    value = document.get(key)
    if not isinstance(value, dict) or not value:
        raise RegistryError(f"the registry needs a non-empty {key} object")
    return value


def _entry(name: str, value: object) -> Entry:
    if not isinstance(value, dict):
        raise RegistryError(f"entry {name} must be an object")
    outputs = value.get("outputs")
    if not isinstance(outputs, dict) or not outputs:
        raise RegistryError(f"entry {name} must declare its outputs")
    for output, convention in outputs.items():
        if convention not in OUTPUTS.get(output, ()):
            raise RegistryError(f"entry {name} has an unknown {output} output {convention!r}")
    conversion = value.get("conversion")
    if conversion is not None and conversion not in CONVERSIONS:
        raise RegistryError(f"entry {name} has an unknown conversion {conversion!r}")
    files = value.get("files")
    if not isinstance(files, list):
        raise RegistryError(f"entry {name} must list its files")
    try:
        manifest = Manifest(tuple(ModelFile.from_document(item) for item in files))
    except (TypeError, ValueError) as error:
        raise RegistryError(f"entry {name} has an invalid file: {error}") from None
    model = value.get("model")
    if not isinstance(model, str) or not model:
        raise RegistryError(f"entry {name} must name its model")
    return Entry(name, model, dict(outputs), manifest, conversion)


def _positive(value: object, number_type: type) -> bool:
    return isinstance(value, number_type) and not isinstance(value, bool) and value > 0


def _option(role: str, value: object, entries: dict[str, Entry]) -> Option:
    if not isinstance(value, dict):
        raise RegistryError(f"options for {role} must be objects")
    entry = entries.get(value.get("entry"))
    if entry is None:
        raise RegistryError(f"an option for {role} names an unknown entry")
    output = role.split(".")[0]
    if output not in entry.outputs:
        raise RegistryError(f"entry {entry.name} has no {output} output for {role}")
    device, side = value.get("device"), value.get("side")
    if device not in DEVICES or side not in SIDES:
        raise RegistryError(f"an option for {role} has an unsupported device or size")
    seconds, gpu, ram = value.get("seconds"), value.get("gpu_peak_mib"), value.get("ram_peak_mib")
    if not _positive(seconds, (int, float)):
        raise RegistryError(f"an option for {role} needs its measured seconds")
    if (device == "cuda") != (gpu is not None) or (gpu is not None and not _positive(gpu, int)):
        raise RegistryError(f"GPU options for {role} need their measured GPU peak, and CPU options must not have one")
    if not _positive(ram, int):
        raise RegistryError(f"an option for {role} needs its measured RAM peak")
    return Option(entry.name, device, side, float(seconds), gpu, ram)
