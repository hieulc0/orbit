"""Qualify a prebuilt local image using separate disposable, labeled containers."""
import argparse
import hashlib
import importlib.util
import json
from pathlib import Path
import secrets
import sys
import time
from urllib.error import HTTPError, URLError
from urllib.request import Request, urlopen
import uuid

from managed_runtime import Runtime
import orbit_backup

spec = importlib.util.spec_from_file_location("initialize", Path(__file__).with_name("init-deployment.py"))
initialize = importlib.util.module_from_spec(spec)
spec.loader.exec_module(initialize)
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "sdk/python"))
from orbit_worker import Client, operation


def request(url, token, path, body=None, raw=False):
    req = Request(url + path, data=None if body is None else json.dumps(body).encode(),
                  headers={"Content-Type":"application/json", "Authorization":"Bearer " + token})
    with urlopen(req, timeout=5) as response:
        data = response.read()
        return data if raw else json.loads(data)


def wait_ready(url):
    deadline = time.monotonic() + 45
    while time.monotonic() < deadline:
        try:
            if request(url, "", "/readyz")["status"] == "ready":
                return
        except (OSError, ValueError):
            pass
        time.sleep(0.2)
    raise RuntimeError("server image did not become ready")


def wait_terminal(url, token, run):
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        value = request(url, token, f"/runs/{run}")
        if value["state"] in ("SUCCEEDED", "FAILED", "CANCELLED", "NEEDS_INTERVENTION"):
            assert value["state"] == "SUCCEEDED", value["state"]
            return value
        time.sleep(0.1)
    raise RuntimeError("fixture run did not finish")


def artifact_fixture(url, token, installation):
    # A deterministic SDK protocol worker, not an OCI runtime qualification.
    image = "docker.io/library/alpine@sha256:28bd5fe8b56d1bd048e5babf5b10710ebe0bae67db86916198a6eec434943f8b"
    resources = {"cpu_millis":500,"memory_mib":64,"gpu":0}
    definition = {"apiVersion":"orbit/v1","kind":"Definition","metadata":{"name":"deployment-artifact-fixture"},
        "inputs":{"task":"Retain a deterministic SDK fixture artifact across restore."}, "steps":{"compute":{
            "uses":"container.run","container":{"image":image,"command":["printf","alpha artifact"]}, "resources":resources,
            "timeout_seconds":120,"max_attempts":1,"retry_backoff_seconds":0,"recovery_policy":"restart_from_inputs"}}}
    run = request(url, token, "/runs", {"request_id":str(uuid.uuid4()),"definition":definition})["run_id"]
    worker = Client(url, (installation / "secrets/compute-token").read_text().strip(), timeout=5)
    worker.register(["container.run"])
    deadline = time.monotonic() + 10
    while True:
        response = worker.claim("container.run", request_id=str(uuid.uuid4()))
        if response["status"] == "accepted":
            break
        if time.monotonic() > deadline:
            raise RuntimeError("fixture worker did not claim")
        time.sleep(0.1)
    assignment = response["assignment"]
    worker.send_operation(operation(assignment, "start"))
    report = {"attempt_id":assignment["attempt_id"],"idempotency_key":assignment["idempotency_key"],
              "image":image,"resources":resources,"exit_code":0,"success":True,"timed_out":False}
    outputs = []
    for kind, data in [("logs", b"deterministic SDK storage fixture\n"), ("container_report", json.dumps(report).encode()), ("data", b"alpha artifact")]:
        worker.send_operation(operation(assignment, "heartbeat"))
        prepared = worker.send_operation(operation(assignment, "prepare_artifact", kind=kind, checksum=hashlib.sha256(data).hexdigest(), size=len(data)))
        identity = prepared["artifact"]["id"]
        worker.upload(operation(assignment, "finalize_artifact", artifact_id=identity), data)
        outputs.append(identity)
    worker.send_operation(operation(assignment, "complete", success=True, outputs=outputs, failure=None))
    snapshot = wait_terminal(url, token, run)
    artifact = next(a for a in snapshot["artifacts"] if a["kind"] == "data")
    assert worker.artifact(run, artifact) == b"alpha artifact"
    return run, artifact


def qualify(runtime, image, backup=False):
    with Runtime(runtime, image) as fixture:
        installation = initialize.initialize(fixture.root / "installation")
        database = fixture.postgres("database", installation, "postgres")
        server = fixture.server("server", installation)
        url = fixture.url(server)
        token = (installation / "secrets/operator-token").read_text().strip()
        wait_ready(url)
        assert b"<html" in request(url, "", "/console/", raw=True)
        assert request(url, "", "/healthz")["status"] == "alive"
        definition = {"apiVersion":"orbit/v1","kind":"Definition","metadata":{"name":"packaged-timer"},
            "inputs":{"task":"Fresh packaged installation"},"steps":{"wait":{"uses":"engine.timer","delay_seconds":1,
            "timeout_seconds":60,"max_attempts":1,"retry_backoff_seconds":0,"recovery_policy":"restart_from_inputs"}}}
        run = request(url, token, "/runs", {"request_id":str(uuid.uuid4()),"definition":definition})["run_id"]
        wait_terminal(url, token, run)
        artifact_run, artifact = artifact_fixture(url, token, installation)
        before = request(url, token, f"/runs/{artifact_run}")
        journal = request(url, token, f"/runs/{artifact_run}/events")
        assert b"orbit_http_responses_total" in request(url, token, "/metrics", raw=True)
        fixture.stop(server)
        fixture.command("start", server)
        url = fixture.url(server)
        wait_ready(url)
        assert request(url, token, f"/runs/{artifact_run}") == before
        assert request(url, token, f"/runs/{artifact_run}/artifacts/{artifact['id']}", raw=True) == b"alpha artifact"
        # Rotation is a coordinated restart, not an implied hot-reload promise.
        fixture.stop(server)
        new_token = secrets.token_hex(32)
        token_file = installation / "secrets/operator-token"
        token_file.write_text(new_token + "\n")
        fixture.command("start", server)
        url = fixture.url(server)
        wait_ready(url)
        try:
            request(url, token, "/runs")
            raise AssertionError("old credential remained authorized after restart")
        except HTTPError as error:
            assert error.code == 401
        token = new_token
        assert request(url, token, f"/runs/{artifact_run}") == before
        fixture.stop(server)
        if backup:
            bundle = fixture.root / "backup"
            db = orbit_backup.Database(runtime, database)
            manifest = orbit_backup.create(db, installation / "artifacts", bundle)
            target = initialize.initialize(fixture.root / "replacement")
            # Keep generated directory out of the restore destination; never delete it.
            (target / "artifacts").rename(target / "initial-empty-artifacts")
            replacement = fixture.postgres("replacement-db", target, "replacement-postgres")
            password = (target / "secrets/db-password").read_text().strip()
            (target / "secrets/database-url").write_text(f"postgres://orbit:{password}@replacement-postgres:5432/orbit\n")
            orbit_backup.restore(orbit_backup.Database(runtime, replacement), bundle, target / "artifacts")
            restored = fixture.server("restored-server", target)
            restored_url = fixture.url(restored)
            restored_token = (target / "secrets/operator-token").read_text().strip()
            wait_ready(restored_url)
            assert request(restored_url, restored_token, f"/runs/{artifact_run}") == before
            assert request(restored_url, restored_token, f"/runs/{artifact_run}/events") == journal
            assert request(restored_url, restored_token, f"/runs/{artifact_run}/artifacts/{artifact['id']}", raw=True) == b"alpha artifact"
            fixture.stop(restored)
            assert manifest["accepted_artifacts"]
        result = {"format":"orbit-deployment-qualification/v1","status":"passed","runtime":runtime,"image":image,
                  "checks":["non-root read-only server","bundled console","readiness","timer","SDK artifact storage","server restart","credential rotation"],
                  "backup_restore":backup,"run_id":artifact_run}
        orbit_backup.write_json(fixture.root / "result.json", result)
        print(json.dumps(result))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--runtime", choices=["docker","podman"], required=True)
    parser.add_argument("--image", default="localhost/orbit:alpha")
    parser.add_argument("--backup", action="store_true")
    args = parser.parse_args()
    qualify(args.runtime, args.image, args.backup)
