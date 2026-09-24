# Google Antigravity ACP Adapter Guide

This document describes Orbit's **Google Antigravity Agent Client Protocol (ACP) adapter**, packaging, environment configuration, and credential staging. Operator-local personal OAuth enrollment and fresh-runtime ACP reuse are live-qualified. The same catalog credential also has a file-backed `agy-cli` representation and one qualified `/usage` status observation; see [provider status discovery](../operations/provider-status-discovery.md). The ACP↔agy provider-account binding remains UNVERIFIED.

> Runtime packaging note (2026-09-24): the fixture instructions below describe
> historical packaging and are not a reproducible build path. The new pinned
> 1.1.1 runtime recipe is defined in
> [Antigravity compatibility](../operations/acp-agent-compatibility.md#automated-packaging-pipeline).
> The new image has been built and qualified for credential-free ACP initialize;
> enrollment and execution qualification for that new image remain open.

## Operator credential enrollment (Phase A)

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
`LocalPrivateSecretBackend` locator. No token, URL, locator, or physical path
goes into PostgreSQL or user-facing output. A fresh runtime stages only those
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

Existing manual `~/.orbit/credentials/...` and worker AuthLease are unchanged
and are not migrated. Gemini Enterprise, API key, Agent Platform, interactive
agy OAuth enrollment/capture automation, and provider-specific logout are not
implemented here. The `agy-cli` representation and one bounded `/usage`
observation are qualified separately; its credential-scoped snapshot is
UNKNOWN because no readiness threshold or exact-model scope is justified. The
agy 1.2.9 artifact remains operator-supplied and is not officially
artifact-verified. The Antigravity ACP↔agy account binding remains UNVERIFIED.

---

## 1. Overview and Architecture

The Google Antigravity ACP adapter enables Orbit to run Google Antigravity agent harnesses within isolated execution environments (such as Podman or Docker sandboxes) via the Agent Client Protocol (ACP).

### Architectural Components

```
+-------------------------------------------------------------------------+
|                              Orbit Runner                               |
|  - Task Dispatcher & Trajectory Coordinator                             |
|  - Private Auth-File Lease (lock, stage, write-back)                    |
|  - Client Tool Handlers (File system, Terminal Execution)              |
+-------------------------------------------------------------------------+
                                   |
                          Agent Client Protocol
                       (Standard I/O RPC / JSON)
                                   |
                                   v
+-------------------------------------------------------------------------+
|                     Containerized Sandbox Environment                   |
|                                                                         |
|  +-------------------------------------------------------------------+  |
|  |                   Google Antigravity ACP Adapter                  |  |
|  |                     (/opt/antigravity/agy_acp_server.par)         |  |
|  |                                                                   |  |
|  |  +-------------------------------------------------------------+  |  |
|  |  |                   Antigravity Local Harness                 |  |  |
|  |  |             (/opt/antigravity/localharness_external)        |  |  |
|  |  +-------------------------------------------------------------+  |  |
|  +-------------------------------------------------------------------+  |
|                                  |                                      |
|    - Settings: /orbit/home/.gemini/antigravity-acp/settings.json        |
|    - Auth Token: /orbit/home/.gemini/antigravity-acp/acp_token.json     |
|    - Workspace Root: /orbit/home/workspace                              |
+-------------------------------------------------------------------------+
```

### Key Responsibilities
1. **Orchestration**: Orbit acts as the host runner, initializing the container runtime and launching the ACP server.
2. **Standard Protocol**: The adapter (`agy_acp_server.par`) communicates with Orbit over ACP standard streams, translating higher-level agent actions into tool invocations.
3. **Client Callbacks**: File operations (`client_view_file`, `client_edit_file`, `client_create_file`) and terminal executions (`run_command`) execute within the target workspace via client callbacks.
4. **Trajectory & Session Tracking**: Conversation state and trajectories are captured in SQLite databases located at `$GEMINI_HOME/antigravity-acp/conversations/` with conversation UUID identifiers.

---

## 2. Container Packaging with `prepare-antigravity-fixture.sh`

The `scripts/prepare-antigravity-fixture.sh` script automates the creation and staging of reproducible container fixtures containing the necessary Antigravity binaries, runtime configurations, and workspace scaffolding.

### Purpose of the Packaging Script
- Assembles binary dependencies into `/opt/antigravity/`.
- Sets up non-root execution users (`hieulc` or standard workspace user) and permissions.
- Prepares CA certificate stores for secure gRPC and HTTPS outbound connections.
- Generates base configuration schemas for ACP settings and authentication templates.

### Script Execution and Usage

```bash
# Run fixture preparation script from the repository root
./scripts/prepare-antigravity-fixture.sh [OPTIONS]
```

#### Common Options and Environment Variables
- `--output-dir <DIR>`: Specifies the target fixture output directory (default: `build/fixtures/antigravity-acp`).
- `--base-image <IMAGE>`: Defines the base OCI/container image (e.g., Ubuntu/Debian minimal base).
- `--binaries-path <PATH>`: Directory containing `agy_acp_server.par` and `localharness_external`.
- `PINNED_BASE`: Specifies the Git commit base revision (e.g., `1bf4fd8fbe558b9d1bbacead0d03b997480ad4b4`) to anchor fixture builds to a known deterministic state.

### Fixture Directory Layout
When packaged, the container root filesystem contains the following layout:

```
/
├── opt/
│   └── antigravity/
│       ├── agy_acp_server.par           # Main ACP entry point server
│       └── localharness_external        # Underlying agent execution harness
└── orbit/
    └── home/
        ├── .gemini/
        │   ├── antigravity/
        │   │   └── bin/
        │   └── antigravity-acp/
        │       ├── settings.json        # Adapter configuration
        │       ├── acp_token.json       # Operator-provisioned auth state, staged by Orbit
        │       └── conversations/       # SQLite session storage
        └── workspace/                   # Active user workspace mount
```

---

## 3. Legacy manual authentication files and Orbit's local lease

This section describes the pre-catalog `AuthLease` path only; it is not the
production `orbit credential add antigravity` enrollment path above.

`NO_BROWSER=1` disables interactive browser launch; it does not authenticate
the ACP process or obtain provider credentials. The legacy manual worker
configuration does not implement Google login, OAuth/token issuance,
credential conversion, or provider refresh. Its operator must provision the
private source files configured in the worker's `auth.path` and `auth.files`
mapping.

For the current example, Orbit treats `acp_token.json` and `settings.json` as
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

---

## 4. Runtime Environment Variables

Configure the following environment variables when running the adapter container:

| Variable | Description | Example Value |
| :--- | :--- | :--- |
| `ANTIGRAVITY_AGENT` | Flags the environment as an Antigravity agent process | `1` |
| `ANTIGRAVITY_HARNESS_PATH` | Absolute path to the external harness binary | `/opt/antigravity/localharness_external` |
| `ANTIGRAVITY_CONVERSATION_ID` | UUID for the active conversation session | `a553f2b2-aff2-4630-8530-66cc0b58948b` |
| `ANTIGRAVITY_TRAJECTORY_ID` | Trajectory tracking identifier | `a553f2b2-aff2-4630-8530-66cc0b58948b` |
| `AGY_ACP_FORCE_FILE_STORAGE` | Enforces SQLite file-backed conversation persistence | `1` |
| `GEMINI_HOME` | Base path for Gemini and Antigravity user configurations | `/orbit/home/.gemini` |
| `HOME` | Home directory of the container execution user | `/orbit/home` |
| `NO_BROWSER` | Disables interactive web browser triggers | `1` |
| `SSL_CERT_FILE` | Path to the CA certificates bundle for TLS verification | `/etc/ssl/certs/ca-certificates.crt` |
| `REQUESTS_CA_BUNDLE` | CA bundle path for HTTP client requests | `/etc/ssl/certs/ca-certificates.crt` |

---

## 5. End-to-End Execution Workflow

### Step 1: Prepare the Fixture
Build the container image using the fixture packaging script:
```bash
./scripts/prepare-antigravity-fixture.sh --base-image debian:bookworm-slim
```

### Step 2: Provision the configured private auth store
Provision the operator-controlled credential files at the private source
directory configured by `auth.path`. Orbit stages those files into the runtime
HOME; it does not create or acquire them. Never put credential contents in a
Definition, source-controlled example, or command history.

### Step 3: Run the ACP Adapter Server
Start `agy_acp_server.par` inside the container:
```bash
export ANTIGRAVITY_AGENT=1
export ANTIGRAVITY_HARNESS_PATH=/opt/antigravity/localharness_external
export AGY_ACP_FORCE_FILE_STORAGE=1
export NO_BROWSER=1

/opt/antigravity/agy_acp_server.par
```

### Step 4: Dispatch Tasks via Orbit
Orbit connects to standard I/O of the ACP process, sending agent requests and handling tool callbacks for file modification and command execution in `/orbit/home/workspace`.

---

## 6. Troubleshooting and Verification

- **Legacy manual credential authentication error**: Verify the operator-provisioned `acp_token.json` exists and is readable. This applies only to the unchanged manual AuthLease path; the new registry enrollment validates a fresh ACP representation before marking it enrolled.
- **Harness Path Not Found**: Ensure `ANTIGRAVITY_HARNESS_PATH` points directly to the executable binary at `/opt/antigravity/localharness_external`.
- **Database Locks / Storage Issues**: Ensure `AGY_ACP_FORCE_FILE_STORAGE=1` is set and the directory `/orbit/home/.gemini/antigravity-acp/conversations/` has write permissions.
- **TLS Handshake Failures**: Confirm `SSL_CERT_FILE` and `REQUESTS_CA_BUNDLE` correctly point to the system CA certificate bundle.
