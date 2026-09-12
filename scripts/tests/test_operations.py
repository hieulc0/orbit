import importlib.util
import json
import os
from pathlib import Path
import tempfile
import unittest
import uuid


def module(name, filename):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).parents[1] / filename)
    value = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(value)
    return value


init = module("initialize", "init-deployment.py")
backup = module("backup", "orbit_backup.py")


class Operations(unittest.TestCase):
    def test_initializer_preserves_existing_data_and_uses_private_secret_files(self):
        with tempfile.TemporaryDirectory() as root:
            destination = init.initialize(Path(root) / "new")
            config = json.loads((destination / "server.json").read_text())
            self.assertEqual(config["operator_credential"]["provider"], "file")
            token = (destination / "secrets/operator-token").read_text().strip()
            self.assertEqual(len(token), 64)
            self.assertNotIn(token, (destination / ".env").read_text())
            self.assertEqual((destination / "secrets/operator-token").stat().st_mode & 0o777, 0o600)
            with self.assertRaises(ValueError):
                init.initialize(destination)
            self.assertEqual(token, (destination / "secrets/operator-token").read_text().strip())

    def test_verifier_rejects_corruption_traversal_and_symlink_files(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "artifacts").mkdir()
            identity = str(uuid.uuid4())
            artifact = root / "artifacts" / identity
            artifact.write_bytes(b"accepted bytes")
            (root / "database.dump").write_bytes(b"fixture dump")
            expected = backup.fingerprint(artifact)
            manifest = {"format":"orbit-backup/v1", "files":{"database.dump":backup.fingerprint(root / "database.dump"), f"artifacts/{identity}":expected}, "accepted_artifacts":{identity:expected}}
            def save():
                (root / "manifest.json").write_text(json.dumps(manifest))
            save()
            self.assertEqual(backup.verify(root), manifest)
            artifact.write_bytes(b"corrupt")
            with self.assertRaises(ValueError):
                backup.verify(root)
            artifact.write_bytes(b"accepted bytes")
            manifest["files"]["../escape"] = expected
            save()
            with self.assertRaises(ValueError):
                backup.verify(root)
            del manifest["files"]["../escape"]
            save()
            artifact.unlink()
            artifact.symlink_to(root / "database.dump")
            with self.assertRaises(ValueError):
                backup.verify(root)

    def test_restore_refuses_nonempty_database_before_artifact_write(self):
        class Database:
            def offline(self):
                pass
            def sql(self, _):
                return "1"
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            bundle = root / "bundle"
            bundle.mkdir()
            (bundle / "artifacts").mkdir()
            (bundle / "database.dump").write_bytes(b"fixture dump")
            backup.write_json(bundle / "manifest.json", {"format":"orbit-backup/v1", "files":{"database.dump":backup.fingerprint(bundle / "database.dump")}, "accepted_artifacts":{}})
            with self.assertRaisesRegex(ValueError, "not empty"):
                backup.restore(Database(), bundle, root / "restored")
            self.assertFalse((root / "restored").exists())


if __name__ == "__main__":
    unittest.main()
