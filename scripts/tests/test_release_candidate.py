import copy
import tempfile
from pathlib import Path
import unittest
from unittest.mock import patch

from gpu_process import belongs_to_container
from release_candidate import ROLES, digest, publish, validate, validate_proof, write_new
from release_identity import ReleaseIdentity


class CandidateTests(unittest.TestCase):
    def setUp(self):
        self.identity = ReleaseIdentity()
        self.candidate = {
            "revision": "a" * 40,
            "source": self.identity.source,
            "images": {role: self.identity.repository(role) + ":sha-" + "a" * 12
                       + "@sha256:" + str(index) * 64 for index, role in enumerate(ROLES)},
        }

    def test_matching_immutable_set_and_proof(self):
        validate(self.candidate, self.identity)
        validate_proof(self.candidate, {"candidate_sha256": digest(self.candidate), "gpu": "passed"})

    def test_reject_wrong_roles_revisions_namespaces_and_mutable_images(self):
        for replacement in ("ubuntu:latest", self.candidate["images"]["cad"],
                            self.candidate["images"]["blender"].replace("sha-aaa", "sha-bbb"),
                            self.candidate["images"]["blender"].replace("chrisbennight", "other")):
            with self.subTest(replacement=replacement):
                candidate = copy.deepcopy(self.candidate)
                candidate["images"]["blender"] = replacement
                with self.assertRaises(ValueError):
                    validate(candidate, self.identity)

    def test_proof_for_other_candidate_cannot_publish(self):
        proof = {"candidate_sha256": digest(self.candidate), "gpu": "passed"}
        self.candidate["images"]["server"] = self.candidate["images"]["server"].replace("0" * 64, "f" * 64)
        with patch("release_candidate.subprocess.run") as run:
            with self.assertRaises(ValueError):
                publish(self.candidate, proof, self.identity)
            run.assert_not_called()

    def test_failed_or_missing_gpu_proof_cannot_publish(self):
        for proof in ({}, {"candidate_sha256": digest(self.candidate), "gpu": "failed"}):
            with patch("release_candidate.subprocess.run") as run:
                with self.assertRaises(ValueError):
                    publish(self.candidate, proof, self.identity)
                run.assert_not_called()

    def test_evidence_is_create_only(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "candidate.json"
            write_new(path, self.candidate)
            original = path.read_bytes()
            with self.assertRaises(FileExistsError):
                write_new(path, {})
            self.assertEqual(path.read_bytes(), original)

    def test_host_pid_matching_accepts_rootless_and_rootful_cgroups(self):
        container = "a" * 64
        for path in ("0::/system.slice/docker-" + container + ".scope",
                     "0::/user.slice/user-1002.slice/user@1002.service/app.slice/docker-" + container + ".scope",
                     "0::/docker/" + container):
            self.assertTrue(belongs_to_container(path, container))
        self.assertFalse(belongs_to_container("0::/docker/" + "b" * 64, container))
        self.assertFalse(belongs_to_container("0::/docker/" + container + "-other", container))
