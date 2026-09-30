# Candidate-bound verification

Authoritative verification is Orbit-controlled execution against an exact candidate.
Agent text and exploratory tool commands are evidence about agent activity; they
cannot qualify a workflow. Code ownership is in
[the subsystem map](../architecture/subsystem-ownership.md).

## Identities and immutable inputs

| Record | Contract |
| --- | --- |
| WorkspaceState | Baseline, HEAD and content identity for the candidate |
| VerificationPlan | Versioned ordered checks, dependencies, command specifications and bounds |
| VerificationPolicy | Allowed commands, time/output limits and required specialized checks |
| VerificationEnvironmentPolicy | Explicit inherited/set/denied environment-variable policy |
| VerificationRun | Attempt, candidate, plan/policy/environment identity and normalized outcome |
| VerificationStepRun | Check identity, process outcome, timing and bounded output/artifact evidence |

Disk candidates use version 2 identities including tracked Git changes and eligible
untracked files. Legacy snapshot identities retain their original encoding. Observation
errors fail closed. A passing run for candidate A remains historical evidence after a
mutation creates candidate B, but cannot qualify B. Review, verification and completion
recheck the candidate. A failed diff cannot be interpreted as no changes.

## Command and environment policy

Commands use explicit argv, workspace-relative cwd, authorized environment values,
timeouts and bounded output. Executables and argument prefixes must match policy.
No implicit shell, host environment inheritance or provider credentials are granted.
Only intentionally supplied environment/resource values enter the execution environment;
secret values and a complete host environment dump are not evidence.

Final workflow verification requires an independent digest-pinned rootless OCI
profile. The candidate and Git metadata have controlled access, networking follows
policy, and required tools/dependencies are provisioned before execution. Missing
runtime/image/capability fails; there is no generic host or Cargo/docs fallback.
Clean environments must not inherit an implementer's mutable build/service state.
Caches are explicit policy and cannot silently become accepted candidate input.

## Outcomes and cleanup

Distinguish a check that ran and failed from one Orbit could not execute. Process
outcomes retain numeric exit, signal, timeout or unknown status. Typed timeout,
cancellation and cleanup uncertainty survive diagnostic context; message substring
matching must not determine the result. Optional skipped checks remain distinguishable
from passing checks, and required checks cannot qualify a run when skipped.

Cancellation and deadlines reach owned process groups and isolated execution resources.
Cleanup confirms actual resource absence before publishing success. For Podman, only
its documented absence exit status establishes removal; command failure or timeout
leaves cleanup unconfirmed. Cleanup uncertainty takes precedence over a successful
check or cancellation and preserves fencing/recovery evidence.

## Regression selection

FAST, STANDARD and FULL select checks under a pinned SelectionPolicy and optional
RegressionPolicy. Selection records retain selected/skipped checks, reasons, policy
and selection digests, change classification and dependency coverage. Agents cannot
redefine authoritative tests or lower a pinned required tier.

Legacy selection digests retain their serialized encoding. Interactive policies opt
into canonical JSON under a new immutable policy identity/version. Documentation
flow reductions require explicit low risk and observed documentation-only changes;
code, manifests, configuration, unknown or empty changes escalate. A pinned regression
policy can require stronger checks. See [skill flows](../guides/interactive-execution.md#skill-flows).

## Managed integration environments

IntegrationEnvironmentSpec pins services, dependencies, readiness probes and lifecycle
bounds. Startup follows a validated dependency order and persists service/run evidence.
Readiness is observed through authorized bounded probes, not assumed from process
creation. ReadinessTimeout remains a typed environment outcome.

Managed services use isolated resources and explicitly controlled networking. Teardown
records each result and confirms service/network removal. Environment failure and
uncertain cleanup cannot count as passing verification. Do not reuse a live installation
as a disposable fixture or infer cleanup after a supervisor/host failure.

## Browser verification

BrowserVerificationSpec pins the harness/backend, test cases, timeouts, console/page
error/network policy and screenshot/trace/video capture rules. Browser results retain
per-test outcomes and bounded artifacts. Capture failures are visible; absence of an
artifact is not a successful capture. Process status identifies crashes when structured
harness output is missing. Browser tests use the managed environment and candidate
identity of their parent verification run.

## Operator qualification

[Testing setup](../development/testing.md) provisions disposable databases and pinned
local runtimes. [Workflow qualification](../development/workflow-qualification.md)
provides focused commands and fencing/completion assertions. Tests ignored without
explicit prerequisites are not passes. Backend parity, real GUI behavior, live provider
semantics and separately hosted deployment require their own observed evidence.
