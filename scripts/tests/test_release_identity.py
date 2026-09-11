import unittest

from release_identity import ReleaseIdentity


class ReleaseIdentityTests(unittest.TestCase):
    def test_default_and_independent_registry(self):
        self.assertEqual(ReleaseIdentity().repository("server"), "ghcr.io/chrisbennight/mcp-printable-rs")
        identity = ReleaseIdentity("registry.example.com:5443", "team/models", "https://github.com/team/printable")
        reference = identity.repository("blender") + ":sha-" + "a" * 12 + "@sha256:" + "b" * 64
        self.assertIsNotNone(identity.reference_pattern("blender").fullmatch(reference))
        self.assertIsNone(identity.reference_pattern("server").fullmatch(reference))
        self.assertIsNone(identity.reference_pattern("blender").fullmatch(reference.split("@")[0]))

    def test_rejects_ambiguous_identity_and_embedded_credentials(self):
        for registry in ["https://ghcr.io", "user@ghcr.io", "ghcr.io/path", "ghcr.io:99999", "-host", "host\n"]:
            with self.subTest(registry=registry), self.assertRaises(ValueError):
                ReleaseIdentity(registry=registry)
        for namespace in ["../team", "Team", "team:latest", "team//project", "team\n"]:
            with self.subTest(namespace=namespace), self.assertRaises(ValueError):
                ReleaseIdentity(namespace=namespace)
        for source in ["http://github.com/team/repo", "https://token@github.com/team/repo", "https://github.com/team/repo?token=a"]:
            with self.subTest(source=source), self.assertRaises(ValueError):
                ReleaseIdentity(source=source)


if __name__ == "__main__":
    unittest.main()
