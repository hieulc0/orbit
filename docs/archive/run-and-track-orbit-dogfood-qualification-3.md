# Autonomous report qualification specification (Q3)

This is a historical qualification specification, not a result or current runbook.
Its identifiers and fixed inputs are retained for experiment traceability. Current
procedures are in [dogfooding](../development/dogfooding.md).

## Fixed identity and objective

Baseline: `fd9746007338f653f991e38bbf9075171f89371d`.
Selected runtime: Antigravity ACP, credential reference `antigravity-weedy`,
requested model `gemini-3.8-flash`, reasoning `high`, observed target
`gemini-3.8-flash-high`. Capability declarations alone do not establish activation.
The execution budget is 256 calls and cannot be raised after admission.

Exact engineering task:

> Implement a CLI qualification report for completed Orbit workflow runs. The report should use existing persisted AgentExecution observability data to help evaluate real dogfood runs, including task and execution outcomes, continuation behavior, termination reasons, durations, tool activity, validation results, and provider-reported token usage. Missing usage must remain explicitly unknown rather than being treated as zero. Provide useful human-readable output and machine-readable JSON, preserve existing behavior, and add tests.

## Experiment boundary

Submit through Orbit's coordinator, worker, capability resolution and isolated
Attempt workspace with the selected authenticated runtime. No direct provider
implementation, operator edits, implementation hints, in-flight budget increases,
unrecorded restart or silent model substitution are permitted. Observation and
independent validation after the run are permitted. Record failures as results.

Preserve Task, Attempt, AgentExecution and VerificationRun as distinct identities.
An Attempt owns its workspace; at most one execution mutates it at a time.
Provider chat history cannot substitute for Orbit-owned handoff/recovery state.

## Required measurements

Record run/task/attempt and ordered execution identities; requested/resolved/observed
model and reasoning; logical credential reference; runtime/image digests; capability
source; outcomes, termination reasons, durations, validation results and cleanup.
Never record credential contents. Missing usage and measurements remain unknown.

Where durable evidence supports them, record calls at first read, first mutation,
first compilation/test and first repair; final consumption, trajectory/turn counts,
broker success/failure counts and provider-reported usage completeness. Passive
observations of ranged reads, recoverable tool errors and budget exhaustion must
not be manufactured to improve the experiment's outcome.

Budget exhaustion must use its controlled lifecycle rather than an invented provider
failure. Genuine unresolved dispatch remains conservative. Independent validation
inspects the actual patch and requirements, executes the declared checks, and retains
raw evidence, exported manifests, failures and scope limits. No result is asserted
by this specification.
