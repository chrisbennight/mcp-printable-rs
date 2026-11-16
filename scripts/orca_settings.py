#!/usr/bin/env python3
"""Extract OrcaSlicer's per-category preset setting names from an Orca source checkout.

The slicer worker accepts profile overrides only for these names. Regenerate the
list whenever the packaged Orca release changes:

    python3 scripts/orca_settings.py ORCA_CHECKOUT --engine-version 2.4.2 \\
        --source-revision FULL_COMMIT_SHA \\
        --output crates/printable-server/src/slicing/orca-settings.json
"""

import argparse
import json
from pathlib import Path
import re
import subprocess
import sys

import orca_definitions
from tree_sitter import Language, Parser
import tree_sitter_cpp


# Orca builds each preset category from these initializer lists; see
# Preset::printer_options, Preset::print_options and Preset::filament_options.
CATEGORIES = {
    "printer": [("src/libslic3r/Preset.cpp", "s_Preset_printer_options"),
                ("src/libslic3r/Preset.cpp", "s_Preset_machine_limits_options"),
                ("src/libslic3r/PrintConfig.cpp", "m_extruder_option_keys")],
    "process": [("src/libslic3r/Preset.cpp", "s_Preset_print_options")],
    "filament": [("src/libslic3r/Preset.cpp", "s_Preset_filament_options")],
}
NAME = re.compile(r"[A-Za-z][A-Za-z0-9_]*")


def initializer(text, symbol):
    """Read the named initializer through syntax nodes, without counting braces."""
    root = Parser(Language(tree_sitter_cpp.language())).parse(text.encode("utf-8")).root_node
    matches = []
    for node in orca_definitions.walk(root):
        if node.type not in ("init_declarator", "assignment_expression"):
            continue
        name = node.child_by_field_name("declarator" if node.type == "init_declarator" else "left")
        value = node.child_by_field_name("value" if node.type == "init_declarator" else "right")
        if name is not None and orca_definitions.text(name) == symbol and value is not None:
            matches.append(value)
    if len(matches) != 1 or matches[0].has_error or matches[0].type != "initializer_list":
        raise ValueError(f"{symbol} requires one complete literal initializer")
    names = [orca_definitions.literal(child) for child in matches[0].named_children if child.type != "comment"]
    if any(not isinstance(name, str) or not NAME.fullmatch(name) for name in names):
        raise ValueError(f"{symbol} contains unexpected names")
    if not names:
        raise ValueError(f"{symbol} initializer is empty")
    return names


def extract(root):
    sources = {}
    result = {}
    for category, parts in CATEGORIES.items():
        names = set()
        for relative, symbol in parts:
            if relative not in sources:
                sources[relative] = (root / relative).read_text(encoding="utf-8")
            names.update(initializer(sources[relative], symbol))
        result[category] = sorted(names)
    return result


def verify_source(root, revision):
    """Bind generated facts to the clean public source revision named by the caller."""
    result = subprocess.run(["git", "-C", str(root), "rev-parse", "HEAD"],
                            capture_output=True, text=True, check=False)
    if result.returncode != 0 or result.stdout.strip() != revision:
        raise ValueError("source checkout HEAD does not match --source-revision")
    files = sorted({relative for parts in CATEGORIES.values() for relative, _ in parts}
                   | {"src/libslic3r/Config.hpp"})
    result = subprocess.run(["git", "-C", str(root), "diff", "--quiet", "HEAD", "--", *files],
                            capture_output=True, check=False)
    if result.returncode != 0:
        raise ValueError("source definition files differ from the specified revision")


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("orca", type=Path, help="OrcaSlicer source checkout")
    parser.add_argument("--engine-version", required=True, help="packaged Orca version, such as 2.4.2")
    parser.add_argument("--source-revision", required=True, help="full commit SHA of the checkout")
    parser.add_argument("--output", type=Path, required=True, help="JSON file to write")
    args = parser.parse_args(argv)
    if not re.fullmatch(r"[0-9]+(?:\.[0-9]+)+", args.engine_version):
        parser.error("--engine-version must be a dotted release number")
    if not re.fullmatch(r"[0-9a-f]{40}", args.source_revision):
        parser.error("--source-revision must be a full lowercase commit SHA")
    try:
        settings = extract(args.orca)
        verify_source(args.orca, args.source_revision)
        keys = set().union(*(set(names) for names in settings.values()))
        definitions = orca_definitions.extract(
            (args.orca / "src/libslic3r/PrintConfig.cpp").read_text(encoding="utf-8"), keys)
        verify_source(args.orca, args.source_revision)
    except (OSError, ValueError) as error:
        print(f"orca_settings: {error}", file=sys.stderr)
        return 1
    document = {"engine": "OrcaSlicer", "engine_version": args.engine_version,
                "source_revision": args.source_revision, **settings, "definitions": definitions}
    args.output.write_text(json.dumps(document, indent=1) + "\n", encoding="utf-8")
    return 0


if __name__ == "__main__":
    sys.exit(main())
