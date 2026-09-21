from pathlib import Path
import runpy
import stat
import tempfile
import unittest


create = runpy.run_path(str(Path(__file__).resolve().parents[1] / "create-local-secret"))["create"]


class LocalSecretTests(unittest.TestCase):
    def test_private_parent_readable_mount_and_preserved_existing_credential(self):
        with tempfile.TemporaryDirectory() as root:
            directory = Path(root) / "private"
            create(directory)
            credential = directory / "mcp-bearer"
            self.assertEqual(stat.S_IMODE(directory.stat().st_mode), 0o700)
            self.assertEqual(stat.S_IMODE(credential.stat().st_mode), 0o444)
            original = credential.read_bytes()
            self.assertTrue(len(original) == 65 and original.endswith(b"\n"))
            self.assertTrue(all(byte in b"0123456789abcdef" for byte in original[:-1]))
            with self.assertRaises(FileExistsError):
                create(directory)
            self.assertTrue(credential.read_bytes() == original)

    def test_refuses_shared_directory_and_symlink(self):
        with tempfile.TemporaryDirectory() as root:
            directory = Path(root) / "shared"
            directory.mkdir(mode=0o755)
            directory.chmod(0o755)
            with self.assertRaises(ValueError):
                create(directory)
            directory.chmod(0o700)
            alias = Path(root) / "alias"
            alias.symlink_to(directory, target_is_directory=True)
            with self.assertRaises(OSError):
                create(alias)
            self.assertFalse((directory / "mcp-bearer").exists())


if __name__ == "__main__":
    unittest.main()
