"""Disposable, labeled containers only. No daemon restart or broad cleanup."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time
import uuid

POSTGRES_IMAGE = "docker.io/library/postgres@sha256:18cfe3ef5e6815560c98237d6216d1e5119702fb0f3894c8785dd58b8bbe5d73"
LABEL = "io.orbit.qualification"


class Runtime:
    def __init__(self, runtime, image):
        if runtime not in ("docker", "podman"):
            raise ValueError("runtime must be docker or podman")
        self.runtime, self.image = runtime, image
        self.prefix = [runtime] + (["--cgroup-manager=cgroupfs"] if runtime == "podman" else [])
        for reference in (POSTGRES_IMAGE, image):
            self.command("image", "inspect", reference)
        self.identity = uuid.uuid4().hex
        self.network = f"orbit-smoke-{self.identity}"
        self.containers = []
        self.network_created = False
        parent = Path(__file__).resolve().parents[1] / "target/deployment-smoke"
        parent.mkdir(parents=True, exist_ok=True)
        self.root = Path(tempfile.mkdtemp(prefix="fixture-", dir=parent))

    def command(self, *args, timeout=40, check=True):
        result = subprocess.run([*self.prefix, *map(str, args)], stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=timeout)
        if check and result.returncode:
            raise RuntimeError(f"{self.runtime} {args[0]} failed (exit {result.returncode}); inspect retained fixture {getattr(self, 'root', '')}")
        return result

    def __enter__(self):
        self.command("network", "create", "--label", f"{LABEL}={self.identity}", self.network)
        self.network_created = True
        return self

    def run(self, suffix, image, *args):
        name = f"orbit-smoke-{self.identity}-{suffix}"
        self.containers.append(name)
        self.command("run", "--detach", "--pull=never", "--name", name, "--label", f"{LABEL}={self.identity}",
                     "--network", self.network, *args, image)
        return name

    def postgres(self, suffix, installation, alias):
        name = self.run(suffix, POSTGRES_IMAGE, "--network-alias", alias,
                        "--env", "POSTGRES_USER=orbit", "--env", "POSTGRES_DB=orbit",
                        "--env", "POSTGRES_PASSWORD_FILE=/run/secrets/db-password",
                        "--volume", f"{installation}/secrets/db-password:/run/secrets/db-password:ro,z")
        deadline = time.monotonic() + 40
        while time.monotonic() < deadline:
            # The image's initialization-only temporary server listens on a
            # Unix socket. TCP readiness means the final server is running.
            if self.command("exec", name, "pg_isready", "-h", "127.0.0.1", "-U", "orbit", "-d", "orbit", check=False).returncode == 0:
                return name
            time.sleep(0.2)
        raise RuntimeError("fixture PostgreSQL did not become ready")

    def server(self, suffix, installation):
        identity = ["--user", f"{os.getuid()}:{os.getgid()}"]
        if self.runtime == "podman":
            identity = ["--user", "10001:10001", "--userns", "keep-id:uid=10001,gid=10001"]
        return self.run(suffix, self.image, *identity, "--read-only", "--cap-drop=ALL", "--security-opt=no-new-privileges",
                        "--pids-limit=256", "--memory=1g", "--cpus=2", "--tmpfs", "/tmp:rw,noexec,nosuid,size=16m",
                        "--publish", "127.0.0.1::7700", "--volume", f"{installation}/server.json:/etc/orbit/server.json:ro,z",
                        "--volume", f"{installation}/secrets:/run/orbit:ro,z",
                        "--volume", f"{installation}/artifacts:/var/lib/orbit/artifacts:z")

    def url(self, server):
        mapping = self.command("port", server, "7700/tcp").stdout.decode().strip().splitlines()[0]
        return f"http://{mapping}"

    def stop(self, name):
        self.command("stop", "--time", "35", name)

    def __exit__(self, exc_type, exc, traceback):
        removed = 0
        for name in reversed(self.containers):
            try:
                result = self.command("inspect", name, check=False)
                if result.returncode:
                    continue
                info = json.loads(result.stdout)[0]
                if info.get("Config", {}).get("Labels", {}).get(LABEL) != self.identity:
                    print(f"Refused cleanup of unexpected container {name}")
                    continue
                # Only containers created with this invocation's exact ownership label.
                self.command("stop", "--time", "5", name, check=False)
                self.command("rm", "--volumes", name)
                removed += 1
            except (RuntimeError, subprocess.TimeoutExpired, ValueError):
                print(f"Cleanup incomplete for named fixture {name}; inspect manually.")
        if self.network_created:
            try:
                self.command("network", "rm", self.network)
            except (RuntimeError, subprocess.TimeoutExpired):
                print(f"Cleanup incomplete for fixture network {self.network}")
        print(f"Removed {removed} labeled disposable containers and their anonymous volumes; private fixture files retained at {self.root}")
