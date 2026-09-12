"""Initialize a NEW private single-host installation; never overwrite or start it."""
import argparse
import json
import os
from pathlib import Path
import secrets


def initialize(destination, port=7700):
    destination = Path(destination).absolute()
    if destination.exists() or destination.is_symlink():
        raise ValueError("destination already exists; refusing to replace installation")
    if not 1024 <= port <= 65535:
        raise ValueError("choose an unprivileged TCP port")
    if os.getuid() == 0:
        raise ValueError("initialize as the dedicated non-root installation owner")
    if any(c in str(destination) for c in '\n\r\"\'#$\\:'):
        raise ValueError("installation path contains unsupported environment-file characters")
    destination.mkdir(mode=0o700)
    (destination / "secrets").mkdir(mode=0o700)
    (destination / "artifacts").mkdir(mode=0o700)

    def private(name, content):
        fd = os.open(destination / name, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(fd, "w") as file:
            file.write(content)
            file.flush()
            os.fsync(file.fileno())

    password = secrets.token_hex(32)
    private("secrets/db-password", password + "\n")
    private("secrets/database-url", f"postgres://orbit:{password}@postgres:5432/orbit\n")
    private("secrets/operator-token", secrets.token_hex(32) + "\n")
    private("secrets/compute-token", secrets.token_hex(32) + "\n")
    config = {"operator_credential": {"provider": "file", "path": "/run/orbit/operator-token"},
              "ui_directory": "/opt/orbit/ui", "repositories": {},
              "workers": {"compute": {"credential": {"provider": "file", "path": "/run/orbit/compute-token"},
                  "capabilities": ["container.run"], "capacity": {"pool": "local-compute",
                  "resources": {"cpu_millis": 2000, "memory_mib": 512, "gpu": 0}}}}}
    private("server.json", json.dumps(config, indent=2) + "\n")
    private(".env", f'ORBIT_DATA_DIR="{destination}"\nORBIT_UID={os.getuid()}\nORBIT_GID={os.getgid()}\nORBIT_PORT={port}\nORBIT_IMAGE=localhost/orbit:alpha\n')
    return destination


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("destination", type=Path)
    parser.add_argument("--port", type=int, default=7700)
    args = parser.parse_args()
    try:
        location = initialize(args.destination, args.port)
    except (ValueError, OSError) as error:
        parser.exit(1, f"Initialization failed: {error}\n")
    print(f"Initialized private installation: {location}. No service was started; no secrets printed.")
