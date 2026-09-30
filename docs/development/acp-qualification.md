# ACP adapter qualification

An ACP adapter is accepted for a specific runtime, account and effect boundary.
Shared wire support or successful initialization alone does not qualify native
file, terminal, web, MCP or plugin effects. See
[the architecture](../architecture/acp-agent-integration.md),
[setup](../guides/acp-coding.md) and
[compatibility evidence](../operations/acp-codex-compatibility.md).

## Required evidence

| Boundary | Evidence |
| --- | --- |
| Identity | Fixed source/release, binary and image hashes, launch policy and wire version |
| Authentication | Existing private auth reuse, missing/expired auth, refresh writes, locking and quarantine |
| Files | Correlated Orbit callbacks and observed read/write/create effects, including symlink and race denials |
| Commands | Pre-effect terminal mediation, observed exit/nonzero/cancellation behavior and no native fallback |
| Other effects | Source/config mapping for native tools, web, MCP, delegation, hooks, user and repository config |
| Model | Exact requested/observed selection, or explicit `agent_configured` attribution |
| Accounting | Retained prompt/broker/time charges; absent usage and billing remain null |
| Transport | Bounded frames/queues, callback interleaving, response correlation, EOF and malformed peer behavior |
| Ownership | Reservation replay, lease/generation/cancellation fencing and post-I/O rechecks |
| Cleanup | Agent and terminal process-tree exit under timeout, cancellation and worker death |
| Repository result | Accepted patch/manifest, fresh independent test workspace and durable human review |

Fixtures use disposable PostgreSQL, repositories and pinned local images. The
[testing guide](testing.md) defines provisioning and commands. Keep real and generic
ACP cases distinct; each named adapter needs its own evidence.

## Compatibility and authority

Absent optional contracts must preserve legacy serialized bytes and digests.
Nested/referenced bindings affect plan identity; unreferenced ones do not.
Definitions contain logical references, not executable/auth paths or arbitrary
launch arguments. Operator installation policy authorizes the exact runtime.
Unsupported effects and isolation requirements fail before dispatch.

Storage hashing and artifact verification happen outside coordination locks;
ownership is rechecked before acceptance. Unknown external outcomes cannot become
success or authorize automatic redispatch. Cleanup remains possible after execution
budgets are exhausted, and unconfirmed cleanup retains quarantine and fencing.

## Live acceptance

Select the account, repository-data policy and worker host explicitly. Demonstrate
unattended auth, refresh/expiry, actual model semantics, provider rate limits and
an inspect/edit/test/revise task with exact accepted artifacts. Separate-host
acceptance must not depend on a shared developer checkout. Preserve failed attempts,
resource/backend differences and unverified gates. Export into a fresh destination,
verify manifest and artifact identities, and inspect raw content before sharing.
A passing local suite is not owner acceptance.
