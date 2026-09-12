"""Offline local-artifact backup/restore. Uses PostgreSQL tools inside a named container.

Only restore trusted bundles into a new, empty, disposable/replacement database.
No database, table, container, volume or existing file is deleted by this utility.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import stat
import subprocess
import uuid


class Database:
    def __init__(self, runtime, container, user="orbit", database="orbit"):
        if runtime not in ("docker", "podman") or any(not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.-]*", v) for v in (container, user, database)):
            raise ValueError("invalid runtime/container/database identity")
        self.prefix = [runtime] + (["--cgroup-manager=cgroupfs"] if runtime == "podman" else [])
        self.container, self.user, self.database = container, user, database

    def command(self, tool, *args):
        return self.prefix + ["exec", "-i", self.container, tool, "-U", self.user, "-d", self.database, *args]

    def sql(self, text):
        result = subprocess.run(self.command("psql", "-X", "-A", "-t", "-v", "ON_ERROR_STOP=1"),
                                input=text.encode(), stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=30)
        if result.returncode:
            raise RuntimeError("PostgreSQL query failed; inspect the named database locally")
        return result.stdout.decode().strip()

    def offline(self):
        connections = int(self.sql("SELECT count(*) FROM pg_stat_activity WHERE datname=current_database() AND pid<>pg_backend_pid() AND backend_type='client backend';"))
        if connections:
            raise ValueError("other database clients remain; stop all Orbit servers and writers first")

    def accepted(self):
        documents = json.loads(self.sql("SELECT COALESCE(jsonb_agg(document),'[]'::jsonb) FROM orbit_runs;"))
        accepted = {}
        for run in documents:
            ids = {value for task in run["tasks"] for value in task["accepted_outputs"]}
            artifacts = {a["id"]: a for a in run["artifacts"]}
            for identity in ids:
                artifact = artifacts[identity]
                if artifact.get("location") and artifact["location"]["provider"] != "local":
                    raise ValueError("S3/object-store artifacts need a separate coordinated snapshot; this utility supports local storage only")
                uuid.UUID(identity)
                accepted[identity] = {"sha256": artifact["checksum"], "size": artifact["size"]}
        return accepted


def regular(path):
    if not stat.S_ISREG(path.lstat().st_mode):
        raise ValueError("backup paths must be regular files, never symlinks/devices")


def private_directory(path):
    path = Path(path).absolute()
    if path.exists() or path.is_symlink():
        raise ValueError("destination exists; use a NEW destination")
    path.mkdir(mode=0o700)
    return path


def fingerprint(path):
    regular(path)
    digest, size = hashlib.sha256(), 0
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(fd, "rb") as stream:
        if not stat.S_ISREG(os.fstat(stream.fileno()).st_mode):
            raise ValueError("non-regular backup file")
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
            size += len(chunk)
    return {"sha256": digest.hexdigest(), "size": size}


def write_json(path, value):
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(fd, "w") as stream:
        json.dump(value, stream, indent=2)
        stream.flush()
        os.fsync(stream.fileno())


def copy_regular(source, destination):
    regular(source)
    before = fingerprint(source)
    src = os.open(source, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    dst = os.open(destination, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(src, "rb") as reader, os.fdopen(dst, "wb") as writer:
        if not stat.S_ISREG(os.fstat(reader.fileno()).st_mode):
            raise ValueError("non-regular source file")
        shutil.copyfileobj(reader, writer, 1024 * 1024)
        writer.flush()
        os.fsync(writer.fileno())
    if fingerprint(destination) != before:
        raise ValueError("source changed during offline copy")
    return before


def create(database, artifacts, output):
    artifacts, output = Path(artifacts).absolute(), Path(output).absolute()
    if artifacts.is_symlink() or not artifacts.is_dir() or output == artifacts or artifacts in output.parents:
        raise ValueError("use a real artifact directory and a separate NEW backup destination")
    database.offline()
    accepted = database.accepted()
    sources = sorted(artifacts.iterdir())
    for source in sources:
        uuid.UUID(source.name)
        regular(source)
    for identity, expected in accepted.items():
        if fingerprint(artifacts / identity) != expected:
            raise ValueError("accepted artifact checksum mismatch")
    output = private_directory(output)
    (output / "artifacts").mkdir(mode=0o700)
    dump = output / "database.dump"
    fd = os.open(dump, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(fd, "wb") as stream:
        result = subprocess.run(database.command("pg_dump", "--format=custom", "--no-owner", "--no-acl"), stdout=stream, stderr=subprocess.PIPE, timeout=300)
        if result.returncode:
            raise RuntimeError("pg_dump failed; partial backup retained without a valid manifest")
        stream.flush()
        os.fsync(stream.fileno())
    files = {"database.dump": fingerprint(dump)}
    for source in sources:
        files[f"artifacts/{source.name}"] = copy_regular(source, output / "artifacts" / source.name)
    database.offline()
    if database.accepted() != accepted:
        raise ValueError("database changed during offline backup")
    manifest = {"format": "orbit-backup/v1", "files": files, "accepted_artifacts": accepted,
                "notice": "Sensitive offline backup, not a public evidence export. Configuration/secrets and S3 are not included."}
    write_json(output / "manifest.json", manifest)
    return manifest


def verify(bundle):
    bundle = Path(bundle).absolute()
    if bundle.is_symlink() or not bundle.is_dir():
        raise ValueError("bundle must be a real directory")
    regular(bundle / "manifest.json")
    manifest = json.loads((bundle / "manifest.json").read_text())
    if manifest.get("format") != "orbit-backup/v1" or "database.dump" not in manifest.get("files", {}):
        raise ValueError("unsupported or incomplete backup")
    if (bundle / "artifacts").is_symlink() or not (bundle / "artifacts").is_dir():
        raise ValueError("invalid backup artifact directory")
    for name, expected in manifest["files"].items():
        if name != "database.dump":
            parts = Path(name).parts
            if len(parts) != 2 or parts[0] != "artifacts" or str(Path(name)) != name:
                raise ValueError("unsafe backup manifest path")
            uuid.UUID(parts[1])
        if fingerprint(bundle / name) != expected:
            raise ValueError("backup checksum mismatch")
    for identity, expected in manifest["accepted_artifacts"].items():
        uuid.UUID(identity)
        if manifest["files"].get(f"artifacts/{identity}") != expected:
            raise ValueError("accepted artifact missing from backup")
    return manifest


def restore(database, bundle, artifacts):
    manifest = verify(bundle)  # Validate before any target write.
    database.offline()
    tables = int(database.sql("SELECT count(*) FROM pg_tables WHERE schemaname NOT IN ('pg_catalog','information_schema');"))
    if tables:
        raise ValueError("restore target database is not empty; refusing to overwrite")
    artifacts = private_directory(artifacts)
    for name in manifest["files"]:
        if name.startswith("artifacts/"):
            if copy_regular(Path(bundle) / name, artifacts / Path(name).name) != manifest["files"][name]:
                raise ValueError("backup changed after verification")
    dump = Path(bundle) / "database.dump"
    fd = os.open(dump, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(fd, "rb") as stream:
        if not stat.S_ISREG(os.fstat(stream.fileno()).st_mode):
            raise ValueError("non-regular database dump")
        digest = hashlib.file_digest(stream, "sha256").hexdigest()
        if {"sha256": digest, "size": stream.tell()} != manifest["files"]["database.dump"]:
            raise ValueError("database dump changed after verification")
        stream.seek(0)
        result = subprocess.run(database.command("pg_restore", "--no-owner", "--no-acl", "--exit-on-error", "--single-transaction"),
                                stdin=stream, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=300)
    if result.returncode:
        raise RuntimeError("pg_restore failed; no automatic deletion or retry was performed")
    if database.accepted() != manifest["accepted_artifacts"]:
        raise ValueError("restored database and artifact snapshot disagree")
    return manifest


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=["create", "restore", "verify"])
    parser.add_argument("--runtime", choices=["docker", "podman"], default="docker")
    parser.add_argument("--database-container")
    parser.add_argument("--database-user", default="orbit")
    parser.add_argument("--database", default="orbit")
    parser.add_argument("--artifacts", type=Path)
    parser.add_argument("--bundle", type=Path, required=True)
    parser.add_argument("--offline-confirmed", action="store_true")
    args = parser.parse_args()
    try:
        if args.action == "verify":
            manifest = verify(args.bundle)
        else:
            if not args.offline_confirmed or not args.database_container or not args.artifacts:
                parser.error("create/restore require --offline-confirmed, --database-container and --artifacts")
            db = Database(args.runtime, args.database_container, args.database_user, args.database)
            manifest = create(db, args.artifacts, args.bundle) if args.action == "create" else restore(db, args.bundle, args.artifacts)
        print(json.dumps({"status": "verified", "format": manifest["format"], "files": len(manifest["files"])}))
    except (ValueError, OSError, RuntimeError, subprocess.TimeoutExpired) as error:
        parser.exit(1, f"Backup operation failed: {error}\n")
