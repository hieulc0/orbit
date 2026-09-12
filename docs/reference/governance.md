# Governance

Governance is an opt-in, server-configured authorization boundary. It models
organizations, projects, environments, roles, users, service accounts, agent
identities and integrations. This bounded release uses deployment configuration,
not an identity administration UI, SSO or a live policy-distribution service.
All replicas must deploy the same authorization configuration; changing it
requires a coordinated restart. Keep configurations and credential files local.

## Immutable execution scope

Submission accepts `scope: {organization_id, project_id, environment_id}`. The
configured `default_scope` is used if omitted. Governed submissions require a
known environment. Scope enters the immutable plan digest, workers cannot
redefine it, child runs inherit it, and parent linkage cannot cross scopes.
`submitted_by` retains the authenticated root submitter through child execution.
Unscoped legacy plans retain their digests and existing deployments work with
governance disabled.

```sh
orbit identity
orbit projects
orbit run definition.yaml --scope acme/research/development --request-id request-1
```

`GET /projects` returns only environments visible to the caller. Scoped run
listing filters in PostgreSQL before the latest-100 limit. Run inspection,
journal/SSE, artifacts, cancellation, signaling and approval all check resource
scope and action. Workers have independent server-owned scope allowlists;
scoped workers cannot claim unscoped runs or another environment's runs. Their
persisted profile detects conflicting capacity, capability or scope declarations
across replicas. Use a new worker identity when changing that profile.

## Roles and policy

`governance.roles` maps names to explicit action strings. `principals` map an
identity to a `kind`, credential reference and grants. A grant has a scope and
role names; an explicit `scope: null` grants those actions globally. A scoped
grant never grants global worker, queue, scheduler-limit or audit access.
Supported actions are enumerated in `src/governance.rs` and include
`run.read/submit/cancel/signal/approve`, `artifact.read`,
`definition.read/validate`, `worker.read`, `queue.read`, `limits.read/write`,
`audit.read` and `package.read/publish`.

Human approval additionally requires a `user` identity and assignment on the
step. Service accounts, agents and integrations cannot approve, even with an
approval role. The legacy operator remains an explicit global/break-glass
identity, but still cannot bypass a step's assignee list or environment admission
policy. Legacy approval-only tokens can decide only unscoped approvals.

Environment policies allow capabilities, repository IDs and agent binding names;
they cap per-step resources, agent budgets and per-run concurrency. Validation
walks nested child definitions before admission. These are per-run/task bounds,
not monthly organizational spending quotas. Model/tool credentials and provider
billing remain the trusted runtime's responsibility.

See [the example configuration](../../examples/server-governance.json). Replace its
credential references with operator-provisioned environment variables or private
files; it deliberately contains no usable credentials.

## Secret references

`operator_credential`, worker `credential`, and principal `credential` support:

```json
{"provider":"env","name":"ORBIT_SERVICE_CREDENTIAL"}
```

or `{"provider":"file","path":"/absolute/private/credential"}`. File references
must resolve to an owner-private regular file (no group/other permission bits),
at most 8 KiB; the final component cannot be a symlink. A trailing newline is
trimmed. Credential values must be 24–8,192 bytes, without CR/LF. They are resolved
at startup and never put in execution plans or API responses. Legacy literal
operator/worker tokens remain compatible, but cannot be combined with a reference
for the same identity. Duplicate credentials/identities are rejected.

File parent directories and server configuration must be trusted and protected.
There is no cloud vault, OAuth rotation or per-task secret delivery service here.
S3 credentials continue to use the artifact provider's environment references.

## Audit controls

With governance enabled, authorization of mutations and denied resource actions
is recorded in `orbit_audit`, with authenticated actor, action, scope, optional
run ID, decision and database timestamp. No request bodies, bearer credentials or
provider keys are copied into this journal. The record means authorization was
evaluated, not that a later mutation committed successfully. Committed run/attempt
effects remain in the transactional run journal with the correct actor.

`orbit audit --after CURSOR` / `GET /audit?after=CURSOR` returns up to 256 ordered
records. Each carries the previous record hash and SHA-256 of compact JSON
`[previous_hash,event]`. Concurrent servers serialize appends under the control
row lock. API callers cannot update/delete audit records. A database administrator
can still rewrite/truncate history; retain independently witnessed hash heads,
backups and restricted database credentials for stronger tamper evidence.
Retention, external witnessing, denied worker transport auditing, and policy
change distribution are not automated in this release.

## Qualification

Four regular governance tests cover scope digests, role isolation, policy limits
and secret-file restrictions. Two PostgreSQL/HTTP cases cover per-project lists,
cross-project denials, service-account restrictions, actual human decisions,
environment admission, hash-chain validation, conflicting worker profiles,
reconnection and immutable child scopes. Both pass in the full release suite;
see [release qualification](../archive/release-qualification-2026-09-12.md).
