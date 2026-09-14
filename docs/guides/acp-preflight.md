# ACP installation preflight

Orbit provides this ACP v1 initialization probe separately from the experimental
[ACP workflow runtime](acp-coding.md). Start with this command when qualifying an
installed agent. The
[Codex compatibility record](../operations/acp-codex-compatibility.md) explains
the maintained adapter's tool-routing gap and Orbit's Codex bridge; the
[implementation plan](../development/acp-implementation-plan.md) tracks next work.

`orbit acp-probe` verifies pinned installation files, launches the configured
command in a fresh private directory, and sends only `initialize`. It advertises
no filesystem or terminal capability and denies permission/extension requests.
It never sends `authenticate`, `session/new`, `session/load` or `session/prompt`.
It does not contact Orbit's API or resolve `ORBIT_TOKEN`/`ORBIT_TOKEN_FILE`.

Install and review the chosen agent separately. Use a canonical absolute executable
path and SHA-256 pins for that executable, adapter script and underlying agent
files. For the checked Codex release, pin the Node executable, built ACP adapter,
underlying Codex executable and package lock. A checksum list verifies listed
files, not every dependency in an installation; keep the installation and its
parent directories operator-controlled and immutable during use.

Example private configuration (replace every placeholder; this is not a worker
configuration or Definition):

```json
{
  "command": "/absolute/installed/node",
  "args": ["/absolute/installed/codex-acp/dist/index.js"],
  "files": {
    "/absolute/installed/node": "REPLACE_WITH_SHA256",
    "/absolute/installed/codex-acp/dist/index.js": "REPLACE_WITH_SHA256",
    "/absolute/installed/codex": "REPLACE_WITH_SHA256"
  },
  "expected_agent_name": "@agentclientprotocol/codex-acp",
  "expected_agent_version": "1.11.0",
  "timeout_seconds": 15
}
```

The underlying Codex path must be the binary actually resolved by the pinned ACP
installation, not an unrelated system binary. The command and listed paths must
be canonical regular files without symlink components, with no group/other write
permissions. On Linux, resolve installed paths with `readlink -f` and calculate
pins with `sha256sum`. Keep the resulting config outside Git.

Use an existing disposable parent directory:

```sh
orbit acp-probe --config /absolute/private/codex-probe.json \
  --workspaces /absolute/disposable/acp-probes
```

Each probe creates `acp-probe-<uuid>` with mode 0700. It clears the inherited
environment and sets a fresh HOME/config/cache directory, a fixed `/usr/bin:/bin`
PATH and `NO_BROWSER=1`. No user auth directory, provider key or runtime socket
is supplied. Child stderr is discarded; peer-provided errors, descriptions and
notification content are not emitted as diagnostics. Only bounded identity,
authentication-method IDs and capability booleans enter the report. SDK raw
transport logging must remain disabled.

Limits: 64 KiB CLI config, at most 64 pinned files (512 MiB each), 30 seconds for
verification, 1–30 seconds for initialization, 1 MiB per incoming frame, 4 MiB total
incoming traffic and 128 incoming messages. After initialization or failure, the
probe kills its process group, kills/reaps the direct child and bounds that wait
to three seconds. These are probe controls, not resource/lease supervision for
workflow execution. Hostile code can escape a process group or use host-user
privileges; run only a reviewed installation. Abrupt probe-process death and
full process-tree containment are handled separately by the workflow supervisor,
not this host-only probe.

The JSON/JSONL result has format `orbit-acp-probe/v1`. A successful report includes
`protocol_version: 1`, the exact expected identity, verified config digest and
`direct_child_reaped: true`. It deliberately retains:

```json
{
  "workflow_execution_supported": false,
  "broker_mediation": "not_verified",
  "authentication": "not_tested"
}
```

Exit 0 means that initialization and direct-child cleanup passed. It does not
mean the agent is logged in, that native tools are brokered, that all descendants
were contained, or that a coding workflow is qualified. An identity/version/hash
mismatch, malformed/oversized input, EOF or timeout fails the probe. The generic
probe can inspect later agents, but their actual execution support follows the
requested order: Codex, official Antigravity ACP, then Claude.

Private directories created by the agent are retained under the supplied parent;
inspect and apply local retention policy. The probe does not upload evidence or
delete prior workspaces. For regular offline verification, run
`cargo test --locked --test acp`; no provider account or database is needed.
