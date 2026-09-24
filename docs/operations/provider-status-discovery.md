# Provider status discovery — Checkpoint A qualification

Checkpoint update: 2026-09-25. This record summarizes the completed Codex and
Antigravity status-probe qualifications. Antigravity's pinned agy 1.2.9
observations include bounded quota normalization and provider-defined group
membership persisted to the durable credential catalog. Provider account
identity remains unverified, and availability remains UNKNOWN. This does not
claim Phase B or Q7 readiness.

| Qualification dimension | Codex | Antigravity |
| --- | --- | --- |
| Provider identity | QUALIFIED for the earlier Codex qualification credential: scope was explicitly enrolled, confirmed, then independently matched by a subsequent status probe (`account_scope_matched=true`). The newly Orbit-owned `codex-main` remains UNCONFIRMED. | UNVERIFIED: no comparable provider-reported identity has been established between ACP and agy. |
| Structured status / quota parser | QUALIFIED: pinned Codex 0.156.0 App Server status path. | QUALIFIED: one authenticated pinned agy 1.2.9 `/usage` response was captured through bounded selective extraction; normalized bucket/window evidence is persisted without raw response values. |
| Provider quota groups | Provider-specific bucket/window grouping remains as reported by Codex. | QUALIFIED: provider `name` and `description` identify two quota groups and their member labels; qg1 identities link the existing qb1 windows. |
| Credential quota | QUALIFIED: native evidence normalized to credential scope; state READY with authoritative-native confidence. | QUALIFIED observation, state UNKNOWN: quota metadata is credential-scoped, but Orbit has no justified readiness threshold or exact model scope. |
| Exact-model readiness | UNKNOWN. Credential-wide READY is not promoted to model-specific READY. | UNKNOWN. |
| ACP execution | Not assessed by this status qualification. | QUALIFIED from retained execution evidence only; it does not qualify status or quota. |

Codex's two provider quota groups were retained as groups, not three unrelated
windows:

```text
provider credential
├── bucket.3547634a…
│   ├── operational interpretation: “Luna reserve weekly”
│   └── primary / 7-day window: 26% used
└── bucket.57de4cf4…
    ├── operational interpretation: “normal Codex allowance”
    ├── primary / 5-hour window: 31% used
    └── secondary / weekly window: 46% used
```

The `bucket.…` identifiers are opaque provider-bucket evidence. “Luna reserve
weekly” and “normal Codex allowance” are operational interpretations only;
they are not persisted as provider facts without a trustworthy provider
contract. The 5-hour and weekly windows are siblings in one bucket. Missing
remaining/credits values stay unknown. The 5-hour and weekly windows are not
separate quota resources. These percentages are provider-reported used values,
not derived remaining values.

The status lifecycle completed without a model thread or turn, Task, Attempt,
AgentExecution, broker, or repository effect. The prior UNKNOWN enrollment
snapshot remained immutable. Provider-side billing, quota cost, and rate-limit
cost of the status operation remain UNKNOWN; no claim that the read is free is
made. Normalized Antigravity quota values are held only in the credential-scoped
snapshot; this record intentionally omits account-specific fractions, reset
times, and quota fingerprints. Missing values remain absent rather than
inferred. Antigravity ACP execution remains supported by retained execution
evidence, and its status observation is now qualified separately from execution.

## Pinned runtimes and interfaces

| Runtime used by Orbit | Verified interface | Account and quota semantics | Remaining uncertainty |
| --- | --- | --- | --- |
| Codex App Server 0.156.0 bridge | The [documented `account/rateLimits/read` JSON-RPC method](https://learn.chatgpt.com/docs/app-server) returned structured quota fields in the authorized qualification. The [exact pinned protocol type](https://github.com/openai/codex/blob/rust-v0.156.0/codex-rs/app-server-protocol/src/protocol/v2/account.rs) defines optional `result.accountId`, the usage-snapshot account scope. | An earlier qualification credential's provider-scope enrollment matched on an independent subsequent probe. The newly enrolled `codex-main` status snapshot is UNKNOWN with provider scope UNCONFIRMED. Evidence is credential-scoped, not exact-model evidence. | Billing/request cost and whether the status request consumes provider quota or rate-limit allowance remain UNKNOWN; no general lifetime-stability guarantee for account IDs is claimed. |
| Google Antigravity ACP 1.1.1, including Orbit's separately pinned terminal overlay; agy CLI 1.2.9 for status only | ACP execution and persisted ACP credential reuse are qualified separately. The operator-supplied agy 1.2.9 binary (`sha256:1dbb10f8295cc1ad2e558bd006c7808fe53b6c7f678a887eb557b576bb591711`) has qualified file-backed auth reuse and SecretBackend-staged reuse. One final `/usage` request from that exact pinned artifact exposed and normalized four quota buckets in `command.data.groups[].buckets[]`. | A credential-scoped AvailabilitySnapshot was durably recorded with state UNKNOWN, four buckets, their provider windows/fractions/reset timestamps, and Orbit's separate 60-second evidence TTL. No model readiness or provider-scope binding is inferred. | agy provenance is not official-artifact-verified. ACP↔agy identity remains UNVERIFIED: the response exposed no reviewed stable account/profile identifier. Status-call billing/cost remains UNKNOWN. |

No other provider is qualified for the ACP coding path. The Responses and
command adapters retain their existing provider/worker semantics; this
checkpoint does not claim a native status method for them.

### Account-ID binding preflight

The pinned `GetAccountRateLimitsResponse.account_id` serializes as
`account/rateLimits/read.result.accountId`. Its protocol comment says it is
the account associated with the usage snapshot *when supplied by the backend*.
The [pinned account processor](https://github.com/openai/codex/blob/rust-v0.156.0/codex-rs/app-server/src/request_processors/account_processor.rs)
compares that value with active auth's `get_account_id()` when gating some
account-bound fields. The [pinned auth implementation](https://github.com/openai/codex/blob/rust-v0.156.0/codex-rs/login/src/auth/manager.rs)
uses the selected ChatGPT account/workspace ID for this purpose. This is not a
ChatGPT user ID or email. It is provider-owned, optional scope metadata; Orbit
must not infer exact-model availability from it.
No provider guarantee of lifetime stability was found; compare the exact ID
on every probe and treat absence/change as UNKNOWN.

The [documented `account/read` response](https://learn.chatgpt.com/docs/app-server)
contains account type, and for ChatGPT, optional email and plan type. Neither
is equivalent to `accountId`. The pinned source has other optional or
experimental account/workspace fields, but no *documented* read-only App Server
operation establishes a second independently comparable `accountId`. Orbit
does not call an undocumented operation or decode auth tokens to obtain it.

For a selected ChatGPT **Enterprise workspace**, an operator can independently
obtain the workspace UUID from ChatGPT Admin Settings, as described in
[OpenAI's workspace-control guide](https://help.openai.com/en/articles/20001323-corporate-network-controls-in-chatgpt-enterprise).
That provider-admin record is the appropriate private source for
`ORBIT_STATUS_EXPECTED_ACCOUNT_ID` when it is the workspace selected by the
qualification credential. The selected credential's actual workspace must be
confirmed independently; a different workspace under the same user is not an
acceptable match. No general documented independent acquisition route for a
personal ChatGPT account ID was established by this preflight. Do not use an
email, plan, credential-directory label, auth file, or the status response
itself as the expected ID. If no independent ID is available, use the separate
enrollment path below; never treat first observation as strong equality.

The expected raw ID exists only in private process configuration and for
in-memory comparison. Orbit persists neither it nor email; the normalized
snapshot retains a bounded digest of the provider result and the logical
credential identity. An exact mismatch, absent ID or malformed ID produces
UNKNOWN, never READY.

### Personal-account provider-scope enrollment

`src/provider_scope.rs` derives a domain-separated `ps1:` SHA-256 fingerprint
from the provider label and bounded provider-reported `accountId`. Migration
`0008_provider_scope_bindings.sql` stores one current binding per provider,
logical credential reference and credential generation, plus append-only
transition events with operator actor, time, and (for observations) the exact
AvailabilitySnapshot ID. PostgreSQL rejects event updates, deletes, and
truncation. No raw account ID, email, auth material or unrestricted provider
payload is persisted.

```text
none --first observe--> UNCONFIRMED --explicit confirm--> CONFIRMED
                            |                                |
                  differing observation            differing observation
                            +---------> MISMATCH <-----------+
                                         |
                               explicit re-enroll
                                         v
                                    UNCONFIRMED
```

`observe` always records an UNKNOWN AvailabilitySnapshot, even if the provider
reported positive usage permission. Confirmation cannot promote that old
snapshot. A later `probe` must match the confirmed fingerprint. After provider
I/O and auth/runtime cleanup, a short PostgreSQL transaction locks and
rechecks the binding and atomically records the snapshot/current pointer.
Mismatch records MISMATCH and UNKNOWN without replacing the approved
fingerprint. Re-enrollment needs a separate explicit action and confirmation.
A new credential generation has no inherited binding. Existing
`ORBIT_STATUS_EXPECTED_ACCOUNT_ID` mode retains exact raw-ID equality, without
enrollment. Credential-scoped READY still does not prove exact model readiness.

The operator-only `orbit-status-gate-a` binary consumes the existing private
`{"runtime":...,"resource":...}` JSON. The selected logical reference must
equal both `runtime.auth.owner` and `resource.credential.reference`. Its
PostgreSQL URL must be loopback and name a dedicated `orbit_status_*` database,
never the operator Orbit DB. `inspect`, `confirm`, and `reenroll` do not access
auth/runtime. `observe` and `probe` each perform exactly one isolated status
read, with no model thread/turn, Task, Attempt, AgentExecution, broker, or
repository workspace. Do not execute them without separate live authorization.

After authorization, the personal-account sequence is:

```sh
docker compose up -d --wait postgres
docker exec orbit-postgres createdb -U orbit orbit_status_gate_a
export ORBIT_TEST_DATABASE_URL='postgres://orbit:orbit-local-test@127.0.0.1:55439/orbit_status_gate_a'
export ORBIT_STATUS_PROBE_CONFIG="$HOME/.orbit/private/codex-status-gate-a.json"  # fill private path
export ORBIT_STATUS_SELECTED_CREDENTIAL='<SELECTED_CREDENTIAL_REFERENCE>'  # fill exact reference
unset ORBIT_STATUS_EXPECTED_ACCOUNT_ID

cargo run --locked --bin orbit-status-gate-a -- observe
cargo run --locked --bin orbit-status-gate-a -- inspect
export ORBIT_STATUS_OPERATOR='<OPERATOR_LOGICAL_ID>'  # fill bounded operator identity
cargo run --locked --bin orbit-status-gate-a -- confirm --fingerprint '<ps1:FINGERPRINT_FROM_INSPECT>'
cargo run --locked --bin orbit-status-gate-a -- probe
```

Review the bounded observation and fingerprint before confirming. The final
`probe` is a *second*, separately authorized status observation, not a model
run; no earlier positive snapshot is promoted. For managed accounts with an
independently authoritative ID, set `ORBIT_STATUS_EXPECTED_ACCOUNT_ID`
privately and run only `probe`. Review/export bounded evidence before cleanup:

```sh
docker exec orbit-postgres dropdb -U orbit orbit_status_gate_a
# Stop Compose postgres only if this procedure started it.
```

## Implemented offline normalization

`src/provider_status.rs` accepts only a bounded `account/rateLimits/read`
*result* for the exact Codex bridge revision. It never invokes Codex itself.
The expected account ID is supplied by trusted operator context and is not
persisted. Missing or mismatched identity, malformed JSON, oversized output,
ambiguous bucket identity, unexpected response version markers, invalid percentages or timestamps all produce an
UNKNOWN snapshot without quota windows. The source payload is reduced to a
SHA-256 digest; arbitrary provider text and account IDs are not copied into
the snapshot.

On a verified response, the normalizer preserves each reported window's
used percentage, duration and reset time. It leaves remaining percentage and
`exhausted` null unless reported. `ordinaryUsageAllowed=true` becomes a
credential-scoped READY observation; the availability evaluator does not
treat a broad READY as exact model readiness. `false` becomes LIMITED rather
than a model-specific quota-exhausted claim. A missing value remains UNKNOWN.
No reset is calculated from a window duration.

A separate pure adapter converts confirmed normalized `QuotaExhausted` or
`RateLimited` execution outcomes into exact-resource snapshots without a
fabricated reset. It is not wired to automatic worker publication: there is
no selected `ExecutionResourceIdentity` or resource lease on existing claims.
Both adapters use the existing PostgreSQL `AvailabilityStore` for persistence
when an authorized caller records their returned snapshot. A new additive
`quota_buckets` snapshot field explicitly retains the domain-separated
`qb1:` provider-bucket fingerprint and its provider window IDs/measurements;
the existing flat `quota_windows` field remains a compatibility projection.
Empty `quota_buckets` is omitted during serialization, preserving legacy
snapshot decoding and IDs. Snapshots are already stored as JSONB, so this model
addition requires no migration; enrollment uses migration 0008 without
changing legacy Run or availability schema.

## Qualification status

On 2026-09-25 Orbit completed its first native Codex enrollment as the new
catalog credential `codex-main` (generation 1). The pinned App Server's device
flow wrote only `.codex/auth.json` to `LocalPrivateSecretBackend`; a fresh
runtime reused it through `account/read` without a model turn. A subsequent
single catalog-backed status observation used the existing
`account/read`/`account/rateLimits/read` protocol and persisted an UNKNOWN
AvailabilitySnapshot. Its provider-scope binding is UNCONFIRMED for this new
logical credential, so the state machine intentionally did not trust/promote
the quota result. This is separate from the earlier, independently confirmed
Codex qualification identity; no binding is inherited between credential
references or generations.

| Gate | Evidence |
| --- | --- |
| Parser and unknown semantics | `cargo test --locked --test provider_status --test availability` exercises bounded structured results, multiple windows, null fields, mismatch, ambiguity and stale evaluation. |
| PostgreSQL persistence | The targeted ignored `availability_history_is_idempotent_and_current_is_monotonic` case checks scoped persistence/current-pointer/freshness against disposable PostgreSQL. Live qualification also read back the persisted snapshot and verified current-pointer linkage. |
| Secret and payload retention | Unit tests check that unrelated provider text, raw account ID, raw bucket IDs, and operational/friendly labels do not enter the serialized snapshot. |
| Enrollment and concurrency | Focused unit tests and ignored disposable-PostgreSQL `provider_scope::` tests check UNCONFIRMED/CONFIRMED/MISMATCH, generation isolation, duplicate/concurrent confirmation, restart retention, and UNKNOWN before confirmation or after mismatch. |
| Probe isolation, auth binding and inference | The earlier ID-verified Codex qualification trace confirmed its selected credential scope. The new catalog-backed `codex-main` trace authenticated but remains UNCONFIRMED under current binding policy. Both paths ran status only; no model thread/turn or repository/broker workflow effect occurred. Billing/quota/rate-limit cost remains UNKNOWN. |

Antigravity matrix: ACP credential enrollment/reuse and agy file-backed
credential reuse are QUALIFIED for the recorded operator-supplied artifacts.
The pinned agy 1.2.9 `/usage` status command, selective quota extraction,
credential-scoped normalization, and durable AvailabilitySnapshot persistence
are QUALIFIED by one authorized request plus offline parser/PostgreSQL tests.
The separately authorized final group-metadata request parsed provider group
membership from the exact `description` field and persisted an additive
credential-scoped snapshot. Availability remains UNKNOWN, provider account
identity remains UNVERIFIED, and no additional `/usage` request was made.

### Historical Antigravity identity-binding preflight (offline, 2026-09-24)

This preflight predates the subsequent authorized agy file-auth and `/usage`
observations below. Its conclusions about provider identity remain applicable;
its earlier assumption that a usable headless file credential/status path had
not been demonstrated is superseded by those later observations.

Decision: `UPSTREAM_IDENTITY_BLOCKED`. The official CLI auth documentation
describes OS keyring-backed token profiles (Linux Secret Service/D-Bus,
Apple Keychain, or Windows Credential Manager), with Google Sign-In fallback.
It does not define a supported credential import/export format, Linux keyring
service/account namespace, named profile selector, or non-inference account-ID
read. The credential-free local `agy 1.2.9 --help` inspection likewise showed
no account/profile selection command. Official troubleshooting notes that a
locked or headless keyring prevents credential reads; the official changelog
describes keyring bypass when no D-Bus session exists, but does not define a
safe Orbit-provisionable credential source for that case. The documented
`AGY_CLI_DISABLE_AUTO_UPDATE=true` control can disable the updater; this does
not resolve identity binding.

Orbit's ACP configuration stages operator-provisioned `acp_token.json` and
`settings.json` into `.gemini/antigravity-acp/`. The official ACP registry
identifies a proprietary executable distribution but publishes no auth
contract. Orbit's ACP lease copies and refreshes those files; no supported
conversion or provenance link from the CLI's OS-keyring profile to those ACP
files is recorded. The lease stages files and copies staged changes back on
finish; it does not create or convert the provider login. A common HOME, email,
or Orbit credential label cannot establish equivalence. The Gemini API-key
mode is a different auth path and does not create a signed-in Antigravity
account session.

The official status-line schema includes `email` and quota keys described as
model/bucket IDs, but no opaque provider account/workspace identifier. Email
is not accepted as identity evidence, and quota bucket identifiers cannot be
promoted to account identity. This offline preflight made no authentication or
provider request.

Reopen only when upstream provides at least one supported binding path: a
documented credential artifact accepted by both ACP and CLI; the same stable
opaque provider identity from both interfaces; a shared named account/profile
selector bound to the Orbit credential generation; or an ACP-native status
operation using the ACP credential. The present statusline email and CLI
keyring default are not substitutes.

### Antigravity usage capture safety preflight (offline, 2026-09-24)

One separately authorized `agy --print "/usage" --output-format json` command
from a fresh HOME using only the Orbit-managed `agy-cli` representation exited
successfully and emitted valid JSON. The initial capture guard used substring
matching and rejected a key containing a token-like substring; it discarded
the response before recording which key caused rejection. No response document,
availability snapshot, or provider-scope binding was written. The command was
not repeated.

The offline capture preflight uses exact normalized key classification:
explicit sensitive names are redacted, explicitly allowlisted status/quota
names are Safe, and all other names remain Unknown. Unknown and Sensitive
fields block any future raw-document retention, but their bounded schema
diagnostics retain only path, field name, JSON type, rejection reason, and
limited structural counts/lengths. Safe sibling structure remains visible.
The input, nesting, node, key, array, string, diagnostic, and serialized-schema
budgets are bounded in `src/agy_usage_schema.rs`. Values are scrubbed after
schema generation; raw status JSON is not retained. At that checkpoint,
provider values were not available to the normalizer, so availability remained
UNKNOWN and ACP↔agy identity remained UNVERIFIED.

### Antigravity selective quota extraction and live status (2026-09-24)

A separately authorized final `/usage` response used the exact pinned agy 1.2.9
artifact and the registered agy-cli representation in a fresh isolated HOME.
It contained two groups with two bucket entries each. Each bucket had one
window; bucket fields included `id`, `name`, `remaining_fraction`, `reset_time`,
`window`, and an additional `description` field. The response also had the
top-level fields `command`, `conversation_id`, `duration_seconds`, `num_turns`,
`response`, `status`, and `usage`. Values outside the reviewed quota paths were
discarded; unknown field values, including the additional description, were
redacted. No stable account/profile identity was exposed.

`antigravity_usage_capture` now returns bounded schema diagnostics plus only
context-approved quota observations. Value extraction is limited to the exact
bucket path: finite `remaining_fraction` numbers are preserved without
clamping or an assumed range; short, control-free `window` strings are retained
as provider window IDs; and `reset_time` is retained only when it parses as a
valid compact UTC timestamp (`YYYY-MM-DDTHH:MM:SSZ`). Other timestamp forms
produce redacted diagnostics and no reset evidence. A missing/null value stays
absent. Raw bucket IDs are used only in memory to derive domain-separated
`qb1:` fingerprints, never as account identity or persisted values. Repeated
windows under one bucket remain siblings. A bucket-ID collision across groups
fails closed because no stable group identifier is approved for namespacing.

Routine CLI output uses a compact deterministic schema summary and the
normalized bucket/window structure; it does not print the detailed schema
tree or raw provider document. The live response had four distinct bucket
fingerprints, each with one window. The actual values and fingerprints are in
the credential-scoped availability evidence, not this document. No missing or
null values occurred on the reviewed `remaining_fraction`, `reset_time`, and
`window` paths. Unknown/sensitive values, token-usage values, group
descriptions, response/status strings, and conversation identifiers were not
retained. Reviewed group names/descriptions and parsed member labels are now
retained only from `command.data.groups[*].name` and `.description`; unreviewed
group/bucket fields remain value-redacted. Snapshots remain `UNKNOWN`; provider reset time is distinct from
Orbit's `expires_at_ms` (a separate 60-second local freshness interval). No
model/reasoning readiness or provider scope is inferred. ACP↔agy identity
remains UNVERIFIED. The staged token's metadata changed during the command, but
its contents were not inspected or written back; token refresh and conversation
creation remain UNKNOWN. Provider billing/status-call cost also remains
UNKNOWN. Synthetic offline parser and PostgreSQL tests plus the full Rust suite
passed; the single live request was not repeated.

The final separately authorized group-metadata request confirmed the actual
provider JSON fields under `command.data.groups[*]`: `name: string`,
`description: string`, and `buckets: array<object>`. The exact descriptions
supplied the member lists using the reviewed prefix “Models within this group:”;
no provider-issued stable group ID was used. Provider values were:

| Provider display name | Provider member labels | Attached quota windows |
| --- | --- | --- |
| `Gemini Models` | `Gemini Flash`, `Gemini Pro` | `weekly`, `5h` |
| `Claude and GPT models` | `Claude Opus`, `Claude Sonnet`, `GPT-OSS` | `weekly`, `5h` |

The second name's lowercase `models` is the exact API value; the operator UI
capitalizes that word. The `qg1:` identity is Orbit-derived from the provider
name and sorted member labels, not a provider identity. The four existing
`qb1:` bucket fingerprints were preserved and attached by their structural
parent group in the JSON; array order, screenshot position, and bucket order
were not used to infer membership. Raw response and opaque provider bucket IDs
were not persisted. The new immutable credential-scoped snapshot remains
`UNKNOWN`; this enrichment does not establish exact runtime model capability or
readiness. No stable account/profile identity appeared, so provider scope is
NONE and ACP↔agy identity remains UNVERIFIED.

On this final request the staged token file's metadata changed, but token bytes
were not read or compared, so a refresh is UNKNOWN and no changed token was
written back. The response's empty conversation identifier does not prove that
no provider conversation was created, so conversation creation and billing /
status-call cost remain UNKNOWN. No model turn occurred. The one live request
was not repeated.

Sources: [CLI authentication](https://antigravity.google/docs/cli/install/),
[CLI keyring/updater troubleshooting](https://antigravity.google/docs/cli/troubleshooting/),
[CLI status-line schema](https://antigravity.google/docs/cli/statusline/),
[Antigravity ACP registry entry](https://github.com/agentclientprotocol/registry/blob/main/antigravity-acp/agent.json).

The dedicated `codex_status_probe` path now reuses pinned version preflight,
rootless read-only Podman launch, isolated auth staging, the worker-local
credential lock and quarantine marker. It does not create a Task, Attempt,
AgentExecution, ACP session or broker. Its fixed lifecycle is App Server
`initialize`, `initialized`, `account/read` (no refresh), then
`account/rateLimits/read`. It rejects server callback requests and
thread/turn/item notifications, bounds total wire bytes and notification
count, and has a 30-second protocol deadline. No repository directory is
mounted. The auth marker is cleared only after confirmed container removal
and staged-credential cleanup. A live result is not returned on uncertain
cleanup. Probe failures carry fixed typed categories and a bounded structural
receipt, never raw App Server errors or credentials. Auth staging itself is
explicitly recorded; zero filesystem/terminal *broker* callbacks must not be
misread as zero local auth-staging filesystem activity.

The disposable control root is created explicitly with mode `0700` and must be
owned by the effective Orbit UID, canonical, non-symlinked, and inaccessible to
group/other users. A shared temporary parent such as `/tmp` need not be private
itself when it is a trusted or sticky shared directory; an insecure writable
parent is rejected. Do not use `tempfile::tempdir()` with its default mode
(`0777 & !umask`, commonly `0755`) for this root. Control-root errors report a
typed reason and bounded UID/mode metadata without exposing a path.

In particular, `AuthLease::stage_status` creates an **empty `workspace`
directory inside the disposable private control HOME** because the pinned
runtime launch contract expects it. It is not an Orbit Attempt/repository
workspace: no Task, Attempt, AgentExecution, repository checkout or
materialization, coding broker, or repository mount exists on this path. A
focused offline fixture verifies the directory is empty and the control HOME
contains only that directory and staged auth scaffolding. The Gate-A zero-effect
assertion is: Task, Attempt, AgentExecution, Attempt/repository workspace,
repository materialization, broker connection, broker file/terminal callbacks,
and repository effects are all zero. The private control directory is cleaned
up with the harness's disposable environment, while auth marker removal still
requires confirmed container cleanup.

The ignored `one_pinned_codex_status_probe` test remains an expected-ID-mode
qualification harness. It is not the personal-account enrollment path used
for the completed qualification. The operator-only `orbit-status-gate-a`
command performed the observe → human confirm → independent probe sequence.
The earlier control-root validation failure was corrected offline; that failed
attempt occurred before runtime preflight, credential staging, or provider I/O
and produced no enrollment event.

The qualified Codex protocol trace establishes that no model thread or turn
was created and no inference request was part of the status lifecycle. It does
not establish whether the provider treats the status read as billable, charges
quota, or rate-limits it; all such cost properties remain UNKNOWN. Antigravity's
status schema, quota windows, provider quota groups, and credential-scoped
snapshot are now qualified; effective availability remains UNKNOWN and the
ACP↔agy identity remains UNVERIFIED. See the final group-metadata observation
above for the current result.
