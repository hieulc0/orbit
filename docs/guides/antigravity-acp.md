# Antigravity ACP runtime

Orbit runs a pinned Antigravity ACP server in a supervised rootless Podman
container. Repository effects use Orbit's client broker; the provider process
has a private control HOME and no repository mount. See
[ACP worker setup](acp-coding.md) for worker authorization, resource allocation,
network policy, repository confinement and cleanup.

The supported image is Orbit's `agy_acp_server_1.1.1-orbit-terminal-v2` variant.
The unmodified Google distribution routes native commands inside its harness;
client terminal capabilities alone do not mediate those commands. Orbit's
versioned overlay removes native command execution and local file fallback and
exposes client-terminal calls. Keep the original distribution and this variant
as separate runtime identities.

Credential-free initialization and catalog-owned OAuth enrollment with fresh
runtime reuse have been qualified for the pinned image below. Model execution,
account scope and separately hosted worker acceptance require their own evidence.
[Adapter compatibility](../operations/acp-agent-compatibility.md) and
[provider status qualification](../operations/provider-status-discovery.md)
preserve observed results; enrollment success is not model readiness.

## Build and pin the runtime

Use the tracked [runtime builder](../../scripts/build-antigravity-runtime.sh).
It takes exactly one directory containing regular, non-symlink
`agy_acp_server.par` and `localharness_external` files from the reviewed 1.1.1
release. It verifies their hashes, the tracked patcher, terminal client,
Containerfile and group overlay, and the deterministic patched output. Exact
Python 3.14.7 is required on the build host to generate the pinned bytecode.
Input hashes are recorded in the
[compatibility record](../operations/acp-agent-compatibility.md#automated-packaging-pipeline).

Provision the exact base separately, then build from the repository root:

```sh
podman --remote=false --cgroup-manager=cgroupfs pull --arch amd64 \
  gcr.io/distroless/base-nossl-debian13@sha256:792f51c506fc67f7eaa38093f6d4937a053cebb79ec0e7c3b7746f6bbba85606

bash scripts/build-antigravity-runtime.sh /path/to/reviewed/antigravity-acp-artifacts
```

The build uses `--pull=never`, `--network=none`, OCI format and epoch timestamps.
It refuses an existing output tag and never overwrites local runtime evidence.
The image contains the pinned server and external harness under
`/opt/antigravity/`; it does not import host CA files or install packages during
build. Review the printed immutable image digest. A build reports `UNQUALIFIED`;
reusing qualification requires the exact reviewed image identity and scope.
A different digest needs separate qualification. The legacy
`prepare-antigravity-fixture.sh` is not the current reproducible setup path.

## Operator credential enrollment

`orbit credential add antigravity` provides the operator-local
`oauth-personal` adapter. It requires current catalog migrations in the durable
PostgreSQL database, `ORBIT_DATABASE_URL_FILE`, and the exact
local rootless Podman image
`localhost/orbit-antigravity-runtime:agy_acp_server_1.1.1-orbit-terminal-v2@sha256:3e7415f6f732ae4168b98a6fb0e14e0fba965020cf5cc1fc5a3b35867b4cf830`.
The adapter verifies the local digest and launches with `--pull=never`.

The operator completes the real Google login in the terminal during enrollment:

```sh
ORBIT_DATABASE_URL_FILE=/path/to/private/control-plane-url \
  cargo run --locked -- credential add antigravity \
  --name antigravity-oauth-test --auth-method oauth-personal
```

The CLI does not use the API server or a worker. It first records a pending
credential and `acp` representation. ACP `initialize` must advertise the
agent-managed `oauth-personal` method; Orbit then sends ACP `authenticate` and
displays the one-time Google URL only in the operator terminal. The pinned
provider chooses a dynamic `127.0.0.1` callback port. Only this short-lived,
operator-initiated enrollment profile uses rootless Podman host networking so
the host browser reaches that loopback listener. It has a fresh owner-only
HOME, no existing credential mounts, no repository/workspace, no broker, no
agent tools, no ACP session or prompt, and bounded stdio. Normal agent runtime
network policy is unchanged. The CLI does not open a browser itself.
Ctrl-C during the authentication wait follows the same bounded container
removal and disposable-HOME cleanup path as other enrollment failures.

After provider authentication, the adapter captures only
`.gemini/antigravity-acp/acp_token.json` and `settings.json` from the disposable
HOME. Their bytes form one versioned opaque bundle under one
`LocalPrivateSecretBackend` locator. PostgreSQL retains that logical locator,
not the token bytes or physical path. Runtime output and authentication URLs
are excluded from stored diagnostics; the enrollment URL is shown only in the
operator terminal. A fresh runtime stages only those
files and calls ACP `authenticate(oauth-personal)` again. A new login URL fails
reuse validation; only successful noninteractive reuse lets the catalog move
from pending to enrolled. This check may contact provider OAuth/onboarding
endpoints but never starts a model session. The adapter does not invoke ACP
`auth.logout`: local disable/revoke and provider logout are distinct. Provider
logout remains unimplemented pending separate semantics review.

If a secret write succeeds but validation or database finalization fails, the
pending representation retains its opaque locator. It remains unusable and is
an explicit reconciliation/GC candidate, not automatically deleted. Only
unreferenced secrets older than an operator-reviewed retention threshold should
be eligible for future explicit GC. Enrollment does not promote availability
to READY. The registry catalog ID keeps new identity evidence distinct from
pre-catalog credentials, even when provider/reference/generation coincide.

Existing manual `~/.orbit/credentials/...` and worker AuthLease are not migrated
to the catalog. Gemini Enterprise, API-key and Agent Platform enrollment and
provider-specific logout are not implemented by this adapter. For agy login or
import through `credential add-representation`, use the
[credential registry contract](../architecture/credential-registry.md).
The `agy-cli` representation and one bounded `/usage` observation are qualified separately; its credential-scoped snapshot is
UNKNOWN because no readiness threshold or exact-model scope is justified. The
agy 1.2.9 artifact remains operator-supplied and is not officially
artifact-verified. The Antigravity ACP↔agy account binding remains UNVERIFIED.

## Runtime environment and storage

The pinned image supplies these defaults:

| Variable | Value and purpose |
| --- | --- |
| `ANTIGRAVITY_HARNESS_PATH` | `/opt/antigravity/localharness_external`, the pinned executable |
| `GEMINI_HOME` | `/orbit/home/.gemini`, private provider state |
| `AGY_ACP_FORCE_FILE_STORAGE` | `1`, file-backed conversation storage |
| `NO_BROWSER` | `1`, suppress automatic browser launch; not authentication |
| `SSL_CERT_FILE`, `REQUESTS_CA_BUNDLE` | `/etc/ssl/certs/ca-certificates.crt`, image CA bundle |

Orbit supplies the fresh HOME under `/orbit/home`. ACP-visible repository paths
under `/orbit/home/workspace` map through the client broker to the actual Attempt
workspace; they do not grant the provider direct filesystem access. Provider
conversation databases under `$GEMINI_HOME/antigravity-acp/conversations/` are
private runtime state, not authoritative Orbit workflow state or accepted
transcripts. Do not export the control HOME.

Changing launch environment, resources or network policy requires a new launch
pin. Normal coding uses the worker's reviewed provider network policy; host
networking does not restrict egress to the provider. Repository commands follow
the separately authorized tool profile and have no network.

## Legacy manual authentication compatibility

This section describes the pre-catalog `AuthLease` path only; it is not the
production `orbit credential add antigravity` enrollment path described above.

`NO_BROWSER=1` disables interactive browser launch; it does not authenticate
the ACP process or obtain provider credentials. The legacy manual worker
configuration does not implement Google login, OAuth/token issuance,
credential conversion, or provider refresh. Its operator must provision the
private source files configured in the worker's `auth.path` and `auth.files`
mapping.

For the manual configuration, Orbit treats `acp_token.json` and `settings.json` as
opaque files. `settings.json` is not validated as an authentication schema by
Orbit. This repository does not establish whether the source token file was
created by Antigravity desktop, `agy`, the ACP runtime, or another login
workflow; do not infer its origin from its filename or directory.

`AuthLease` is a local mutual-exclusion and staging lease, not a provider auth
lease. It locks the configured private store, copies mapped source files into
the new control HOME with mode `0600`, and creates an active marker. After the
runtime is confirmed stopped, cleanup copies the staged file contents back to
the configured source store and removes the marker. If runtime cleanup or
write-back is uncertain, the marker remains and the store is quarantined.
Orbit does not independently refresh credentials; any mutation returned in a
staged file is from the runtime and is copied back by cleanup. The control
HOME is isolated from repository workspaces and is not a credential-origin
boundary.

The qualified `agy-cli` representation uses a separate file-backed token
artifact, not these ACP files. The operator intentionally selected the same
Google account for both enrollments, but no machine-verifiable common provider
identity is available; provisioning either representation does not prove that
the other uses the same account.

## Troubleshooting

- **Enrollment or reuse failure:** keep the credential pending and inspect the
  bounded failure and cleanup result. A runtime requesting another login URL has
  not demonstrated noninteractive reuse; do not mark it enrolled manually.
- **Manual authentication failure:** verify only the mapped private source files
  are readable. `NO_BROWSER=1` does not acquire credentials, and filenames do not
  prove their provider-account origin.
- **Harness missing:** verify the pinned image and
  `ANTIGRAVITY_HARNESS_PATH`; do not substitute a host executable.
- **Conversation storage failure:** verify the private staged HOME is writable
  and file-backed storage is enabled. Reconcile uncertain cleanup before reuse;
  do not share one active auth store among runtimes.
- **TLS failure:** verify the image CA bundle and its configured paths. A host CA
  copy would change the packaging boundary and requires an explicitly reviewed
  runtime identity.
