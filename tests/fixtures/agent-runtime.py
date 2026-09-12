"""Deterministic qualification runtime: no model provider, tool execution or cost.

This is a transport/lifecycle fixture, not a production agent or sandbox.
"""
import hashlib
import json
import os
import threading
import time
import uuid
from pathlib import Path
from orbit_worker import Client, operation, reserve_agent_call, agent_report

client = Client(os.environ["ORBIT_URL"], os.environ["ORBIT_TOKEN"], timeout=1)
client.register(["agent.run", "agent.fixture-v1"])
while True:
    response = client.claim("agent.run", request_id=str(uuid.uuid4()))
    if response["status"] == "accepted":
        assignment = response["assignment"]
        break
    time.sleep(0.05)

sent = time.monotonic()
started = client.send_operation(operation(assignment, "start"))
lease_until = sent + started["lease_remaining_ms"] / 1000
deadline = sent + started["deadline_remaining_ms"] / 1000
stopped = threading.Event()


def heartbeats():
    global lease_until
    try:
        while not stopped.wait(0.25):
            sent = time.monotonic()
            if sent >= lease_until or sent >= deadline:
                os._exit(2)
            response = client.send_operation(operation(assignment, "heartbeat"))
            if time.monotonic() >= lease_until:
                os._exit(2)
            lease_until = sent + response["lease_remaining_ms"] / 1000
    except Exception:
        if not stopped.is_set():
            os._exit(2)


thread = threading.Thread(target=heartbeats, daemon=True)
thread.start()
reservation = reserve_agent_call(assignment, call_id=assignment["attempt_id"],
                                 tokens=100, cost_microusd=10)
assert client.send_operation(reservation)["replayed"] is False
root = Path(os.environ["ORBIT_AGENT_FIXTURE"])
root.mkdir(exist_ok=True)
workspace = root / assignment["workspace_id"]
workspace.mkdir()
saved = dict(assignment)
saved.pop("lease_token")
(workspace / "assignment.json").write_text(json.dumps(saved))
(root / "reserved.json").write_text(json.dumps({"attempt_id": assignment["attempt_id"],
    "generation": assignment["generation"], "workspace_id": assignment["workspace_id"]}))
if assignment["generation"] == 1 and os.environ.get("ORBIT_AGENT_KILL_TEST"):
    time.sleep(60)

outputs = []
report = agent_report(assignment, {"result": "durable fixture result"})
for kind, data in [("logs", b"deterministic local agent fixture\n"),
                   ("agent_report", json.dumps(report).encode())]:
    assert time.monotonic() < min(lease_until, deadline)
    prepared = client.send_operation(operation(assignment, "prepare_artifact", kind=kind,
        checksum=hashlib.sha256(data).hexdigest(), size=len(data)))
    artifact_id = prepared["artifact"]["id"]
    client.upload(operation(assignment, "finalize_artifact", artifact_id=artifact_id), data)
    outputs.append(artifact_id)
stopped.set()
thread.join(timeout=2)
assert time.monotonic() < min(lease_until, deadline)
client.send_operation(operation(assignment, "complete", success=True, outputs=outputs, failure=None))
