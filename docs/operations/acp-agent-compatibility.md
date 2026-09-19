# Antigravity and Claude ACP compatibility

Source review: 2026-09-13; status updated 2026-09-19. The requested rollout order is Codex → official Google
Antigravity ACP → maintained Claude ACP.

## Official Antigravity 1.1.1

The [official registry entry](https://github.com/agentclientprotocol/registry/blob/main/antigravity-acp/agent.json)
identifies Google LLC's proprietary distribution, Linux command
`agy_acp_server.par`, and the
[versioned archive](https://dl.google.com/agy-extensions/releases/linux/agy-acp-server-agy_acp_server_1.1.1-linux-x86_64.zip).
Zed editor ships identical verified binaries in `~/.local/share/zed/external_agents/registry/antigravity-acp/v_1.1.1_c5752c93158aa0bc_eef079d17742fe39/`.

Observed binary digests:

| File | SHA-256 |
| --- | --- |
| Archive, 681,969,407 bytes | `38f62d01b32deb0907b3d39a71ec301fd36369f6ffd1cf262d4af385177f79df` |
| `agy_acp_server.par` | `267affa691085fe5d78895e34dffe723d6528713e01bd37ed40feb7b43d1f4c7` |
| `localharness_external` | `d98770b161eb3cc37ae4fa17f088285f9b6ac75c7cb53e0d90ed5077def9d69a` |

### Architecture & Runtime Discovery

1. **Protocol Handshake**:
   - Responds to standard ACP 1.0 `initialize` over stdin/stdout.
   - `agentInfo`: `{"name": "antigravity-acp", "title": "Google Antigravity", "version": "agy_acp_server_1.1.1"}`.
2. **Tool Routing & Filesystem Mediation**:
   - When the client advertises `fs.readTextFile: true` and `fs.writeTextFile: true`, `agy_acp_server` configures client session tools (`client_view_file`, `client_create_file`, `client_edit_file`).
   - These client tools route file reading and file writing directly to Orbit's `Broker` via standard ACP reverse RPCs (`fs/read_text_file`, `fs/write_text_file`).
3. **Session Modes & Unattended Automation**:
   - `session/new` creates an ACP session and advertises available modes: `default`, `auto_edit`, `yolo`.
   - In `yolo` mode, tool calls auto-proceed without interactive permission prompts.
   - Orbit's `Adapter::Antigravity` automatically initializes the session mode to `yolo` via `session/set_mode`.
4. **Isolated Authentication & Credential Leasing**:
   - Governed by `GEMINI_HOME` (set to `/orbit/home/.gemini`).
   - Token store: `/orbit/home/.gemini/antigravity-acp/acp_token.json`.
   - Settings: `/orbit/home/.gemini/antigravity-acp/settings.json`.
   - Orbit's `AuthLease` copies the host's private token store into the container's isolated home directory before startup, prevents concurrent use via file locking, and refreshes any rotated tokens on shutdown.
5. **Container Base Requirement**:
   - Google's embedded C++ initialization (`RealInitGoogle`) expects group `nobody:x:65534:` in `/etc/group`. The packaging script injects this entry into Debian bookworm-slim.

### Automated Packaging Pipeline

Build and pin the container image using:

```bash
bash scripts/prepare-antigravity-fixture.sh [path/to/binaries]
```

This verifies the exact binary SHA-256 hashes, packages them into an immutable image (`localhost/orbit-antigravity:1.1.1`), and outputs the container image ID.

### Multi-Version Registration Workflow

When Google releases newer versions of `antigravity-acp`:
1. Obtain the new version's binaries and record their SHA-256 hashes.
2. Run `scripts/prepare-antigravity-fixture.sh` to produce a pinned container image `localhost/orbit-antigravity:<version>`.
3. Compute the launch digest:
   ```bash
   orbit acp-launch-digest --config /path/to/launch.json
   ```
4. Register the new version alongside previous versions in `workers.json` under distinct `binding_name` identifiers (e.g. `antigravity-acp-v1`, `antigravity-acp-v2`).
5. Workflows can target any registered version without breaking existing runs.

### Example Configurations

- Worker definition: [examples/antigravity-worker.json](../../examples/antigravity-worker.json)
- Workflow definition: [examples/antigravity-coding.yaml](../../examples/antigravity-coding.yaml)

---

## Maintained Claude ACP 0.76.0

The [registry](https://github.com/agentclientprotocol/registry/blob/main/claude-acp/agent.json)
selects `@agentclientprotocol/claude-agent-acp@0.76.0`. Reviewed source tag
`v0.76.0`, commit `c2e4815029ef3962787ecaefe208b0b6f8b81302`, declares
`@anthropic-ai/claude-agent-sdk` 0.3.257 and ACP TypeScript SDK 1.4.0 in its
[manifest](https://github.com/agentclientprotocol/claude-agent-acp/blob/c2e4815029ef3962787ecaefe208b0b6f8b81302/package.json).
The source was read from a temporary clone; dependencies were not installed and
no Claude process, login or model request was started.

In [acp-agent.ts](https://github.com/agentclientprotocol/claude-agent-acp/blob/c2e4815029ef3962787ecaefe208b0b6f8b81302/src/acp-agent.ts),
session creation defaults to the native `claude_code` tool preset and user/project/
local settings. Explicit programmatic options can replace the tools list or use
an empty list; MCP options are merged. Permission handling still authorizes SDK
effects. Read/write client forwarding helpers are present, but this review did
not find a replacement execution path for native Read/Write/Bash through those
helpers and standard ACP terminal creation.
