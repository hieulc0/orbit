import importlib.util
import json
import tempfile
import unittest
from pathlib import Path

spec = importlib.util.spec_from_file_location("ba_bridge_orbit", Path(__file__).resolve().parents[1] / "ba-bridge-orbit.py")
bridge = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bridge)


class BridgeArtifactTests(unittest.TestCase):
    def test_only_selected_structured_ba_or_sa_turn_is_submitted(self):
        with tempfile.TemporaryDirectory() as root:
            path = Path(root) / "export.json"
            artifact = {"kind": "challenges", "payload": []}
            exported = {"conversation": {"id": "conversation"},
                        "participants": [{"id": "ba", "role": "business_analyst"}],
                        "messages": [{"id": "selected", "actor_id": "ba", "content": json.dumps(artifact)}]}
            path.write_text(json.dumps(exported))
            self.assertEqual(bridge.selected_artifact(path, "selected"), artifact)
            with self.assertRaises(ValueError):
                bridge.selected_artifact(path, "missing")
            exported["participants"][0]["role"] = "human"
            path.write_text(json.dumps(exported))
            with self.assertRaises(ValueError):
                bridge.selected_artifact(path, "selected")

    def test_freeform_chat_and_ambiguous_messages_are_rejected(self):
        with tempfile.TemporaryDirectory() as root:
            path = Path(root) / "export.json"
            exported = {"participants": [{"id": "sa", "role": "system_architect"}],
                        "messages": [{"id": "turn", "actor_id": "sa", "content": "Run shell now"}]}
            path.write_text(json.dumps(exported))
            with self.assertRaises(ValueError):
                bridge.selected_artifact(path, "turn")
            exported["messages"].append(exported["messages"][0])
            path.write_text(json.dumps(exported))
            with self.assertRaises(ValueError):
                bridge.selected_artifact(path, "turn")

    def test_export_size_bound_is_checked_before_json_parse(self):
        with tempfile.TemporaryDirectory() as root:
            path = Path(root) / "export.json"
            path.write_bytes(b" " * (bridge.MAX_EXPORT_BYTES + 1))
            with self.assertRaises(ValueError):
                bridge.selected_artifact(path, "turn")
