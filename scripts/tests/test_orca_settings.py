import contextlib
import io
import json
from pathlib import Path
import tempfile
import subprocess
import unittest

import orca_settings


PRESET = """
static std::vector<std::string> s_Preset_print_options{
    "layer_height", // "commented_after",
    /* "commented_block", */ "brim_type",
};
static std::vector<std::string> s_Preset_filament_options {/*"filament_colour", */ "default_filament_colour",
    "required_nozzle_HRC"};
static std::vector<std::string> s_Preset_machine_limits_options {
    "machine_max_speed_x"};
static std::vector<std::string> s_Preset_printer_options {
    "printable_area", "nozzle_diameter"};
const std::vector<std::string>& Preset::printer_options() { opts = s_Preset_printer_options; }
"""
CONFIG = """
    m_extruder_option_keys = {
        "nozzle_diameter", "retraction_length", "url // not a comment"
    };
"""


def checkout(directory, preset=PRESET, config=CONFIG):
    root = Path(directory)
    (root / "src/libslic3r").mkdir(parents=True)
    (root / "src/libslic3r/Preset.cpp").write_text(preset, encoding="utf-8")
    (root / "src/libslic3r/PrintConfig.cpp").write_text(config, encoding="utf-8")
    return root


def commit_source(root):
    for arguments in [["init", "--quiet"], ["add", "src"],
                      ["-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
                       "commit", "--quiet", "-m", "Source fixture"]]:
        subprocess.run(["git", "-C", str(root), *arguments], check=True, capture_output=True)
    return subprocess.check_output(["git", "-C", str(root), "rev-parse", "HEAD"], text=True).strip()


class OrcaSettingsTests(unittest.TestCase):
    def test_extracts_each_category_without_commented_names(self):
        config = CONFIG.replace('"url // not a comment"', '"max_layer_height"')
        with tempfile.TemporaryDirectory() as directory:
            settings = orca_settings.extract(checkout(directory, config=config))
        self.assertEqual(settings["process"], ["brim_type", "layer_height"])
        self.assertEqual(settings["filament"], ["default_filament_colour", "required_nozzle_HRC"])
        self.assertEqual(settings["printer"], ["machine_max_speed_x", "max_layer_height", "nozzle_diameter",
                                               "printable_area", "retraction_length"])

    def test_rejects_missing_empty_or_unexpected_lists(self):
        cases = [
            (PRESET.replace("s_Preset_print_options", "s_Preset_other_options"), CONFIG),
            (PRESET.replace('"machine_max_speed_x"', ""), CONFIG),
            (PRESET, CONFIG),
        ]
        for preset, config in cases:
            with self.subTest(preset=preset[:80]), tempfile.TemporaryDirectory() as directory:
                with self.assertRaises(ValueError):
                    orca_settings.extract(checkout(directory, preset, config))

    def test_main_records_the_engine_identity(self):
        config = CONFIG.replace('"url // not a comment"', '"max_layer_height"')
        with tempfile.TemporaryDirectory() as directory:
            root = checkout(directory, config=config)
            output = Path(directory) / "settings.json"
            revision = commit_source(root)
            self.assertEqual(orca_settings.main([str(root), "--engine-version", "2.4.2",
                                                 "--source-revision", revision, "--output", str(output)]), 0)
            document = json.loads(output.read_text(encoding="utf-8"))
        self.assertEqual(document["engine"], "OrcaSlicer")
        self.assertEqual(document["engine_version"], "2.4.2")
        self.assertEqual(document["source_revision"], revision)
        self.assertIn("brim_type", document["process"])

    def test_source_identity_rejects_wrong_revision_and_changed_files(self):
        with tempfile.TemporaryDirectory() as directory:
            root = checkout(directory)
            revision = commit_source(root)
            orca_settings.verify_source(root, revision)
            with self.assertRaisesRegex(ValueError, "HEAD"):
                orca_settings.verify_source(root, "0" * 40)
            (root / "src/libslic3r/PrintConfig.cpp").write_text("changed")
            with self.assertRaisesRegex(ValueError, "differ"):
                orca_settings.verify_source(root, revision)

    def test_main_rejects_ambiguous_identity_and_reports_extraction_errors(self):
        with tempfile.TemporaryDirectory() as directory:
            root = checkout(directory)
            output = Path(directory) / "settings.json"
            for version, revision in [("latest", "a" * 40), ("2.4.2", "6915edb")]:
                with self.subTest(version=version, revision=revision), \
                        contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
                    orca_settings.main([str(root), "--engine-version", version,
                                        "--source-revision", revision, "--output", str(output)])
            with contextlib.redirect_stderr(io.StringIO()) as errors:
                status = orca_settings.main([str(root), "--engine-version", "2.4.2",
                                             "--source-revision", "a" * 40, "--output", str(output)])
            self.assertEqual(status, 1)
            self.assertIn("unexpected names", errors.getvalue())
            self.assertFalse(output.exists())


if __name__ == "__main__":
    unittest.main()
