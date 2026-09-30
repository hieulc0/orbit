# Credentials and staging

- [Catalog and private secret storage](#catalog-and-private-secret-storage)
- [Authentication staging and quarantine](#authentication-staging-and-quarantine)

## Catalog and private secret storage

Orbit has an operator-scoped credential catalog. PostgreSQL records credential
identity, generations, representations, lifecycle and logical secret locators.
A `SecretBackend` stores the secret bytes and resolves their private physical
paths. Catalog-backed CLI role execution selects an eligible credential,
rechecks its generation and stages the selected representation through the local
backend. The graph worker's configured `AuthLease` remains a separate manual
staging path; the catalog does not issue a general worker credential-lease API.

An `AgentRuntime` identifies provider, protocol, image digest, adapter revision
and capabilities. A `Credential` identifies provider, operator reference,
generation, endpoint, auth type and secret backend. Role resolution combines
runtime, credential, model and reasoning effort under the
[workflow selection contract](../requirements/scheduling.md#credential-selection).
The graph scheduler does not become a provider-selection or secret service.

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

### Domain and scope

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

### Local private backend

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

### Crash consistency and authority

Antigravity ACP personal OAuth enrollment uses this crash-safe sequence:

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

A crash before secret publication leaves an inert pending locator. A crash or
DB failure after publication leaves a discoverable pending file; operator recovery can
retry finalization only if the same generation remains current, or later GC it.
A concurrent rotation prevents stale finalization. This is not a distributed
transaction. The generic service exposes these operations to trusted
control-plane code only; there are no credential mutation API endpoints or
agent/workflow parameters that select physical secret paths.
Reconciliation is manual: enumerate pending
representations and their recorded opaque locators in the operator catalog,
check backend existence, compare the credential's *current* generation and
representation state, then either finish a separately validated current
enrollment or classify the older, inactive secret as an orphan candidate.
Only an operator-approved cleanup after a reviewed retention period may delete
such a candidate through `SecretBackend::delete`; no enrollment failure
automatically removes it. Never garbage-collect a locator referenced by a
stored/current representation, and keep the catalog history for audit. There
is no automatic GC or retry command.
An error from atomic replacement after rename but before directory fsync has
an uncertain publication outcome; a future refresh adapter must re-read and
validate the stored representation before deciding whether to retry.

## Authentication staging and quarantine

### Authentication and failure recovery

One private canonical auth directory is exclusively locked on the worker host.
Only explicitly mapped private regular files are staged into a fresh control HOME.
A durable marker records the exact agent container and attempt before launch.
After confirmed removal, refresh files are checked and atomically replaced;
staged copies are cleared and the marker removed. Invalid refresh or uncertain
cleanup quarantines the store. Other agent-created private state is retained,
not exported; it can still contain secrets and requires reviewed retention.

Stdio is the worker lifeline. Independent supervisors remove agent/tool containers
after EOF, deadlines or worker death. If a supervisor/host itself dies, do not
infer cleanup: retain quarantine and inspect exact resources before operator
recovery. Auth directories are not shared across hosts. Draining stops new claims,
not active leases.

Unknown provider outcomes remain unknown after local cleanup. A reserved prompt
without an accepted receipt blocks automatic retry. Artifact or completion
acknowledgement replay uses existing immutable receipts and request deduplication;
it never repeats a provider invocation.
