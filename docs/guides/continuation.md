# Continuation contracts and workspace snapshots

Orbit's continuation module provides provider-neutral records and pure decisions for
bounded execution chains. These helpers do not themselves launch a provider or commit
workflow transitions. The graph engine does not invoke `next_agent` or
`continuation_recovery_action`; automatic production role continuation is not implemented.
The workflow coordinator's bounded review/repair loop and runtime selection are
separate behaviors. See [workflow authority](../architecture/self-development-control-plane.md)
and [interactive execution](interactive-execution.md).

## Ownership and identity

An Attempt owns its workspace and can record sequential AgentExecutions. A receiving
execution uses the same candidate only after an authorized handoff, drift check and
confirmed prior cleanup. At most one mutating execution owns the candidate. Provider
credentials remain isolated; an auth lease cannot be reused while cleanup is uncertain.
A new Attempt retry starts from its declared immutable inputs and is distinct from
continuation within an Attempt.

Workspace snapshots are read-only. They record the immutable baseline, observed HEAD,
changed/added/deleted/untracked paths and binary diff digest. Snapshotting must not run
reset, checkout or clean. Legacy snapshot identity encoding is preserved; authoritative
disk verification additionally uses the [candidate identity contract](../reference/verification.md).

## Outcomes and triggers

NormalizedAgentResult separates provider/runtime termination from external validation.
FallbackPolicy is explicitly opt-in (`enabled: false` by default). Eligible configured
triggers can include turn limits, rate limits, quota exhaustion, timeout, agent errors
and validation failure. Cancellation, credential/infrastructure errors, process crashes
and unresolved external effects retain their safety semantics. Eligibility is a proposal
for the owner to evaluate, not permission to bypass fencing or redispatch uncertainty.

FallbackPolicy bounds execution count and identifies a fallback agent. ContinuationPolicy
uses ordered AgentCandidates, configured triggers, an execution bound and a same-failure
repetition bound. Candidate count and execution count are distinct: configured candidates
do not grant unlimited invocations. Runtime/model/reasoning/credential authorization
still applies to every proposed execution.

## Failure fingerprints

ValidationSummary can retain a deterministic FailureFingerprint (`validation/v1`).
Normalization removes volatile ANSI, timestamp, path and line/column details while
preserving failure identity. Fingerprint schema version is part of identity. Changing
failure content must not collapse into the same fingerprint merely because volatile
formatting was removed. Repetition can guide candidate progression or stop an exhausted
chain, but cannot establish success. A passing authoritative check and accepted candidate
are still required.

## Handoffs

HandoffRecord (`handoff/v1`) retains task/attempt/source-execution identity, trigger,
workspace snapshot, previous execution summary and bounded validation findings.
`build_handoff_prompt` derives provider-neutral context: original objective, prior outcome,
changed paths, command/exit findings and instructions to inspect the candidate and preserve
valid work. It excludes raw full diffs, logs and prior-provider private state.
Prior output is untrusted context, never a new tool or policy grant.

A durable owner must persist identity and accepted snapshot before launching a receiving
execution. Handoff, snapshot and validation replay require stable content/request identity.
Changed replay or unexpected disk drift cannot authorize launch. Success evidence must
refer to the final candidate, not an earlier handoff's passing checks.

## Pure recovery decisions

`continuation_recovery_action` derives an action from Attempt state, execution history,
validation/handoff records, policy and observed workspace digest. Terminal and cancelled
Attempts do not restart. Proposed actions include validation resumption, handoff preparation,
claiming a pending execution, reconciling a running execution or finalizing failure.
An owner executing such an action must add database claims, liveness checks, fencing and
post-I/O validation. Repeated reconciliation must not create duplicate dispatches.

Running/pending records do not prove process liveness. An expired owner or uncertain
external execution requires reconciliation rather than optimistic fallback. Cancellation
prevents a later handoff or launch even when prior evidence remains available. Provider
chat history and leftover workspace files cannot reconstruct missing accepted authority.

## Qualification scope

Pure selector/recovery tests establish deterministic contracts; they do not establish a
fully wired scheduler path, live-provider acceptance or exactly-once external execution.
[Testing](../development/testing.md) and [workflow qualification](../development/workflow-qualification.md)
identify the actual executed paths. Reports must distinguish these scopes explicitly.
