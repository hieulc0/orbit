# Credential registry foundation

Orbit now has an additive, operator-scoped credential catalog. PostgreSQL knows
**that** a credential and its representations exist, their lifecycle and logical
identity. A `SecretBackend` knows **the secret**. A trusted worker will eventually
receive only a temporary, leased representation. This foundation does not yet
issue such leases or change existing worker `AuthLease` behavior.

An `AgentRuntime` remains separate: it identifies provider, protocol, image
digest, adapter revision and capabilities. A `Credential` identifies provider,
operator reference, generation, endpoint, auth type and secret backend. Future
candidates may combine runtime, credential, model and reasoning effort; the
scheduler is unchanged.

```text
Provider login/import
    -> Credential
        -> Generation
            -> Representation
                -> SecretBackend
                    -> isolated runtime staging
                        -> fresh-runtime validation
                            -> provider status probe
                                -> AvailabilitySnapshot

Provider
    ├── Credential A (independent generations, scope and evidence)
    └── Credential B (independent generations, scope and evidence)
```

## Domain and scope

`orbit_credentials` has a stable UUID ID, globally unique operator reference,
provider, optional endpoint, auth type, current generation and lifecycle state.
The UUID and provider are identity; `reference` is a mutable operator-facing
name. `orbit credential rename <old> <new>` updates only that name in one
transaction. It does not create a generation, touch a representation or secret,
or rewrite provider/quota evidence. Catalog availability and provider-scope
lookups use the UUID and generation as the stable identity, while retaining
compatibility with older reference-derived evidence keys.
This is a single-control-plane catalog (`scope_key=operator`), not a new tenant
hierarchy. `orbit_credential_generations` retains the backend and primary opaque
locator for each generation. `orbit_credential_representations` binds an opaque
interface and optional capability names to a specific `(credential_id,
generation)` and locator. Two interfaces may legitimately share one locator;
the service checks that both refer to the same current generation. Provider
and interface are extensible strings, not a hard-coded provider enum.

`pending` means metadata exists but provider enrollment or secret publication
and validation is incomplete.
`enrolled` means a secret is durable, not that the provider accepted it or is
available. `invalid`, `disabled` and `revoked` are lifecycle terms. Availability
states such as READY and QUOTA_EXHAUSTED, runtime health, provider-scope
confirmation, and representation validation are separate axes. Metadata CRUD
does not promote availability or import historical evidence; explicit
enrollment and status paths update their own state machines. Legacy
provider-scope identities retain `(provider, reference, generation)` and keep
their original scope/fingerprint keys. Catalog-owned identities use credential
UUID plus generation as their stable key; the reference remains descriptive and
may change. A catalog entry cannot inherit availability or provider-scope
evidence from a pre-catalog/manual credential with a coincident logical tuple.
Catalog-backed status probes persist new snapshots for the exact credential
UUID/generation. This is an identity boundary, not a scheduling change.

Rotation advances the current generation by exactly one, creates a new pending
generation, and retires the old one. No old locator, validation, binding or
availability is inherited. Old generation secrets remain retained and inactive;
deletion/garbage collection requires a future explicit operator policy. Logical
removal is a durable revoked tombstone, also without secret deletion. Use
`orbit credential remove <reference>` (also accepted as `revoke`) to revoke.
This marks the credential and generations revoked; it preserves representation
rows, validation evidence, provider-scope history, availability history and
SecretBackend bytes. Revoked credentials remain inspectable but cannot accept a
representation, rotate, enroll, or pass the catalog-backed status/execution
eligibility checks. Their current reference remains reserved; there is no
implicit restore, hard-delete, secret purge, or alias behavior.

Lifecycle commands connect directly to the configured durable catalog through
`ORBIT_DATABASE_URL_FILE`; they do not require the API server. `list` and
`inspect` continue to use the operator read API. Renames release the old name
because aliases are not modeled; historical evidence remains attached through
the unchanged credential UUID and generation.

## Local private backend

`LocalPrivateSecretBackend` alone maps typed logical locators such as
`credential://<credential-uuid>/generation/<n>/<secret-uuid>` to files beneath
the Orbit-private root. PostgreSQL never receives a physical secret path or
secret payload. The backend requires a canonical operator-owned HOME with no
group/world write access; `.orbit`, `private`, `credentials` and child
directories must be owner-only (`0700`). Secret files must be regular,
single-link, owner-only (`0600`) and at most 1 MiB. Linux `openat2` confines
lookups beneath directory FDs and rejects symlinks and mount crossing. UUID
components and canonical locator parsing reject `..`, absolute paths and
generation aliases. Secret publication writes a random private staging file,
fsyncs it, renames without replacement for create (or atomically over an
existing checked file for replace), then fsyncs the containing directory.
Secret bytes are zeroized on drop and have redacted `Debug`; error messages
contain no secret payload. The operator root is resolved from the effective
user's account record rather than the caller's mutable `HOME`; installations
with an intentional alternate root may set the private `ORBIT_HOME` override.
The database URL file and LocalPrivateSecretBackend use the same root, so a
different shell/service `HOME` cannot silently select another credential store.
Other backends may implement the same trait later.

## Crash consistency and authority

The first provider enrollment adapter is Antigravity ACP personal OAuth. Its
safe sequence is:

1. Operator creates pending metadata and prepares a pending representation with
   an opaque locator in PostgreSQL.
2. After ACP authentication, it captures only the pinned runtime's
   `acp_token.json` and `settings.json` into one versioned opaque secret bundle.
   The backend atomically publishes that bundle. A failure leaves the catalog
   pending and unusable.
3. A completely fresh ACP process stages only those two stored files and calls
   `authenticate(oauth-personal)` without a session or model prompt. A login
   URL during reuse is a failure. Refreshed credential state is republished to
   the same pending locator before finalization.
4. The service checks the durable secret outside a database lock, then locks
   and rechecks the current credential generation, backend and representation.
   It marks the representation stored and generation/credential enrolled in one
   database transaction. For this adapter, it also records
   `last_validated_at` in that same transaction; generic byte-only
   representations remain `stored` without claiming provider validation.

A crash between steps 1 and 2 leaves an inert pending locator. A crash or DB
failure after step 2 leaves a discoverable pending file; operator recovery can
retry finalization only if the same generation remains current, or later GC it.
A concurrent rotation prevents stale finalization. This is not a distributed
transaction. The generic service exposes these operations to trusted
control-plane code only; there are no credential mutation API endpoints or
agent/workflow parameters that select physical secret paths.
The reconciliation path is deliberately manual for now: enumerate pending
representations and their recorded opaque locators in the operator catalog,
check backend existence, compare the credential's *current* generation and
representation state, then either finish a separately validated current
enrollment or classify the older, inactive secret as an orphan candidate.
Only an operator-approved cleanup after a reviewed retention period may delete
such a candidate through `SecretBackend::delete`; no enrollment failure
automatically removes it. Never garbage-collect a locator referenced by a
stored/current representation, and keep the catalog history for audit. There
is no automatic GC or retry command in this checkpoint.
An error from atomic replacement after rename but before directory fsync has
an uncertain publication outcome; a future refresh adapter must re-read and
validate the stored representation before deciding whether to retry.

## Antigravity `agy-cli` representation

The `agy-cli` interface is a second representation of an existing Antigravity
credential generation; it does not create another logical credential or alter
the ACP representation. Its qualified portable auth artifact is the single
relative file `.gemini/antigravity-cli/antigravity-oauth-token`. Orbit stores
only that opaque byte stream through `LocalPrivateSecretBackend`; it does not
store `settings.json`, installation IDs, logs, caches, conversations, updater
state, or generated runtime files. Fresh-process validation stages that one
file into a fresh owner-only HOME, hides the host HOME and `/run`, and provides
no D-Bus or Secret Service. Executable-version checks are networkless. Because
agy 1.2.9 has no auth-status command and its full-screen startup requires a real
terminal emulator, reuse validation runs the non-inference `agy models`
metadata operation with normal outbound network exposed only to that agy
process. A successful exit with non-empty output and no login/authentication or
network error is required. Orbit sends no `/usage` command or model prompt. The
current adapter does not enforce a provider-host allowlist, so validation
network destinations are not claimed to be limited to auth refresh. It never
opens a browser or continues a new login, and any login prompt/URL fails
validation.

The catalog records only a logical artifact label, version, SHA-256 and
provenance as bounded non-secret runtime metadata; physical executable paths
remain local and never enter PostgreSQL or normal inspection output.
Qualification used an Orbit-private pinned copy of agy 1.2.9 with SHA-256
`1dbb10f8295cc1ad2e558bd006c7808fe53b6c7f678a887eb557b576bb591711`; its
provenance remains operator-supplied and is not officially artifact-verified.
The mutable host executable is only an import/discovery source, not runtime
identity. A dedicated immutable `orbit-antigravity-cli` image remains future
work. The production command `orbit credential add-representation <reference>
--interface agy-cli` launches the pinned agy 1.2.9 artifact in a fresh private
HOME, with the host HOME hidden, D-Bus/Secret Service unavailable,
`AGY_CLI_DISABLE_AUTO_UPDATE=true`, and only the provider login interaction.
It captures only `.gemini/antigravity-cli/antigravity-oauth-token`, publishes
it through LocalPrivateSecretBackend, then stages only that backend file into a
second fresh HOME and runs `agy models` before marking the representation valid.
A failed or cancelled login leaves an inert pending representation; ACP and
the logical credential remain unchanged. A hidden qualification-only import
accepts only the retained approved source artifact.

The agy representation records a bounded last-completed enrollment stage
(`prepared`, `login_started`, `login_completed`, `token_captured`,
`secret_persisted`, `validation_started`, `validation_succeeded`, `validated`,
or `legacy_unknown`). These are progress diagnostics, not auth claims.
`login_completed` is recorded only after the isolated process exits and the
approved token file passes capture validation; cancellation or an exit without
that file leaves the stage resumable.
Re-running `add-representation` on a pending/unvalidated row first checks its
existing opaque SecretBackend artifact: if present, Orbit skips login and
validates it in a new HOME; if absent, Orbit starts a new isolated login using
the same credential UUID, generation, representation row and locator. No
second representation or generation is created. A valid stored representation
is idempotent. Normal `credential status` reports the last completed stage and
a bounded reason; `--diagnostics` is required for structural HOME inventory.

An additive migration converts earlier bounded `binary` provenance to a
logical artifact label only for the reviewed pinned Codex and agy version/digest
pairs; those old executable paths are removed from catalog metadata. New writes
use the path-free form. The database constraint remains backward-compatible
with other bounded historical rows, while unknown legacy runtime identities
fail closed in the domain decoder.

Representation publication state and validation remain separate. A backend
write first creates a pending row; fresh-process reuse then sets
`last_validated_at` and leaves the durable row state as `stored` (shown as
validation `valid` in inspection). Failure leaves the agy row pending/inert and
preserves any identifiable secret for explicit orphan reconciliation. The
logical credential remains enrolled through its already-valid ACP
representation. The ACP row is not rewritten, and no availability or
provider-scope state is changed.

The qualified `/usage` path uses the pinned agy artifact and only the
catalog-owned `agy-cli` representation. It parses provider quota groups from
the parent structure `command.data.groups[*]`, with `name` and member labels
reviewed from `description`. Group identity is Orbit-derived and
order-independent:

| Provider name | Provider member labels | Orbit group identity | Windows |
| --- | --- | --- | --- |
| `Gemini Models` | `Gemini Flash`, `Gemini Pro` | `qg1:45333bd9a21030d84869e94f446419b131e79ecc212c080184412c02ebaf37ec` | `weekly`, `5h` |
| `Claude and GPT models` | `Claude Opus`, `Claude Sonnet`, `GPT-OSS` | `qg1:cdb1cd3b7eafd9e7911e0bf61e645f93d91945aa403a54c2c836eac64c8a9912` | `5h`, `weekly` |

Existing `qb1` quota identities are preserved and attached to their containing
provider group. Raw response JSON and raw opaque bucket IDs are not persisted.
The durable Antigravity snapshot is credential-scoped and UNKNOWN: the
observation provides quota metadata but no justified readiness threshold or
exact runtime-model scope. Provider reset timestamps are retained separately
from Orbit's snapshot expiry/freshness policy. This status observation does not
establish account identity; ACP↔agy remains UNVERIFIED.

ACP and agy were intentionally enrolled by the operator using the intended
same Google account, but no stable provider account ID was available for a
machine comparison. Orbit records this pairwise binding as `unverified` with
basis `operator-intent`; it must not be upgraded from an email match or merely
because both representations share a catalog credential. Generation rotation
creates new representations and no identity binding is inherited.

Future provider adapters should discover auth methods, begin/continue
interactive or noninteractive enrollment, validate representations and perform
provider logout. They must not assume OAuth: browser flow, pasted PAT/API key,
SSH key or external secret reference may all fit. The intended operator UX is
`orbit credential add antigravity`, `add codex`, and `add github`; ACP personal
OAuth and Codex account enrollment exist, and agy login is available through
`add-representation` for an existing Antigravity credential. GitHub enrollment
remains future work. The GitHub PAT case can use
`provider=github`, an endpoint
such as `https://github.com`, `auth_type=pat`, and an `api` representation; the
PAT remains in the backend, never PostgreSQL.

## Codex account enrollment

`orbit credential add codex --name <reference>` uses the pinned Codex 0.156.0
[App Server](https://developers.openai.com/codex/app-server)'s provider-native
`account/login/start` method with
`type=chatgptDeviceCode`. Orbit displays the returned HTTPS verification URL
and one-time code only to the operator terminal, then waits for the matching
`account/login/completed` notification. This is the normal ChatGPT/Codex
account flow described by [Codex authentication](https://developers.openai.com/codex/auth);
API-key and unstable raw-token methods are not selected. The
device flow needs outbound provider network but no localhost callback. The
runtime also exposes `account/login/cancel`, `account/logout`, and
`account/read`; logout and refresh persistence are intentionally outside this
checkpoint.

Enrollment runs in the immutable local image
`localhost/orbit-codex@sha256:5e2441ec351e6dc1ce2100111d0e56a08199b4c9d419150fbd236786a1895895`.
That image was built from the checksum-pinned official 0.156.0 package; its
Codex executable SHA-256 is
`78a11f06e0a2dda42d13fba1d50dc62e8cbdb2d5f69789722f4d4d99b5cdbe30`.
The enrollment container has no repository mount, broker, task prompt, previous
agent output, desktop credential store or host `~/.codex`. It forces
`cli_auth_credentials_store="file"` inside a fresh private HOME.

The sole capture allowlist entry is `.codex/auth.json`. Runtime-created SQLite
state, installation identity, logs, sessions, cache, shell snapshots and
built-in assets are excluded. Orbit publishes the file bytes through
`LocalPrivateSecretBackend`; PostgreSQL receives only pending/validated
representation metadata and an opaque locator. A second fresh container stages
only that backend blob and calls `account/read` with `refreshToken=false`.
No `thread/start`, `turn/start`, model prompt, status request or broker is
available on this validation path. Only a non-null ChatGPT account finalizes
the representation and credential as validated/enrolled. A failed or cancelled
login leaves the generation pending. The live `codex-main` browser/device
qualification completed on 2026-09-25 with generation 1 enrolled and the
fresh-runtime reuse check passing.

The existing Codex status adapter has a catalog-backed entry point that
loads the current validated `codex` representation, checks its catalog UUID and
generation against the requested resource, stages only `auth.json`, and then
uses the already-qualified `account/read` plus `account/rateLimits/read`
protocol. It does not copy runtime auth refreshes back; that awaits an explicit
refresh policy. The production `orbit credential status <reference>` command
dispatches to this adapter for Codex or the pinned agy `/usage` adapter for
Antigravity. `orbit credential status --all` processes each credential
independently, up to 32 entries; each eligible account gets its own backend
lookup, isolated staging, provider request and credential-scoped evidence.
Missing/invalid representations, revoked credentials, backend failures and
staging failures use bounded actionable categories without exposing a physical
path or secret locator. ACP-only Antigravity credentials are reported as
status-unavailable because agy-cli is missing, not as broken credentials.
Status is a provider read, not a model turn. The old
`probe-codex-status`/`capture-agy-usage` commands remain hidden qualification
entry points.

Codex provider-scope observations have an explicit operator workflow:
`orbit credential scope show <reference>` (also available as
`provider-scope inspect`) displays the credential-bound `ps1` fingerprint,
latest observation time, safe identity-value comparison and promotion state
without showing provider account material. Confirmation uses the exact current
fingerprint: `orbit credential scope confirm <reference> --fingerprint <ps1>`.
The transaction rejects a replaced fingerprint and requires the latest fresh
status observation to show exact in-memory equality between
`account/read.workspaceRouting.chatgptAccountId` and
`account/rateLimits/read.accountId`. Raw IDs are never retained. This exact
value comparison is a consistency check, not a provider guarantee about
lifetime identity semantics; the operator still explicitly confirms the
fingerprint. Confirmation does not itself probe the provider; run
`credential status <reference>` afterward.

Before confirmation, Orbit persists reviewed quota buckets/windows,
`ordinaryUsageAllowed`, the scope state, and the exact-value comparison inside
the credential-scoped status snapshot. It marks the evidence OBSERVED but
UNCONFIRMED, sets availability/confidence to UNKNOWN, and does not erase quota
values. Those values are visible for status/audit but are not promoted for
scheduling. After explicit confirmation, a fresh matching status read may
promote the same safe quota evidence according to existing availability
policy. Raw provider JSON, email, raw account IDs, and raw limit IDs are not
persisted.

The first observation for `codex-main` was authenticated and persisted as an
UNKNOWN snapshot with provider scope UNCONFIRMED. The status command reports
safe observed quota values separately from promotion and scheduling
availability. Provider scope is never bound by email or auth-file identity.

The registry is multi-account: references are globally unique logical names,
not provider singletons. Each Codex or Antigravity account receives its own
credential UUID, generation history, representation locator, provider-scope
state and availability snapshots. For example, `codex-personal` and
`codex-work` may coexist without implicit account selection or credential
borrowing.

## Operator inspection and compatibility

`orbit credential list` and `orbit credential inspect <reference>` call
operator-only HTTP read endpoints and show metadata, generations, representation
validation, safe runtime version/digest/provenance, and pairwise identity-binding
state. Normal JSON/text output omits runtime executable paths, secret locators,
secret physical paths and secret values. Existing manually
provisioned `~/.orbit/credentials/...`
remain usable by existing `AuthLease` flows. They are **not** catalog entries,
are not migrated/copied/deleted automatically, and require a later explicit
operator-controlled migration. Catalog credentials are not used for worker
selection or lease issuance; the explicit Codex status command is an operator
qualification path and does not change those execution boundaries.
