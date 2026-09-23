"""Release metadata requires matching roles and immutable references."""
import copy
import unittest
from unittest.mock import patch
from release_record import ROLES, publish, validate
from release_identity import ReleaseIdentity

class ReleaseRecordTests(unittest.TestCase):
    def setUp(self):
        self.identity = ReleaseIdentity()
        self.record = {"revision": "a" * 40, "source": self.identity.source,
                       "images": {role: self.identity.repository(role) + ":sha-" + "a" * 12
                                  + "@sha256:" + str(index) * 64 for index, role in enumerate(ROLES)}}

    def test_matching_images(self):
        self.assertEqual(validate(self.record, self.identity), self.record)

    def test_reject_wrong_roles_revisions_namespaces_and_mutable_images_before_push(self):
        for replacement in ("ubuntu:latest", self.record["images"]["cad"],
                            self.record["images"]["blender"].replace("sha-aaa", "sha-bbb"),
                            self.record["images"]["blender"].replace("chrisbennight", "other")):
            with self.subTest(replacement=replacement), patch("release_record.subprocess.run") as run:
                record = copy.deepcopy(self.record)
                record["images"]["blender"] = replacement
                with self.assertRaises(ValueError):
                    publish(record, self.identity)
                run.assert_not_called()
