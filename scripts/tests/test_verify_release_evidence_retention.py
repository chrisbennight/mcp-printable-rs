"""Retain only verified record bundles, outside the indexed release evidence."""

import base64
import hashlib
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

from release_identity import ReleaseIdentity
from verify_release_evidence import EVIDENCE_TYPE, PROVENANCE_TYPE, verify_remote


class RecordBundleRetentionTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.revision = "a" * 40
        self.reference = (ReleaseIdentity().repository("pair") + ":sha-"
                          + self.revision[:12] + "@sha256:" + "b" * 64)
        self.retain = self.root / "evidence"
        self.output = self.root / "record-bundles.jsonl"
        self.index = {"schema": 1, "revision": self.revision, "images": {"server": "fixture"}}
        self.index_bytes = json.dumps(self.index).encode()
        self.bundles = [
            self.bundle(PROVENANCE_TYPE, {"build": "fixture"}),
            self.bundle(EVIDENCE_TYPE, {
                "schema": 1, "revision": self.revision, "images": self.index["images"],
                "evidence_index_sha256": hashlib.sha256(self.index_bytes).hexdigest(),
            }),
        ]

    @staticmethod
    def bundle(predicate_type, predicate):
        payload = json.dumps({"predicateType": predicate_type, "predicate": predicate}).encode()
        return {"dsseEnvelope": {"payload": base64.b64encode(payload).decode()},
                "verificationMaterial": {"fixture": predicate_type}}

    @staticmethod
    def result(bundle):
        return subprocess.CompletedProcess([], 0, stdout=json.dumps([
            {"attestation": {"bundle": bundle}, "unverified_extra": {"must_not_be_saved": True}},
        ]))

    def download(self, reference, directory):
        self.assertEqual(reference, self.reference)
        directory.mkdir()
        (directory / "index.json").write_bytes(self.index_bytes)
        (directory / "retained-material").write_bytes(b"indexed release payload")

    def verify_components(self, candidate, directory, trusted_root):
        self.assertFalse(self.retain.exists())
        self.assertFalse(self.output.exists())
        self.assertEqual((directory / "retained-material").read_bytes(), b"indexed release payload")

    def run_verification(self, retain=None, output=None):
        verify_remote(self.reference, self.revision,
                      retain=self.retain if retain is None else retain,
                      record_bundle_output=self.output if output is None else output)

    def test_retains_both_verified_predicates_without_changing_indexed_evidence(self):
        with patch("verify_release_evidence.subprocess.run", side_effect=[
                self.result(bundle) for bundle in self.bundles]) as run, \
                patch("verify_release_evidence.download_evidence", side_effect=self.download) as download, \
                patch("verify_release_evidence.verify_directory", return_value=self.index), \
                patch("verify_release_evidence.verify_components", side_effect=self.verify_components):
            self.run_verification()
        self.assertEqual([json.loads(line) for line in self.output.read_text().splitlines()], self.bundles)
        self.assertEqual(set(path.name for path in self.retain.iterdir()),
                         {"index.json", "retained-material"})
        self.assertEqual((self.retain / "index.json").read_bytes(), self.index_bytes)
        download.assert_called_once()
        for call, predicate in zip(run.call_args_list, (PROVENANCE_TYPE, EVIDENCE_TYPE)):
            arguments = call.args[0]
            for flag, value in (("--repo", "chrisbennight/mcp-printable-rs"),
                                ("--signer-workflow", "chrisbennight/mcp-printable-rs/.github/workflows/ci.yml"),
                                ("--source-ref", "refs/heads/main"),
                                ("--source-digest", self.revision), ("--predicate-type", predicate)):
                self.assertEqual(arguments[arguments.index(flag) + 1], value)
            self.assertIn("oci://" + self.reference, arguments)
            self.assertIn("--deny-self-hosted-runners", arguments)
            self.assertIn("--bundle-from-oci", arguments)

    def test_missing_or_wrong_verified_predicate_creates_no_retained_output(self):
        failures = [subprocess.CompletedProcess([], 0, stdout="[]"),
                    self.result(self.bundles[0]),
                    subprocess.CompletedProcess([], 0, stdout='[{"attestation": {}}]')]
        for failure in failures:
            with self.subTest(result=failure.stdout), \
                    patch("verify_release_evidence.subprocess.run", side_effect=[self.result(self.bundles[0]), failure]), \
                    patch("verify_release_evidence.download_evidence") as download:
                with self.assertRaises((ValueError, KeyError)):
                    self.run_verification()
                download.assert_not_called()
                self.assertFalse(self.retain.exists())
                self.assertFalse(self.output.exists())

    def test_signature_failure_does_not_download_or_save_bundles(self):
        with patch("verify_release_evidence.subprocess.run", side_effect=[
                self.result(self.bundles[0]), subprocess.CalledProcessError(1, ["gh"])]), \
                patch("verify_release_evidence.download_evidence") as download:
            with self.assertRaises(subprocess.CalledProcessError):
                self.run_verification()
        download.assert_not_called()
        self.assertFalse(self.retain.exists())
        self.assertFalse(self.output.exists())

    def test_index_inventory_or_component_failure_creates_no_retained_output(self):
        for stage in ("index", "inventory", "components"):
            with self.subTest(stage=stage), \
                    patch("verify_release_evidence.subprocess.run", side_effect=[
                        self.result(bundle) for bundle in self.bundles]), \
                    patch("verify_release_evidence.download_evidence", side_effect=self.download), \
                    patch("verify_release_evidence.verify_directory", return_value=self.index,
                          side_effect=ValueError("inventory mismatch") if stage == "inventory" else None), \
                    patch("verify_release_evidence.verify_record_index", side_effect=(
                        ValueError("index mismatch") if stage == "index" else None)), \
                    patch("verify_release_evidence.verify_components", side_effect=(
                        ValueError("component signature failed") if stage == "components" else None)):
                with self.assertRaises(ValueError):
                    self.run_verification()
                self.assertFalse(self.retain.exists())
                self.assertFalse(self.output.exists())

    def test_output_conflicts_are_rejected_before_any_external_call(self):
        existing = self.root / "existing"
        existing.write_bytes(b"preserve existing bytes")
        dangling = self.root / "dangling"
        dangling.symlink_to(self.root / "absent")
        conflicts = [
            (None, self.output), (existing, self.output), (self.retain, existing),
            (existing / "evidence", self.output), (dangling / "evidence", self.output),
            (dangling, self.output), (self.retain, dangling),
            (self.retain, self.retain), (self.retain, self.retain / "bundles.jsonl"),
            (self.output / "evidence", self.output),
            (self.retain, self.root / "absent-parent" / "bundles.jsonl"),
        ]
        with patch("verify_release_evidence.subprocess.run") as run, \
                patch("verify_release_evidence.download_evidence") as download:
            for retain, output in conflicts:
                with self.subTest(retain=retain, output=output), self.assertRaises(ValueError):
                    verify_remote(self.reference, self.revision, retain=retain, record_bundle_output=output)
            run.assert_not_called()
            download.assert_not_called()
        self.assertEqual(existing.read_bytes(), b"preserve existing bytes")
        self.assertTrue(dangling.is_symlink())

    def test_exclusive_bundle_publication_preserves_a_racing_existing_file(self):
        def racing_components(candidate, directory, trusted_root):
            self.output.write_bytes(b"created by another caller")

        with patch("verify_release_evidence.subprocess.run", side_effect=[
                self.result(bundle) for bundle in self.bundles]), \
                patch("verify_release_evidence.download_evidence", side_effect=self.download), \
                patch("verify_release_evidence.verify_directory", return_value=self.index), \
                patch("verify_release_evidence.verify_components", side_effect=racing_components):
            with self.assertRaises(FileExistsError):
                self.run_verification()
        self.assertEqual(self.output.read_bytes(), b"created by another caller")
        self.assertEqual(list(self.root.glob(".printable-record-bundles-*")), [])

    def test_existing_retain_flow_does_not_require_record_bundle_output(self):
        with patch("verify_release_evidence.subprocess.run", side_effect=[
                self.result(bundle) for bundle in self.bundles]), \
                patch("verify_release_evidence.download_evidence", side_effect=self.download), \
                patch("verify_release_evidence.verify_directory", return_value=self.index), \
                patch("verify_release_evidence.verify_components", side_effect=self.verify_components):
            verify_remote(self.reference, self.revision, retain=self.retain)
        self.assertEqual((self.retain / "index.json").read_bytes(), self.index_bytes)
        self.assertFalse(self.output.exists())


if __name__ == "__main__":
    unittest.main()
