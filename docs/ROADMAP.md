# Current and future work

Current behavior belongs in [architecture](architecture/README.md), interfaces in
[reference](reference/README.md), and acceptance constraints in
[requirements](requirements/README.md). This roadmap lists unfinished work only.

## Pending acceptance

- **Intent-based flow selection:** select bounded read-only, small-fix and larger
  engineering flows from intent and risk, and expose roles, profile and required
  verification before substantial work.
- **Editor action recovery:** qualify operator reconciliation after interrupted
  candidate application or discard in the selected editor. Normal reconnect and
  explicit candidate actions do not establish forced-crash recovery.
- **External BA/SA integration:** use the development bridge at `../orbit-ba-bridge`
  with an identified authenticated conversation, typed requirements/proposal artifacts,
  challenge resolution, a frozen acceptance contract, real implementation/review/verification,
  and BA attestation for the exact accepted candidate. Preserve existing bridge conversations.
- **Selected provider and worker deployments:** establish account-class eligibility,
  refresh/expiry, actual model semantics, quota behavior, repository-data/egress policy,
  separately hosted workers and recovery after worker/server failure. Qualification must
  use selected resources and explicit authority.
- **Backend and operational acceptance:** establish Docker compute parity where needed,
  coordinated object-store backup/recovery, physical GPU behavior and installation lifecycle
  on the selected deployment. Template validity and local fixtures are insufficient.

Live provider admission requires fresh credential-scoped quota evidence and the
[selection policy](requirements/scheduling.md). Refresh with
`credential status --all --quota` before execution and review. A queued run,
stale observation or initialization result cannot satisfy acceptance.

## Conditional extensions

Require a concrete workload or deployment need and separate qualification:

- Hostile-workload isolation, such as gVisor or Firecracker.
- Physical GPU execution and additional runtimes.
- Multi-host capacity, provider-capacity leases, HA, throughput and retention guarantees.
- Kubernetes or cloud provisioning for demonstrated capacity requirements.
- SSO, tenant administration, external policy distribution and additional secret backends.
- Additional credential/provider adapters and public package/SDK distribution.
- Continuation after prompt dispatch with explicit reconciliation of uncertain provider effects.
- Parallel analysis or isolated implementation branches with explicit integration.

Commit, merge, push, publication and deployment require explicit authority and a
selected destination. Future work cannot silently change accepted digests, grant
new effects to old bindings or downgrade isolation.
