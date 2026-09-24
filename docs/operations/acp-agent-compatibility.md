# Antigravity and Claude ACP compatibility

Source review: 2026-09-13; status updated 2026-09-25. The requested rollout order is Codex → official Google
Antigravity ACP → maintained Claude ACP.

## Official Antigravity 1.1.1

Post-Q6 controlled preflight: exact model and client file access work, but this
pinned distribution does not wire the client's terminal capability into a model
tool. Its native command tools do not use Orbit's terminal broker. See the
[source review and live evidence](post-q6-hardening.md#live-antigravity-terminal-blocker).
Do not treat an advertised Orbit terminal capability as proof of agent-side
terminal mediation or readiness for another coding qualification.

The [cross-provider remediation](cross-provider-coding.md) supplies a separately
pinned, exact-version build overlay exposing a client-terminal tool and removing
native command execution/local file fallback. It is an Orbit-maintained image
variant, not a claim about the unmodified Google release. Its readiness must be
established by the live model preflight independently of Codex.

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

The previously qualified runtime is historical evidence only. Its image digest
(`sha256:47aeb11ebcccb9192e41bebe97f605021152f8e9f2caf90e3ec6d48dcedc9b97`)
and terminal-overlay parent (`sha256:cdb11fed1c8570f1fdde0060161ab535ba26bc950ebca1307bf7b1cd5875e6f5`)
are not locally recoverable. Do not attempt to recreate either digest or transfer
its qualification to a replacement.

`scripts/prepare-antigravity-fixture.sh` is retained as a legacy fixture builder,
not a reproducible qualification recipe: it uses mutable `debian:bookworm-slim`
and copies the host CA bundle. Do not use it to create the replacement runtime.

The reproducible recipe is defined by
[`deploy/antigravity/Containerfile.reproducible`](../../deploy/antigravity/Containerfile.reproducible)
and [`scripts/build-antigravity-runtime.sh`](../../scripts/build-antigravity-runtime.sh).
It produces a new, initially unqualified runtime revision:

```text
agy_acp_server_1.1.1-orbit-terminal-v2
```

Pinned inputs:

| Input | Identity |
| --- | --- |
| Runtime base | `gcr.io/distroless/base-nossl-debian13@sha256:792f51c506fc67f7eaa38093f6d4937a053cebb79ec0e7c3b7746f6bbba85606` |
| Original ACP `.par` | SHA-256 `267affa691085fe5d78895e34dffe723d6528713e01bd37ed40feb7b43d1f4c7` |
| `localharness_external` | SHA-256 `d98770b161eb3cc37ae4fa17f088285f9b6ac75c7cb53e0d90ed5077def9d69a` |
| Tracked terminal patcher | SHA-256 `27ce5c2ed5f38f4dc6bd99b0027bf5f54c123938deb46bc53bcd87946f4ff502` |
| Tracked terminal client source | SHA-256 `c9a93b16ffca08e313026eee9fbcab1e793368b9b668c6a8351d96822ff33024` |
| Deterministic patched ACP `.par` | SHA-256 `98890a0a1afc3ebe91f6018c15bef26b429147e4b61c408d08b2374465fc10c7` |

The base digest is a published immutable pin used by
[Grafana Mimir's Linux race-image build](https://github.com/grafana/mimir/blob/main/Makefile#L2210-L2224).
Distroless documents that `base-nossl` supplies glibc, CA certificates, and
`/tmp`; its official image catalog lists Debian 13 for amd64. The ACP package's
150 embedded ELF shared objects require glibc plus `libgcc_s.so.1` and
`libunwind.so`; the latter two are present in its embedded solib tree at the
RPATH targets. The external harness is a static x86-64 ELF. Thus the recipe
does not depend on host CA files, a Python installation inside the runtime, a
shell, or an apt/network build step. The minimal `/etc/group` overlay preserves
the root group and the documented `nobody:x:65534:` runtime requirement.

The patcher was run twice into independent temporary outputs. Both outputs were
byte-identical and matched the pinned generated digest above and the existing
generated artifact. The build wrapper additionally pins Python 3.14.7 and
fails closed on any input, patch-output, or base-reference mismatch. Podman is
asked to use the OCI format, epoch timestamps, `--pull=never`, and
`--network=none`. The resulting local image identity is
`localhost/orbit-antigravity-runtime:agy_acp_server_1.1.1-orbit-terminal-v2@sha256:3e7415f6f732ae4168b98a6fb0e14e0fba965020cf5cc1fc5a3b35867b4cf830`.
Credential-free ACP initialize and the catalog-owned OAuth enrollment/fresh
reuse path are qualified with this image. Full model execution qualification is
separate and is not claimed by credential enrollment. Runtime images remain
operator-local and are not committed to the repository.

The offline build path, after the operator has reviewed and staged the exact
source artifact directory, is:

```bash
podman --remote=false --cgroup-manager=cgroupfs pull --arch amd64 \
  gcr.io/distroless/base-nossl-debian13@sha256:792f51c506fc67f7eaa38093f6d4937a053cebb79ec0e7c3b7746f6bbba85606

bash scripts/build-antigravity-runtime.sh \
  /path/to/reviewed/antigravity-acp-artifacts
```

The pull is the only network-dependent step. The wrapper never pulls and the
build itself has no network. A rebuild must produce the reviewed local image
digest above before qualification may reuse this runtime identity; otherwise it
is a new runtime and requires its own qualification.

### Multi-Version Registration Workflow

The v2 wrapper above is pinned specifically to the preserved 1.1.1 artifacts and
patch anchors. Do not use the legacy fixture script for another release. A newer
ACP release needs its own immutable binary pins, patch compatibility review,
runtime recipe and image digest before it can be registered. After that separate
qualification:

1. Compute the launch digest:
   ```bash
   orbit acp-launch-digest --config /path/to/launch.json
   ```
2. Register the newly qualified version alongside previous versions in
   `workers.json` under a distinct `binding_name`.
3. Workflows can target a registered version without changing existing runs.

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
