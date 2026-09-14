# Antigravity and Claude ACP compatibility

Source review: 2026-09-13; status updated 2026-09-14. The requested rollout order is Codex → official Google
Antigravity ACP → maintained Claude ACP. Codex's real binary has passed an offline
broker workflow; see [that record](acp-codex-compatibility.md). The shared
[ACP runtime](../guides/acp-coding.md) is implemented, but neither later named
adapter is qualified for Orbit repository execution. No account was selected,
no login occurred and neither later agent was launched during this review.

## Official Antigravity 1.1.1

The [official registry entry](https://github.com/agentclientprotocol/registry/blob/main/antigravity-acp/agent.json)
identifies Google LLC's proprietary distribution, Linux command
`agy_acp_server.par`, arguments `--uid=`, and the
[versioned archive](https://dl.google.com/agy-extensions/releases/linux/agy-acp-server-agy_acp_server_1.1.1-linux-x86_64.zip).
This is not a similarly named community wrapper.

The official archive was downloaded into a disposable directory and inspected
without execution. Observed pins:

| File | SHA-256 |
| --- | --- |
| Archive, 681,969,407 bytes | `38f62d01b32deb0907b3d39a71ec301fd36369f6ffd1cf262d4af385177f79df` |
| `agy_acp_server.par` | `267affa691085fe5d78895e34dffe723d6528713e01bd37ed40feb7b43d1f4c7` |
| `localharness_external` | `d98770b161eb3cc37ae4fa17f088285f9b6ac75c7cb53e0d90ed5077def9d69a` |

The ELF/Python archive contains inspectable adapter sources under
`google3/cloud/developer_experience/antigravity_extensions/acp_server/`.
`tools.py` defines client view/create/edit functions that call ACP file methods;
`server.py` conditionally installs those functions from the advertised filesystem
capabilities. That is a concrete filesystem handoff candidate, not runtime proof.

The inspected server's command paths use native `run_command` / `shell` execution,
review/sandbox policy and output translation. No `create_terminal` or
`wait_for_terminal_exit` client dispatch was found in the inspected server.
The file-tool descriptions also retain a native read path outside the client
workspace. Therefore client filesystem support alone does not satisfy Orbit's
broker-only command/effect policy. This finding is limited to the pinned inspected
source; it is not proof that no other integration interface exists.

Next gate: demonstrate an official pre-effect terminal handoff or design a pinned
bridge that suppresses native effects and routes commands to Orbit. Then exercise
the real adapter in a resource-limited, credential-free image before selecting
account/auth storage. The earlier approval-service limit has cleared, but no
Antigravity initialization or execution qualification has been performed. The
native-command handoff remains the implementation gate.

Google's [IDE documentation](https://antigravity.google/docs/ide/extensions)
describes user/enterprise sign-in choices. No account class, project, entitlement
or billing assumption is inferred from that documentation. The selected deployment
must explicitly qualify unattended auth, refresh files and its data/egress policy.

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

[tools.ts](https://github.com/agentclientprotocol/claude-agent-acp/blob/c2e4815029ef3962787ecaefe208b0b6f8b81302/src/tools.ts)
builds ACP terminal display content from native Bash tool-use IDs and output
metadata. Display events must not be confused with Orbit-created terminals.

Next gate: pin an actual execution handoff (for example, a reviewed adapter using
only explicit custom tools with the native preset disabled), demonstrate callback
and no-fallback behavior, then qualify isolated startup, account selection and
refresh. No unchecked `dontAsk`/bypass mode or native repository mount is supplied
as a substitute. A future bridge requires its own version, immutable image policy,
fixture and live acceptance record.

## Shared acceptance checklist

For each adapter, record image/binary/source pins; initialized identity; exact
model or explicit agent-configured attribution; private auth mapping; callback
trace before each effect; denied native fallback; tool/agent cleanup on cancel,
EOF and worker death; retained ledger/transcript; independent patch/test/review;
and account/host approval. Until those gates pass, an `adapter: acp` installation
is operator-configurable transport, not named-agent qualification.
