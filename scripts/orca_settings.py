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
import sys


# Orca builds each preset category from these initializer lists; see
# Preset::printer_options, Preset::print_options and Preset::filament_options.
CATEGORIES = {
    "printer": [("src/libslic3r/Preset.cpp", "s_Preset_printer_options"),
                ("src/libslic3r/Preset.cpp", "s_Preset_machine_limits_options"),
                ("src/libslic3r/PrintConfig.cpp", "m_extruder_option_keys")],
    "process": [("src/libslic3r/Preset.cpp", "s_Preset_print_options")],
    "filament": [("src/libslic3r/Preset.cpp", "s_Preset_filament_options")],
}
TOKENS = re.compile(r'"(?:\\.|[^"\\\n])*"|/\*.*?\*/|//[^\n]*', re.S)
NAME = re.compile(r"[A-Za-z][A-Za-z0-9_]*")


def strip_comments(text):
    """Remove C++ comments while keeping string literals intact."""
    return TOKENS.sub(lambda match: match.group(0) if match.group(0).startswith('"') else " ", text)


def initializer(text, symbol):
    """Return the string literals in the braced initializer assigned to symbol."""
    match = re.search(rf"\b{re.escape(symbol)}\s*(?:=\s*)?\{{", text)
    if match is None:
        raise ValueError(f"{symbol} initializer not found")
    depth = 0
    for index in range(match.end() - 1, len(text)):
        if text[index] == "{":
            depth += 1
        elif text[index] == "}":
            depth -= 1
            if depth == 0:
                names = re.findall(r'"([^"\\]*)"', text[match.end():index])
                invalid = [name for name in names if not NAME.fullmatch(name)]
                if invalid:
                    raise ValueError(f"{symbol} contains unexpected names: {invalid}")
                if not names:
                    raise ValueError(f"{symbol} initializer is empty")
                return names
    raise ValueError(f"{symbol} initializer is unterminated")


def extract(root):
    sources = {}
    result = {}
    for category, parts in CATEGORIES.items():
        names = set()
        for relative, symbol in parts:
            if relative not in sources:
                sources[relative] = strip_comments((root / relative).read_text(encoding="utf-8"))
            names.update(initializer(sources[relative], symbol))
        result[category] = sorted(names)
    return result


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
    except (OSError, ValueError) as error:
        print(f"orca_settings: {error}", file=sys.stderr)
        return 1
    document = {"engine": "OrcaSlicer", "engine_version": args.engine_version,
                "source_revision": args.source_revision, **settings}
    args.output.write_text(json.dumps(document, indent=1) + "\n", encoding="utf-8")
    return 0


if __name__ == "__main__":
    sys.exit(main())
