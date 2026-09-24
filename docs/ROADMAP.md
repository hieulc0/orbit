# Current roadmap

## Milestone: reproducible, deployable alpha

The bounded Phase 1–9 feature work is committed. Historical acceptance and
qualification remain in [the archive](archive/README.md); they are not a claim
that the complete long-term vision or production hardening is finished.

The alpha increment covers:

1. Current docs and a fresh-agent entry point, with preserved historical evidence.
2. Repeatable Rust/Python/UI checks and CI qualification with disposable services.
3. A non-root server image, Docker Compose, Podman Quadlet, and host worker units.
4. Probes, metrics, safe structured logs, SIGTERM handling and durable worker drain.
5. Backup/restore, coordinated upgrades, credential rotation and operational tests.
6. A trusted command-agent adapter and Orbit-on-Orbit work against a pinned baseline.

See [alpha qualification](operations/qualification.md) for implemented vs verified
items. A workflow is not qualified by the mere presence of its template or test.

## Acceptance gates

- A fresh checkout has one documented build/check path and no secret prerequisites
  for regular tests.
- A fresh installation can serve the UI/API, finish a worker-free workflow,
  restart with retained state, and preserve verified artifacts.
- Container shutdown and worker drain preserve the existing lease/retry semantics.
- A stopped deployment can be backed up and restored into an empty isolated
  destination, with run/journal/artifact agreement independently checked.
- A fresh agent can locate authoritative docs and run the relevant checks without
  relying on conversation history.
- Orbit-on-Orbit produces a reviewed patch and successful independent checks
  without changing or pushing the developer checkout.

## Next milestone: remote agent-assisted repository change

A remote worker completes an agent-assisted repository change, with scoped
credentials, isolated tools, independent tests, durable recovery, patch artifacts
and human review.

Implementation sequence:

1. Portable private remote Git bindings, pinned commits, logical credential
   references and fresh worker-local workspaces.
2. One live multi-turn coding runtime, together with the minimum execution policy,
   credential authorization and OCI tool containment needed to run it.
3. End-to-end qualification of independent verification, durable artifacts, human
   review and failure recovery.

Acceptance requires a separately hosted worker without a shared developer
checkout; an inspect/edit/test/revise cycle; independent tests on a fresh base
plus accepted patch; denied unauthorized tools/credentials/artifacts; no isolation
downgrade; and worker/server failure or lost acknowledgements preserving budgets,
fencing, artifacts and explicit uncertain provider outcomes. Human review uses the
existing durable approval boundary. Passing local fixtures is not live-provider
or remote-host qualification. Transparent conversation checkpoint resume is not
required.

Rootless Podman/OCI is the first backend. Model calls and Git materialization run
in trusted worker adapters; repository tools have no network or provider/Orbit
credentials. Existing governance and legacy plans remain compatible, and the
workflow must work without enabling organization/project/environment governance.

The implementation now includes these bindings and profiles, a bounded Responses
coding loop, attempt-scoped credential/tool authorization, tracked invocation
receipts, isolated workspaces and an independent-test/review example. See the
[setup guide](guides/remote-coding.md). Qualification is recorded separately;
implementation is not acceptance. The next acceptance work is a selected live
provider/model/account and a separately hosted worker, not more worker types.
See [remote coding qualification](operations/remote-coding-qualification.md) for
the passing local checks, reviewed artifacts and remaining live acceptance gates.

The immediate product increment is a repeatable
[submit, inspect and review workflow](guides/repository-review.md). `orbit export-run`
collects a private, journal-bounded snapshot and verified accepted artifacts using
existing read APIs. It supports human review without changing engine semantics or
claiming live acceptance. The actual CLI now passes the local PostgreSQL/rootless
Podman workflow across a pending-review server restart, with deduplicated approval
and byte-identical candidate artifacts; see the
[qualification record](operations/remote-coding-qualification.md#repository-review-export-qualification).
Repeated real tasks should establish useful outcomes and
expose onboarding/recovery friction before adding further adapters. A subsequent
non-repository workload should exercise existing compute/artifact/wait contracts;
extend the worker contract only when that workload demonstrates a specific gap.

## In progress: ACP agent integration

Add a worker-side ACP adapter alongside the existing Responses and command
adapters. The [design](architecture/acp-agent-integration.md) maps the external
proposal to current Orbit contracts; the
[implementation plan](development/acp-implementation-plan.md) tracks work packages
and failure/acceptance gates. The probe, pinned worker registry, execution-only
accounting, durable session batches, private auth supervision, confined file/terminal
broker, Codex App Server bridge and patch/test/review routing are implemented.
Real Codex and generic ACP offline workflows and all 60 shared local fault-regression
cases have passed. Later named agents and live account/host qualification are not
accepted.

Delivery order is Codex, official Google Antigravity ACP, then Claude. The pinned
Codex ACP adapter passes initialization but uses native tools. A version-specific
Codex App Server bridge hands effects to Orbit before execution; see the
[setup guide](guides/acp-coding.md) and [compatibility record](operations/acp-codex-compatibility.md).
Prioritize Codex-first live account/host acceptance for the selected workflow.
The specific terminal handoff gaps in
[Antigravity/Claude](operations/acp-agent-compatibility.md) remain follow-up work
when a selected workload requires those agents; they do not gate use of the
implemented Codex path.
A prompt may hide multiple model calls; unknown billing stays unknown.
Unresolved prompts retain intervention semantics, with automatic session resume
deferred. Agent and repository tools have distinct OCI resource/network policies;
agent host-network access is not provider-only egress.

This increment does not close or remove the current live-provider and
separate-worker acceptance gates above. Local fixtures precede a separately
authorized live account/host qualification; actual adapter compatibility and
reviewed evidence determine acceptance.

## Conditional follow-up work

The credential foundation now includes the registry, LocalPrivateSecretBackend,
provider-scope state, and catalog-backed status/availability persistence for
Codex and Antigravity. Codex device-code enrollment and Antigravity ACP/agy
enrollment and quota-group normalization are qualified; provider status evidence
is recorded in [provider status discovery](operations/provider-status-discovery.md).
The system still does not inventory or select resources, lease provider
capacity, or dispatch by role. Q7, resource-lease, role, review/repair, and
self-development qualification gates remain open.

4. Additional isolation backends when a defined threat model requires them:
   gVisor/runsc for sandboxed execution, Firecracker for untrusted execution.
5. Physical GPU qualification and additional GPU runtime support.
6. Kubernetes/elastic capacity when a selected deployment or scaling requirement
   justifies it. A second worker host alone is not such a requirement.
7. Tenancy, organization management, SSO or RBAC administration only for a concrete
   product requirement. This is separate from execution isolation.

Vault/cloud credential providers are optional integrations when selected accounts
require them. File/environment references suffice for the initial milestone.
Docker compute qualification, HA/throughput/retention, browser/accessibility,
public distribution and selected-cloud infrastructure retain their separate
evidence requirements. Do not add Kubernetes or Terraform speculatively.

Public publishing, deployments outside disposable local fixtures and paid model
calls require explicit authority and selected destinations/accounts.
