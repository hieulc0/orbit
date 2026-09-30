# External BA and SA reasoning

External conversations are transport and context. Orbit owns immutable reasoning
artifacts, the frozen contract, implementation state, technical evidence and
business acceptance. The existing `orbit-ba-bridge` conversation export is accepted
as transport provenance; its SQLite history does not become workflow authority.

## Connection policy

Launch separate `acp-serve` processes with operator configuration
`external_role: business_analyst` or `external_role: system_architect`. The remaining
repository, workspace, verification and skill configuration must match for a shared
session. There are no provider/role/tool grants in an artifact or prompt.

BA can submit requirements and challenges, freeze a resolved contract and attest
acceptance. SA can submit a proposal and resolutions, and use a separate read-only
investigation session for repository analysis. The analysis uses the existing
planner runtime and budgets; it is not implementation authority. External clients
cannot advance an implementation, apply/discard candidates, change modes, write
files or invoke a terminal. An editor/operator connection explicitly continues
the frozen implementation workflow. BA web research remains on the ChatGPT side;
Orbit records supplied external facts without asserting they were verified.

## Artifacts and revisions

All requests use the existing ACP session ID. The extension methods are:

| Method | Parameters beyond `sessionId` |
| --- | --- |
| `_orbit/reasoning/submit` | `expectedRevision`, `requestId`, `artifact` |
| `_orbit/reasoning/freeze` | `expectedRevision` |
| `_orbit/reasoning/accept` | `acceptance` |

`artifact` is `{"kind":"requirement_brief", "payload":{...}}`,
`technical_proposal`, `challenges`, or `resolutions`. Submit these in that order,
starting at revision 0. The reply contains the accepted revision. A request ID
is immutable: replay of the same actor/content/revision returns the prior result;
changed content or stale ownership is rejected. Each artifact is capped at 12 KiB.
Stored hashes and actor authority are checked when constructing the contract.

- RequirementBrief: objective, user_problem, functional_requirements,
  non_functional_requirements, external_facts, assumptions, acceptance_criteria
  (`id`, `criterion`), open_questions. This version requires resolved questions.
- TechnicalProposal: affected_subsystems, architecture, invariants, data_model,
  apis, migrations, security, failure_modes, verification_plan.
- Challenge: finding_id, category, claim, evidence, severity, requires_resolution.
- Resolution: finding_id, resolution, evidence.

Freeze at revision 4 produces a versioned AcceptanceContract containing all four
artifacts. Every required challenge needs a resolution, and unknown/duplicate
finding IDs are rejected. Freeze stores a canonical digest and binds one CREATED
workflow to it before the session becomes ready. Frozen artifacts cannot be changed.
Revision of frozen requirements requires a new session in this version.

## Acceptance

The normal conservative implementation flow runs through authoritative final
verification, then enters BUSINESS_ACCEPTANCE instead of COMPLETED. BA submits:

```json
{
  "contract_digest": "canonical frozen contract digest",
  "workspace_state_id": "exact reviewed and verified candidate identity",
  "criteria": [
    {"id":"criterion-id","satisfied":true,"evidence":"Observed requirement evidence"}
  ]
}
```

All frozen criteria must appear exactly once with a satisfied outcome and evidence.
The service checks technical completion, exact disk identity and cleanup before
accepting the attestation. It cannot override failed/stale verification or review.
Orbit's coordinator completes on a subsequent explicit continuation only after
matching business acceptance. A changed acceptance replay is rejected. BA claims
are business attestations; they do not substitute for technical verification.

## Development bridge integration

The companion development repository is `../orbit-ba-bridge`. It is independently
developed; use its existing ACP command options and exporter without treating its
conversation store as Orbit acceptance authority.

For a repository-aware SA turn, point the bridge's existing ACP command options
at Orbit and launch the bridge from the admitted repository:

```text
bridge-server --sa-acp-command /absolute/path/to/orbit \
  --sa-acp-arg acp-serve \
  --sa-acp-arg=--config \
  --sa-acp-arg /absolute/path/to/sa-config.json \
  --sa-acp-arg=--database-url-file \
  --sa-acp-arg /absolute/private/database-url-file
```

Use `external_role: system_architect`. Each bridge SA turn currently creates a new
read-only Orbit analysis session. The bridge's ordinary free-form transcript is
context; artifact submission is separate and explicit.

Export a conversation using the bridge's JSON exporter. A selected BA/SA message
must contain exactly one typed artifact as JSON (an optional single `json` code
fence is accepted). Submit it with the bounded mock/operator client:

```text
python3 scripts/ba-bridge-orbit.py \
  --orbit /absolute/path/to/orbit --config /absolute/path/to/ba-config.json \
  --database-url-file /absolute/private/database-url-file \
  --session editor-session-id --expected-revision 0 \
  --bridge-export discussion.json --message durable-bridge-message-id
```

The client preserves the bridge message ID as the idempotency request ID. It
rejects human turns, ambiguous IDs, free-form chat, oversized exports and untyped
payloads. Orbit independently enforces connection authority and revisions. This
operator client does not watch browser DOM or automatically grant approval.

Offline typed artifact exchange and state-machine qualification do not establish
live ChatGPT/browser acceptance. That gate requires an active authenticated bridge,
an identified conversation, actual BA/SA artifacts, real implementation/review/final
verification, and BA acceptance bound to the resulting candidate. No live bridge
repository files or existing conversation state are changed by this integration.
