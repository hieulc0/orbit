# Provider execution

- [Runtime, credential and resource identity](#runtime-credential-and-resource-identity)
- [ACP ownership and lifecycle](#acp-ownership-and-lifecycle)
- [Quota observations and normalization](#quota-observations-and-normalization)

## Runtime, credential and resource identity

### Runtime and resource identity

Keep these dimensions separate:

- Runtime: pinned executable/image, adapter revision and launch policy.
- Credential: logical catalog identity, generation, representation and provider scope.
- Model and reasoning: requested requirements and observed activation, when reported.
- Capability: the runtime's supported operations and execution constraints.
- Role: semantic responsibility and allowed effects.
- Availability: scoped observations with explicit freshness and unknown fields.
- Ownership: current lease, generation, request identity and cancellation state.

Authentication does not establish capability or quota. An advertised model does
not prove activation. Exact-model policies require confirmation before prompting;
`actual_model` remains unknown unless reported. Do not silently substitute providers,
models, reasoning levels or isolation profiles.

Logical resource identities are canonical and secret-free. Credentials and auth
paths are resolved by operator policy at execution time; plans, prompts and safe
inspection use logical references. Provider credentials remain isolated between
runtime executions. Auth-store locking/quarantine and database mutation ownership
protect different resources and neither replaces the other.

### Availability and selection

Availability snapshots record scope, source, observation/expiry timestamps,
confidence and normalized quota buckets, windows and groups. Raw provider bucket
IDs become domain-separated fingerprints. Flat window projections remain a legacy
compatibility surface; consumers use explicit bucket/group membership.

Missing percentages, reset times, token usage and billing are null or unknown.
An exhausted result does not supply the missing numerical values. Provider resets
are separate from Orbit's evidence freshness TTL. Stale positive evidence cannot
remain authoritative READY. Conflicting evidence retains provenance and scope;
optimistic observations do not automatically override stronger negative evidence.
An account-scoped observation must not be presented as model-specific readiness.

The role resolver filters fresh credential-scoped evidence and capabilities before
ranking eligible accounts. Known five-hour headroom below 15% or seven-day headroom
below 5% blocks the default selection; exact thresholds remain eligible. Safe known
weekly quotas rank by earliest reset, then provider preference and stable credential
identity. Unknown windows follow explicit availability policy. Codex uses its default
bucket; reserve quota cannot override that bucket's safety guard. Antigravity uses
the group matching the requested provider/model. See
[resolver qualification](../operations/troubleshooting.md#focused-workflow-qualification).

Selection records explain selected and rejected candidates using bounded evidence.
The LLM does not choose account eligibility or change selection policy. Status probes
are bounded observations, not inference turns or a source of fabricated billing.
A fresh status refresh does not by itself authorize a provider invocation.

## ACP ownership and lifecycle

### Boundaries and ownership

ACP is a worker-to-agent protocol, not a second workflow engine or authorization
service. PostgreSQL owns accepted plans, reservations, receipts, session records
and artifacts. Provider computation is externally uncertain, never implicitly
exactly-once. Each real agent needs independent mediation and account evidence.

```text
Authenticated API → Engine operations → PostgreSQL accepted state
                          |
                    lease-owning worker
                          |
        +-----------------+-------------------+
        |                 |                   |
  private Git        ACP supervisor      ACP client broker
  materializer            |              /              \
        |          pinned agent OCI   confined files   workspace supervisor
        |          private auth HOME       |              |
        |          NO repository mount     |        pinned tool OCI
        +------------------------------ attempt repository
                                             |
                           accepted patch / manifest / session evidence
                                             |
                            fresh independent test → human approval
```

The worker's Orbit token, Git credentials, container runtime and actual repository
path never enter the agent container. Only a fresh control HOME and selected auth
files are mounted. The virtual client workspace `/orbit/home/workspace` exists
as an empty directory in that HOME. The broker maps its absolute paths to the
real attempt workspace; an agent cannot use arbitrary host paths.

Both agent and tool containers use read-only roots, dropped capabilities,
no-new-privileges, rootless UID mapping, bounded CPU/memory/PIDs and temporary
storage. The step reserves their **combined** resources: agent limits must fit
half; the tool container gets half. Tool networking is always disabled. Agent
networking is explicitly `none` or `host`; `host` is not provider-only egress.
Dedicated-host policy must protect other local/network services. This does not
upgrade the established `trusted` isolation class or prove safety for hostile agents.

### Protocol lifecycle and bounds

One attempt creates one fresh session and sends one prompt. There is no
`session/load`, transparent resume, task-time authentication, delegation,
external MCP or dynamic permission approval. A prompt can contain many provider
model requests; it is reserved as one externally uncertain dispatch.

The production wire pump owns bounded reads directly: 1 MiB per frame, 16 MiB
incoming bytes per peer, 16,384 messages, correlated outgoing IDs and bounded
unique callback IDs. There is no unbounded reader queue, SDK transport logger or
raw payload diagnostic. The SDK's stable schema types validate callback/update
structures; the initialize-only probe separately uses its high-level connection.
Version 0.10.2/schema 0.11.2 avoids enabling `serde_json/preserve_order` through
dependency feature unification; an upgrade requires a legacy-digest review.

During a pending prompt, the worker services notifications and reverse requests.
Terminal creation returns promptly; the process continues under an independent
supervisor. Callback handling is serialized, with one terminal and no simultaneous
file operations. Ownership loss/cancellation is watched by the existing outer
worker loop and drops the entire ACP future; it does not wait behind a terminal
callback. A turn timeout sends a bounded cancel notification and closes transport.
Execution/drain grace and cleanup limits remain separate from a model's response.

### Session ownership and interleaved messages

After `session/new` returns an ID, establish broker ownership before model or mode
selection. While awaiting a response, accept legitimate updates and callbacks for
that exact session under normal policy, then continue waiting for the matching
response ID. A notification cannot complete a request. Foreign sessions and genuine
response-ID mismatches remain integrity errors. Consuming the correct response keeps
later requests from seeing stale replies. Exact model activation is verified before
prompting; initialization or a capability manifest alone cannot establish it.

### Codex bridge

The maintained `codex-acp` release is a compatibility reference, not Orbit's
execution backend: it forwards native tool approvals and presents native terminal
events. Zed supports both those display events and client-owned execution.
Displaying a terminal is not proof of an ACP terminal callback.

Orbit's bridge speaks ACP to the worker and the pinned Codex App Server protocol
inside the agent image. It initializes experimental API support, checks existing
auth without starting login, and creates a thread with `environments: []`,
empty workspace/capability roots, exact model/no fallback and only selected
`orbit_read_file`, `orbit_write_file`, `orbit_shell` dynamic tools.
Native environment tools, web, user-input/update-plan tools, selected effect
features, hooks, MCP and delegation are disabled. A clean control directory
prevents developer/project config discovery. The model's requested operation
passes through the Orbit broker before any repository effect.

Foreign thread/turn identities, namespaces, duplicate call IDs, native reverse
requests and native effect items fail closed. Shell maps to create → wait →
output → release, including release after an error. The real binary's loopback
qualification checks provider tool names as well as repository effects; see the
[fixed-source anchors and evidence](../reference/configuration.md#supported-runtime-boundaries-and-pins).

## Quota observations and normalization

Status probes observe a credential representation without starting a model
session or repository workflow. Provider authentication, account binding,
availability, quota and exact-model capability are independent evidence axes.
See the [credential registry](credentials.md#catalog-and-private-secret-storage) and
[workflow selection contract](../requirements/scheduling.md#credential-selection).

### Normalization and identity

Probes bind observations to credential UUID and current generation. Raw account
IDs are compared in memory, then discarded. Authentication alone cannot confirm
provider scope; missing or mismatched identity withholds readiness promotion
without erasing safely normalized quota observations. A broad READY observation
is not exact-model readiness. Rotated generations inherit no identity binding,
validation or availability evidence.

Malformed/oversized JSON, ambiguous bucket identity, unexpected response versions,
invalid percentages or timestamps fail closed. Snapshots retain source hashes and
bounded normalized metadata, not arbitrary provider text, raw account IDs or
opaque bucket IDs. Schema diagnostics mask dynamic identity keys and redact
unreviewed values. Unknown measurements remain absent or explicitly unknown.

Provider reset timestamps differ from Orbit's snapshot expiry/freshness policy.
A window duration cannot establish a reset time. `quota_buckets` retains
fingerprinted provider buckets and sibling windows; `quota_windows` is the legacy
flat projection. Empty optional additions are omitted to preserve legacy bytes
and identities. Bucket/group fingerprints describe quota structure, not provider
account identity.

### Codex

The pinned App Server status path uses `account/read` and
`account/rateLimits/read` without a model thread or turn. The normalizer accepts
only a bounded matching result for the reviewed bridge revision. Percentages
must be finite and within 0–100; invalid values are not clamped. Remaining percent
and fraction are derived from valid `usedPercent`. Unix-second resets convert
exactly to milliseconds. `exhausted` remains absent unless reported.

For confirmed scope, `ordinaryUsageAllowed=true` permits credential-wide READY;
`false` means LIMITED, not exact-model quota exhaustion. Missing permission or
unconfirmed scope remains UNKNOWN. The configured Luna target uses the `default`
bucket; `gpt-reserve` does not determine its admission.

### Antigravity

The `agy-cli` representation uses a bounded `/usage` observation. The portable
file-backed agy 1.2.9 artifact is operator-supplied; official artifact identity
and ACP↔agy provider-account binding remain unverified. ACP credential enrollment
and status-probe reuse do not establish a common provider account.

Only reviewed paths provide quota values. Finite `remaining_fraction` numbers
are retained without clamping or inventing a range. Short control-free `window`
strings identify provider windows; reset evidence requires the compact UTC format
`YYYY-MM-DDTHH:MM:SSZ`. Missing/null values stay absent, and other timestamp forms
produce diagnostics without reset evidence. Bucket IDs derive domain-separated
`qb1:` fingerprints in memory. A bucket collision across groups fails closed.

Reviewed `command.data.groups[*].name` and `.description` provide group metadata.
Member labels are parsed from the provider's reviewed “Models within this group:”
prefix. Orbit derives `qg1:` identity from group name and sorted member labels;
structural JSON parentage attaches buckets. Array order and UI position cannot
establish membership. The Gemini target uses the Gemini group and ignores the
Claude/GPT group. Unreviewed values and raw response text are not persisted.

Snapshots remain credential-scoped UNKNOWN without justified readiness or exact
model scope. Routine output presents a compact schema summary and normalized
quota structure, never raw provider documents, conversation IDs or token data.
A changed staged-file timestamp alone does not establish successful token refresh.

### Execution feedback and recovery

Confirmed `QuotaExhausted` or `RateLimited` execution outcomes can produce exact
resource snapshots without a fabricated reset. The pure adapter is not automatic
worker publication: existing graph claims do not carry a selected resource lease.
Authorized callers persist snapshots through the availability store. Status calls
and callback counters do not measure provider billing. Runtime re-resolution after
an execution quota failure is not automatic.
