#!/usr/bin/env python3
"""Capture and verify source-built native outputs from the actual build."""

import argparse
import hashlib
import json
from pathlib import Path, PurePosixPath
import re


def sha256(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def recipe_settings(recipe, component):
    stage = component["source_built_native"]["recipe_stage"]
    sections = re.split(r"(?m)^(?=FROM )", recipe.read_text())
    matches = [section for section in sections if re.match(r"FROM .+ AS " + re.escape(stage) + r"\n", section)]
    if len(matches) != 1:
        raise ValueError("source-built component has no unique reviewed recipe stage")
    section = matches[0]
    if component["source_url"] not in section or component["source_sha256"] not in section:
        raise ValueError("source-built recipe has inconsistent source identity")
    settings = dict(re.findall(r"-D([A-Za-z0-9_]+)=([^\s\\]+)", section))
    if not settings:
        raise ValueError("source-built recipe has no configured CMake settings")
    return settings


def validate_record(record, component, revision, recipe):
    if not isinstance(record, dict):
        raise ValueError("source-built record is not a component object")
    expected = {"schema": 1, "name": component["name"], "version": component["version"],
                "revision": revision, "source_url": component["source_url"],
                "source_sha256": component["source_sha256"], "recipe_sha256": sha256(recipe),
                "cmake_settings": recipe_settings(recipe, component)}
    if not re.fullmatch(r"[0-9a-f]{40}", revision):
        raise ValueError("source-built material requires a full source revision")
    if any(record.get(key) != value for key, value in expected.items()):
        raise ValueError("source-built record has inconsistent source, revision or build identity")
    outputs = record.get("native_files")
    if (not isinstance(outputs, list) or len(outputs) != 1 or not isinstance(outputs[0], dict)
            or outputs[0].get("path") != component["source_built_native"]["native_output_path"]
            or not isinstance(outputs[0].get("sha256"), str)
            or not re.fullmatch(r"[0-9a-f]{64}", outputs[0].get("sha256", ""))
            or type(outputs[0].get("size")) is not int or outputs[0]["size"] <= 4):
        raise ValueError("source-built record has missing, extra or invalid native outputs")
    install_paths = record.get("installed_paths")
    if (not isinstance(install_paths, list) or not install_paths
            or any(not isinstance(path, str) or not path.startswith(component["notice_prefix"])
                   or ".." in PurePosixPath(path).parts for path in install_paths)
            or outputs[0]["path"] not in install_paths or len(set(install_paths)) != len(install_paths)
            or not isinstance(record.get("install_manifest_sha256"), str)
            or not re.fullmatch(r"[0-9a-f]{64}", record.get("install_manifest_sha256", ""))):
        raise ValueError("source-built record has invalid installed-file material")
    return outputs


def capture(component, revision, recipe, source_archive, install_root, install_manifest, cache):
    if sha256(source_archive) != component["source_sha256"]:
        raise ValueError("source-built archive differs from its reviewed source digest")
    settings = recipe_settings(recipe, component)
    cache_settings = {}
    for line in cache.read_text().splitlines():
        match = re.fullmatch(r"([A-Za-z0-9_]+):[A-Za-z_]+=(.*)", line)
        if match and match[1] in settings:
            if match[1] in cache_settings:
                raise ValueError("source-built CMake cache has duplicate settings")
            cache_settings[match[1]] = match[2]
    if cache_settings != settings:
        raise ValueError("source-built CMake cache differs from its reviewed recipe settings")
    root = install_root.resolve(strict=True)
    installed_paths = []
    for line in install_manifest.read_text().splitlines():
        path = Path(line)
        if not path.is_absolute() or ".." in path.parts:
            raise ValueError("source-built install manifest has an unsafe path")
        if path.is_relative_to(install_root.absolute()):
            try:
                resolved = path.resolve(strict=True)
            except FileNotFoundError as error:
                raise ValueError("source-built installed file is absent") from error
            if not resolved.is_relative_to(root):
                raise ValueError("source-built installed file escapes its output root")
            relative = path.relative_to(install_root.absolute())
            installed_paths.append(component["notice_prefix"] + relative.as_posix())
    native_files = []
    for path in sorted(root.rglob("*")):
        if path.is_symlink():
            raise ValueError("source-built output may not contain links")
        if path.is_file():
            with path.open("rb") as stream:
                native = stream.read(4) == b"\x7fELF"
            if native:
                native_files.append({"path": component["notice_prefix"] + path.relative_to(root).as_posix(),
                                     "sha256": sha256(path), "size": path.stat().st_size})
    record = {"schema": 1, "name": component["name"], "version": component["version"],
              "revision": revision, "source_url": component["source_url"],
              "source_sha256": component["source_sha256"], "recipe_sha256": sha256(recipe),
              "cmake_settings": cache_settings, "native_files": native_files,
              "installed_paths": sorted(installed_paths), "install_manifest_sha256": sha256(install_manifest)}
    validate_record(record, component, revision, recipe)
    return record


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("component")
    parser.add_argument("revision")
    parser.add_argument("--components", type=Path, required=True)
    parser.add_argument("--recipe", type=Path, required=True)
    parser.add_argument("--source-archive", type=Path, required=True)
    parser.add_argument("--install-root", type=Path, required=True)
    parser.add_argument("--install-manifest", type=Path, required=True)
    parser.add_argument("--cache", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    arguments = parser.parse_args()
    components = json.loads(arguments.components.read_text())["components"]
    component = next(value for value in components if value["name"] == arguments.component)
    record = capture(component, arguments.revision, arguments.recipe, arguments.source_archive,
                     arguments.install_root, arguments.install_manifest, arguments.cache)
    arguments.output.write_text(json.dumps(record, sort_keys=True) + "\n")
