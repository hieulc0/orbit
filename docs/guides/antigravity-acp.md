# Google Antigravity ACP Adapter Guide

This document provides a comprehensive guide for configuring, packaging, and operating the **Google Antigravity Agent Client Protocol (ACP) adapter** within Orbit. It details the runtime architecture, fixture packaging using `scripts/prepare-antigravity-fixture.sh`, environment configuration, and auth leasing mechanics.

---

## 1. Overview and Architecture

The Google Antigravity ACP adapter enables Orbit to run Google Antigravity agent harnesses within isolated execution environments (such as Podman or Docker sandboxes) via the Agent Client Protocol (ACP).

### Architectural Components

```
+-------------------------------------------------------------------------+
|                              Orbit Runner                               |
|  - Task Dispatcher & Trajectory Coordinator                             |
|  - Auth Lease Manager (OAuth / Service Token Provider)                  |
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
        │       ├── acp_token.json       # Injected active auth lease token
        │       └── conversations/       # SQLite session storage
        └── workspace/                   # Active user workspace mount
```

---

## 3. Auth Leasing Mechanics

In automated and containerized environments, interactive browser logins are disabled (`NO_BROWSER=1`). Antigravity ACP relies on an **auth leasing** mechanism to securely obtain and maintain credentials.

### Authentication Configuration (`settings.json`)

The adapter configuration file is located at `/orbit/home/.gemini/antigravity-acp/settings.json`:

```json
{
  "auth": {
    "type": "oauth-personal"
  }
}
```

Supported auth types include `oauth-personal` and service account / token delegation modes.

### Lease Lifecycle and Provisioning

```
  +--------------+               +------------------+               +-------------------+
  | Orbit Runner |               | Auth Lease Store |               |  Antigravity ACP  |
  +--------------+               +------------------+               +-------------------+
         |                                |                                   |
         | 1. Request Auth Lease          |                                   |
         |------------------------------->|                                   |
         | 2. Issue Leased Token          |                                   |
         |<-------------------------------|                                   |
         |                                                                    |
         | 3. Mount/Write /orbit/home/.gemini/antigravity-acp/acp_token.json  |
         |------------------------------------------------------------------->|
         |                                                                    |
         | 4. Launch Container & Initialize ACP Session                       |
         |------------------------------------------------------------------->|
         |                                                                    |
         | 5. Periodic Lease Check & Dynamic Token Refresh (before TTL end)   |
         |------------------------------------------------------------------->|
```

### Key Auth Leasing Principles
1. **Short-Lived Leases**: Tokens are issued with a finite Time-To-Live (TTL) to adhere to least-privilege security principles.
2. **Headless Ingestion**: The token file (`acp_token.json`) is read by `agy_acp_server.par` at initialization and refreshed during runtime without requiring user intervention.
3. **Automatic Renewal**: Orbit's auth lease manager monitors active sessions and writes updated credentials to the container's token path before expiration.
4. **Sandbox Isolation**: Auth tokens remain constrained to the container sandbox, and ephemeral storage ensures no credentials persist post-run.

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

### Step 2: Inject Auth Lease
Obtain an auth lease from Orbit and populate `/orbit/home/.gemini/antigravity-acp/acp_token.json`:
```bash
mkdir -p /orbit/home/.gemini/antigravity-acp
echo '{"auth":{"type":"oauth-personal"}}' > /orbit/home/.gemini/antigravity-acp/settings.json
# Auth lease manager injects acp_token.json here
```

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

- **Token Expired or Missing**: Verify that `/orbit/home/.gemini/antigravity-acp/acp_token.json` exists, is readable by the user, and has a valid unexpired lease.
- **Harness Path Not Found**: Ensure `ANTIGRAVITY_HARNESS_PATH` points directly to the executable binary at `/opt/antigravity/localharness_external`.
- **Database Locks / Storage Issues**: Ensure `AGY_ACP_FORCE_FILE_STORAGE=1` is set and the directory `/orbit/home/.gemini/antigravity-acp/conversations/` has write permissions.
- **TLS Handshake Failures**: Confirm `SSL_CERT_FILE` and `REQUESTS_CA_BUNDLE` correctly point to the system CA certificate bundle.
