"""Live SDK qualification, invoked by the Rust disposable-fixture harness."""
import hashlib
import os
from concurrent.futures import ThreadPoolExecutor
from uuid import uuid4

from orbit_worker import Client, OrbitError, operation


def main():
    url, token = os.environ["ORBIT_URL"], os.environ["ORBIT_TOKEN"]
    client = Client(url, token, timeout=10)
    assert client.register(["repository.code"])["status"] == "accepted"
    claim_id = str(uuid4())
    claimed = client.claim("repository.code", request_id=claim_id)
    assert client.claim("repository.code", request_id=claim_id) == claimed
    assignment = claimed["assignment"]
    client.send_operation(operation(assignment, "start"))
    client.send_operation(operation(assignment, "heartbeat"))
    assert client.get_attempt(assignment)["task_state"] == "RUNNING"

    data = b"Python SDK qualification diagnostic"
    artifact = client.send_operation(operation(
        assignment, "prepare_artifact", kind="logs", size=len(data),
        checksum=hashlib.sha256(data).hexdigest()))["artifact"]
    finalize = operation(assignment, "finalize_artifact", artifact_id=artifact["id"])
    try:
        client.upload(finalize, b"wrong bytes")
        raise AssertionError("corrupt upload accepted")
    except OrbitError as error:
        assert error.status == 400

    # Concurrent retransmission must publish one immutable identity and receipt.
    with ThreadPoolExecutor(max_workers=2) as executor:
        uploads = [executor.submit(Client(url, token, timeout=10).upload, finalize, data)
                   for _ in range(2)]
        assert uploads[0].result() == uploads[1].result()
    assert client.artifact(assignment["run_id"], artifact) == data
    completion = operation(assignment, "complete", success=False, outputs=[artifact["id"]],
                           failure={"category": "task_failure", "code": "qualification",
                                    "message": "Intentional SDK failure fixture",
                                    "side_effect_status": "none"})
    receipt = client.send_operation(completion)
    assert client.send_operation(completion) == receipt
    try:
        client.send_operation(operation(assignment, "heartbeat"))
        raise AssertionError("terminal attempt renewed")
    except OrbitError as error:
        assert error.status == "ownership_lost"
    try:
        client.send_operation({**completion, "outputs": []})
        raise AssertionError("conflicting completion accepted")
    except OrbitError as error:
        assert error.status == 409
    assert client.get_attempt(assignment)["task_state"] == "FAILED"
    assert client.claim("repository.code", request_id=str(uuid4()))["status"] == "no_work"
    print("Python SDK live qualification passed")


if __name__ == "__main__":
    main()
