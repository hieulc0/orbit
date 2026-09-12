# Single-host deployment

The alpha packaging is a Linux API/UI server image with PostgreSQL and local
artifacts. Workers run separately on trusted hosts. This is not an HA,
hostile-code sandbox or public-internet production configuration. Read the exact
[qualification boundary](qualification.md) before rollout.

The root `docker-compose.yml` provisions disposable test services. `deploy/` uses
separate names, volumes and credentials. Never reuse test data in a deployment.

## Build and initialize

Build from the repository root with either runtime. Base images are digest-pinned
in [the Containerfile](../../deploy/Containerfile); no image is published.

```sh
docker build --file deploy/Containerfile --tag localhost/orbit:alpha .
# Alternative; docker format preserves image HEALTHCHECK metadata:
podman build --format docker --file deploy/Containerfile --tag localhost/orbit:alpha .
```

The final image contains the release binary, CA certificates and compiled UI, not
source workspaces, credentials, compilers or a runtime socket. Default UID is
10001. The [build-context allowlist](../../.dockerignore) excludes local config and
evidence. Record the resulting immutable image ID/digest for an installation;
`localhost/orbit:alpha` is only a local build tag.

As a dedicated non-root installation owner, choose a NEW directory outside the
checkout whose parent already exists and is appropriately owned:

```sh
python3 scripts/init-deployment.py /srv/orbit
```

The initializer refuses existing destinations and root execution. It writes
private `server.json`, `.env`, `secrets/` and an empty `artifacts/`; it does not
start services or print credentials. `.env` contains paths/UID/port, not tokens.
Keep the installation private and outside Git.

## Docker Compose

```sh
docker compose --env-file /srv/orbit/.env -f deploy/docker/compose.yaml config --quiet
docker compose --env-file /srv/orbit/.env -f deploy/docker/compose.yaml up -d --wait
ORBIT_URL=http://127.0.0.1:7700 target/release/orbit health
ORBIT_TOKEN_FILE=/srv/orbit/secrets/operator-token target/release/orbit run examples/timer.yaml
```

Build the host binary with `cargo build --locked --release` for the last commands.
Open `http://127.0.0.1:7700`; enter the operator credential locally in the console,
never in a URL. PostgreSQL has no host port. The server uses a read-only root,
dropped capabilities, no-new-privileges, resource limits and an artifact write
mount. Compose maps the installation owner's UID/GID for private file access.

Bindings are loopback-only. Remote access needs an explicitly selected TLS/auth
boundary and network policy; these templates do not install a public proxy.
Adjust resource capacity and backup policy before real workloads.

## Rootless Podman / Quadlet

Use a dedicated login user, subordinate UID/GID ranges, cgroup v2 and a working
systemd user manager. Initialize that user's absolute `.local/share/orbit` path
instead of `/srv/orbit`; its parent must exist. Build/pull images into that user's
rootless store. Copy the four [Quadlet files](../../deploy/podman) into the user's
`~/.config/containers/systemd/` after checking names/paths for collisions.

```sh
systemctl --user daemon-reload
systemctl --user start orbit-server.service
systemctl --user status orbit-server.service orbit-postgres.service
```

Quadlet generates units; source `[Install]` sections govern startup. Enable login
lingering only if the host owner wants services to survive logout. The server
uses `keep-id:uid=10001,gid=10001`; SELinux labels distinguish private mounts from
shared secrets. PostgreSQL's named volume survives service removal: never remove
it as routine cleanup. See [official Quadlet documentation](https://docs.podman.io/en/latest/markdown/podman-systemd.unit.5.html).

## Trusted host workers

Each worker needs a matching host binary and a distinct server-authorized identity.
The generated deployment has one `compute` identity with `container.run` and a
2-CPU/512-MiB envelope. Copy its token privately to the worker host, not the
operator credential.

```sh
ORBIT_URL=http://127.0.0.1:7700 \
ORBIT_TOKEN_FILE=/absolute/private/compute.token \
ORBIT_CONTAINER_RUNTIME=podman \
orbit worker --capability container.run --workspaces /absolute/disposable/workspaces
```

Pre-pull digest-pinned task images in that user's runtime store. Rootless Podman
needs its normal user runtime directory and image-store access. Docker socket
access is powerful host authority; keep it off the API server. Repository workers
need Git, approved coding commands/test executables and pinned repository paths
available on the worker host. Never point qualification at a developer checkout.

[The systemd worker template](../../deploy/systemd/orbit-worker@.service) uses an
`orbit-worker` account, credential loading, a 30-second drain and private umask.
Provision the binary, `/etc/orbit/workers/INSTANCE.env`, private `INSTANCE.token`
and workspace ownership before installation. The [environment example](../../deploy/systemd/compute.env.example)
contains no credential. User creation, runtime directory/session setup and Docker
access are host-specific, not automatic. Command agents additionally require
`--agent-runtime`; see [their guide](../guides/command-agent.md).

## Verify before use

```sh
python3 scripts/backup-drill.py --runtime podman --image localhost/orbit:alpha
```

The pinned PostgreSQL image must already exist in the selected runtime. This
drill creates randomly named labelled fixtures, tests restart/rotation/restore,
then removes only its owned containers and anonymous database volumes. Private
files/results remain under `target/deployment-smoke`. It does not touch the
installation above. Continue with [observability](observability.md),
[backups](backup-restore.md) and [upgrades](upgrades.md).
