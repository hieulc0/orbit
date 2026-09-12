# Compute and artifacts

`orbit/v1` supports `container.run`, resource requirements and worker placement.
Definitions containing only compute or engine steps need only `inputs.task`;
repository steps still require a repository binding and a full Git revision.
Existing v0 definitions retain their validation and serialized plan digests.

## Compute contract

See [the executable container definition](../examples/container.yaml). Each
container has an OCI image pinned by SHA-256 digest, an argv command, explicit
CPU (`cpu_millis`), memory (`memory_mib`) and optional GPU count. The operator
must provision that image on a Linux Docker or rootless Podman runner; Orbit
uses `--pull=never`. `ORBIT_CONTAINER_RUNTIME` selects `docker` (default) or
`podman` for the worker or `execute-local`. Podman uses a user namespace that
preserves the worker UID/GID and the cgroupfs manager; its host must delegate
CPU, memory and PID controllers to the worker's user. Provisioned images live in
the operator's runtime image store, outside attempt workspaces.
The runner executes the requested command as the host worker's UID/GID, with
no network, a read-only root filesystem, dropped capabilities, no new
privileges, a bounded tmpfs, a PID limit and CPU/memory limits. Definitions
cannot add host mounts, privileged flags, Docker endpoints or environment secrets.
The OCI runtime remains trusted host infrastructure; this does not qualify a hostile-code
or multi-tenant sandbox.

Direct dependency artifacts are mounted read-only at `/orbit/inputs/<artifact-id>`;
`/orbit/inputs/manifest.json` describes them. The optional regular file
`/orbit/outputs/result` becomes a `data` artifact. A result is limited to 32 MiB.
Symlinks and special files are rejected. Logs and a provenance-bearing
`container_report` are required for a successful completion. Repository testing
continues to receive only its direct coding dependency's artifacts.

The task ID is also the stable idempotency key. Containers receive
`ORBIT_TASK_ID`, `ORBIT_ATTEMPT_ID`, `ORBIT_ATTEMPT_GENERATION` and
`ORBIT_IDEMPOTENCY_KEY`. Attempts use
distinct container names and workspaces. The worker maintains leases while
running and publishing outputs. A separate supervisor owns runtime cleanup;
closing its input pipe after cancellation, lease loss or worker SIGKILL triggers
cleanup. Task timeout also triggers cleanup. Removal retries are bounded. A
failed or unreachable runtime can leave stopping unconfirmed; inspect
the `orbit-<attempt-id>` container in that case. Orbit does not equate revoked
durable ownership with proof that an external process has stopped.

## Capacity and worker pools

Worker capacity is server configuration, never a worker's self-reported claim:

```json
{
  "capabilities": ["container.run", "gpu"],
  "capacity": {
    "pool": "local-compute",
    "resources": {"cpu_millis": 4000, "memory_mib": 8192, "gpu": 2}
  }
}
```

The full worker entry also needs its existing distinct token. `placement.pool`
selects a pool; every `placement.capabilities` entry must be authorized for the
worker. Resource demand must fit its remaining capacity. Claims serialize under
the shared database coordination lock and reserve resources across all active
runs on that worker. GPU device indices are assigned without overlap and pinned
in the attempt and assignment. Docker uses explicit NVIDIA device indices;
Podman uses the corresponding `nvidia.com/gpu=<index>` CDI devices, which the
operator must provision. Hardware GPU execution is not qualified by the CPU
fixtures. Capacity is logical scheduling capacity; the
operator must map identities to actual machines without counting one machine's
capacity repeatedly. Legacy workers and steps without resource requirements keep
their existing behavior and attempt limits.

Registration and claim persist the authorized profile and last-seen time. All
servers must use identical profiles for a worker ID. A conflicting profile is
rejected; use a new worker identity when changing capacity, and drain the old
identity. Resource reservations release on terminal transitions and lease expiry.
An expired process may still exist, as with other at-least-once workers.

`orbit workers` (`GET /workers`) returns profiles without credentials and their
last contact time. `orbit queues` (`GET /queues`) shows ready/active task counts
by capability and pool. These are snapshots, not a claim of host health.

## Artifact providers

The local provider remains the default. It publishes with an immutable link,
file sync and directory sync. S3-compatible providers use conditional object
creation; retransmission accepts the same bytes and rejects conflicting content.
If a PUT response is lost or a concurrent conditional PUT conflicts, a successful
read with the expected length and SHA-256 reconciles the existing publication.
Providers must support conditional writes. Configure S3 using server-local JSON:

```json
{
  "artifact_provider": "objects",
  "artifact_stores": {
    "objects": {
      "bucket": "orbit-artifacts",
      "region": "us-east-1",
      "endpoint": "https://objects.example.invalid",
      "access_key_env": "ORBIT_S3_ACCESS_KEY",
      "secret_key_env": "ORBIT_S3_SECRET_KEY",
      "allow_http": false,
      "prefix": "artifacts"
    }
  }
}
```

Merge these fields with existing server configuration. Provision the bucket and
inject credentials into the server environment; workers receive neither storage
credentials nor signed URLs. HTTP requires an explicit opt-in for local fixtures.
Never commit runtime configuration or real credentials. Keep provider names and
their bucket/endpoint mappings stable for the lifetime of retained artifacts.
The artifact location records its provider, object key and content type; changing
the default affects newly prepared artifacts only. Old records without locations
continue to use the local artifact root.

The same prepare/upload/finalize/read protocol serves both providers. Bytes and
checksum/provenance checks happen outside database coordination locks. After I/O,
the committing transaction rechecks ownership, generation, lease, cancellation,
request identity and immutable artifact metadata. Slow storage therefore cannot
hold the scheduler lock. Publication can leave an unaccepted orphan after lease
loss; only a successful durable finalization/completion grants authority.

The current bounded HTTP transport retains its 32 MiB per-artifact limit. Multipart
upload, presigned transfer, retention/garbage collection, bucket provisioning and
cloud identity federation are future extensions. Local provider storage and the
chosen S3 service must be backed up independently of PostgreSQL.

## Running the compute example

Copy [the compute server configuration](../examples/server-compute.json) to a
local runtime configuration and replace both placeholder tokens with distinct
credentials. Start the server with that configuration, a persistent local
artifact directory and the usual database URL. In a separate terminal, provision
the image in the selected runtime's store, then start its worker:

```sh
podman pull docker.io/library/alpine@sha256:28bd5fe8b56d1bd048e5babf5b10710ebe0bae67db86916198a6eec434943f8b

# ORBIT_TOKEN here is the configured compute worker token.
ORBIT_CONTAINER_RUNTIME=podman orbit worker --capability container.run \
  --workspaces /absolute/path/to/disposable-compute-workspaces
```

With the operator token in a separate terminal, use `orbit validate
examples/container.yaml`, `orbit run examples/container.yaml`, `orbit inspect
RUN_ID`, `orbit workers`, `orbit queues`, and `orbit artifact RUN_ID ARTIFACT_ID
--output result`. The server does not need a repository binding for this example.
For Docker, provision the same image with Docker and select `docker` instead.
`execute-local` also accepts a saved container assignment and the same runtime
selection; it writes new local artifacts without updating the engine.

Provider implementation uses the [object_store S3 client](https://docs.rs/object_store/0.12.5/object_store/aws/struct.AmazonS3Builder.html).
Container restrictions use documented [Docker run options](https://docs.docker.com/reference/cli/docker/container/run/).
The alternate runtime follows [Podman run options](https://docs.podman.io/en/stable/markdown/podman-run.1.html).
