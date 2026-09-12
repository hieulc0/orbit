"""Offline protocol demonstration, not a model integration or reasoning agent."""
import json
import os
from pathlib import Path

payload = json.loads(Path(os.environ["ORBIT_AGENT_INPUT"]).read_text())
assert payload["format"] == "orbit-command-agent/v1"
print(json.dumps({"output": {"runtime": "offline-demo/revision-1", "task": payload["task"]}}))
