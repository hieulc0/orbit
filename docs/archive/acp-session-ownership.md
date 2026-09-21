Implement the ACP session-ownership / interleaved-notification remediation
discovered by Orbit Dogfood Qualification #3.

Do NOT implement the CLI qualification-report feature.
Do NOT start Qualification #4.
Do NOT weaken side-effect uncertainty handling.

============================================================
CONTEXT
============================================================

Qualification #3 started from:

    fd9746007338f653f991e38bbf9075171f89371d

Configuration:

    agent: antigravity
    credential: antigravity-weedy
    model: gemini-3.8-flash
    reasoning_effort: high
    budget.calls: 256

Model resolution succeeded:

    requested: gemini-3.8-flash + high
    resolved:  gemini-3.8-flash-high
    actual:    gemini-3.8-flash-high

The coding task never started.

The run failed during initial ACP model-selection/session setup.

Diagnostic artifact:

    foreign agent response

No repository modifications occurred.

============================================================
CONFIRMED FAILURE SEQUENCE
============================================================

Current runtime ordering is effectively:

    session/new
        ↓
    receive valid session_id = S
        ↓
    select_model(...)
        ↓
    broker.session_id = Some(S)

This is incorrect.

During select_model(), Orbit sends:

    session/set_config_option

The real Antigravity ACP peer may legally emit a session-scoped
notification before returning the JSON-RPC response.

Observed sequence:

    Orbit:
        session/set_config_option
        id = orbit-3

    Antigravity:
        session/update
        sessionId = S

    Antigravity:
        response
        id = orbit-3

But when session/update arrives:

    broker.session_id == None

Broker::owner() therefore rejects the legitimate notification as:

    foreign session ID

and treats it as fatal.

acp_request exits before consuming the still-pending response for
orbit-3.

The next request then reads that stale response while expecting a
different request ID and fails with:

    foreign agent response

This produces wire desynchronization.

============================================================
OBJECTIVE
============================================================

Correct ACP session establishment and request/notification handling so
that legitimate session-scoped notifications may arrive immediately
after session/new and while Orbit is waiting for responses.

Required invariant:

    session/new returns session ID S
        ↓
    Orbit establishes broker ownership of S
        ↓
    any session-scoped ACP operation may begin

NOT:

    session/new
        ↓
    model selection
        ↓
    ownership established afterward

============================================================
1. ESTABLISH SESSION OWNERSHIP IMMEDIATELY
============================================================

Inspect src/acp_runtime.rs and related session initialization code.

After session/new successfully returns and its session ID has been
validated, establish:

    broker.session_id = Some(S)

BEFORE any session-scoped operation such as:

    session/set_config_option
    session/set_model
    session/set_mode
    session/prompt
    or any equivalent operation

The broker must know the authoritative session identity as soon as the
ACP session exists.

Do not disable owner/session validation.

Do not accept arbitrary session IDs.

The fix is ordering, not weakening validation.

============================================================
2. VERIFY REQUEST LOOP HANDLES INTERLEAVED NOTIFICATIONS
============================================================

Review acp_request / wire request handling.

ACP JSON-RPC communication is not necessarily:

    request
    response
    request
    response

A peer may produce:

    request A
        ↓
    notification
        ↓
    notification
        ↓
    response A

While waiting for response A, Orbit must:

    read frame
        ↓
    if legitimate notification:
        dispatch through broker
        continue waiting

    if peer callback/request:
        dispatch through broker
        send callback response
        continue waiting

    if response with expected ID:
        return response

The request must not be considered complete merely because an
unrelated legitimate notification arrived.

============================================================
3. DO NOT HIDE REAL RESPONSE-ID VIOLATIONS
============================================================

Do NOT weaken:

    expected response ID == received response ID

A response carrying the wrong request ID remains a protocol/integrity
problem unless the protocol implementation explicitly supports
multiple outstanding requests and routes them by ID.

The Q3 "foreign agent response" was downstream damage caused by the
previous request loop aborting before consuming its response.

Fix the original synchronization problem.

Do not simply ignore mismatched responses.

============================================================
4. SESSION NOTIFICATION OWNERSHIP
============================================================

After session/new establishes S:

    session/update(sessionId=S)
        → accepted

    legitimate callback(sessionId=S)
        → accepted subject to normal broker policy

But:

    session/update(sessionId=OTHER)
        → rejected

A real foreign session identity must remain fatal where the existing
security model requires it.

Do not make owner() permissive.

============================================================
5. MODEL-SELECTION LIFECYCLE
============================================================

Preserve the existing model-selection invariant:

    ExecutionIntent
        ↓
    resolve gemini-3.8-flash + high
        ↓
    session/new
        ↓
    establish broker session ownership
        ↓
    inspect current model
        ↓
    select requested model if needed
        ↓
    tolerate legitimate interleaved notifications
        ↓
    verify actual model
        ↓
    only then session/prompt

No prompt may execute before requested model activation is verified.

Do not reintroduce silent model substitution.

============================================================
6. SESSION MODE SELECTION
============================================================

Review the same ordering around:

    session/set_mode

or equivalent mode activation.

A peer may emit session/update or other legitimate notifications during
mode changes as well.

Do not implement a special-case fix only for model selection.

The request/notification machinery should work for all session-scoped
ACP requests.

============================================================
7. WIRE SYNCHRONIZATION
============================================================

Add a regression test reproducing the exact Q3 sequence:

    session/new
        → returns S

    Orbit establishes broker ownership S

    Orbit sends request:
        id = orbit-3
        method = session/set_config_option

    peer sends:
        session/update(sessionId=S)

    peer sends:
        response(id=orbit-3)

Verify:

    session/update accepted
    broker not poisoned
    response orbit-3 consumed by the correct request
    request completes successfully

Then send:

    request id=orbit-4

and verify:

    response id=orbit-4

is consumed normally.

There must be no stale orbit-3 response remaining on the wire.

============================================================
8. FOREIGN SESSION REGRESSION
============================================================

Test:

    established session = S

then peer sends:

    session/update(sessionId=OTHER)

Verify the existing security behavior remains enforced.

If this is defined as fatal:

    broker.poisoned == true
    execution terminates appropriately

Do not weaken this boundary.

============================================================
9. RESPONSE-ID REGRESSION
============================================================

Test a genuine response mismatch independently:

    Orbit sends id=orbit-10

    peer responds id=orbit-999

Verify Orbit still fails closed according to the existing protocol
policy.

Do not make the Q3 fix by accepting arbitrary response IDs.

============================================================
10. MULTIPLE INTERLEAVED NOTIFICATIONS
============================================================

Test:

    request orbit-20

    session/update S
    another legitimate notification S
    callback/request if supported by current test harness
    response orbit-20

Verify all intermediate messages are processed and Orbit continues
waiting for orbit-20.

This should establish the general ACP request-loop behavior rather
than only the exact one-notification Q3 case.

============================================================
11. PRESERVE RECOVERABLE TOOL ERROR SEMANTICS
============================================================

Do not regress previous remediation:

    prohibited/missing file request
        ↓
    JSON-RPC error
        ↓
    broker not poisoned
        ↓
    same session continues

============================================================
12. PRESERVE BUDGET-EXHAUSTION SEMANTICS
============================================================

Do not regress:

    budget exhausted
        ↓
    ExecutionLimit / BudgetExhausted
        ↓
    controlled cancellation
        ↓
    broker not poisoned
        ↓
    outstanding prompt reservation settled
        ↓
    pending_model_call == false
        ↓
    AgentExecution Interrupted
        ↓
    deterministic failure

============================================================
13. PRESERVE GENUINE DISPATCH UNCERTAINTY
============================================================

Q3 ultimately produced:

    model dispatch outcome unresolved
    side_effect_status=unknown

Given the wire had already become desynchronized, conservative handling
was correct.

Do NOT weaken this safety mechanism.

A genuine unexpected transport failure with an unresolved model call
must still produce:

    pending_model_call == true
        ↓
    side_effect_status = unknown
        ↓
    NEEDS_INTERVENTION / conservative failure behavior

The objective is to prevent legitimate ACP notifications from causing
the transport failure in the first place.

============================================================
14. AGENTEXECUTION JOURNALING OBSERVATION
============================================================

Q3 reported:

    AgentExecution IDs: None

because execution failed during initial turn dispatch before an
AgentExecution journal record was created.

Inspect this behavior, but do NOT redesign AgentExecution persistence
as part of this remediation unless the missing record is clearly a
bug under the existing data model.

Report whether an AgentExecution is expected to exist once:

    task claimed
    ACP session created
    model selected
    prompt reservation created

or only after a later lifecycle point.

If this is an observability gap, document it for later.

Do not broaden this protocol fix unnecessarily.

============================================================
15. TEST AGAINST REAL ANTIGRAVITY RUNTIME
============================================================

Mocks are necessary but not sufficient because Q3 exposed behavior
from the real ACP peer.

After unit/integration tests pass, perform a narrow live protocol
preflight against the same pinned Antigravity runtime:

    localhost/orbit-antigravity@
    sha256:cdb11fed1c8570f1fdde0060161ab535ba26bc950ebca1307bf7b1cd5875e6f5

Use:

    antigravity-weedy
    gemini-3.8-flash
    reasoning_effort: high

The live preflight should prove:

    session/new succeeds
    ownership established
    model selection succeeds
    interleaved session/update is accepted if emitted
    mode selection succeeds
    actual model = gemini-3.8-flash-high

Do NOT submit the qualification-report engineering task.

Do not expose credential contents.

============================================================
16. VALIDATION
============================================================

Run:

    cargo fmt --check
    cargo clippy --all-targets
    cargo test

Report exact results and any intentionally ignored environment-specific
integration tests.

============================================================
17. DEFINITION OF DONE
============================================================

Complete when:

[ ] Broker ownership is established immediately after successful
    session/new.

[ ] No session-scoped operation begins before ownership is established.

[ ] Legitimate session/update for the active session is accepted during
    model selection.

[ ] Request processing continues waiting for the matching response
    after legitimate notifications/callbacks.

[ ] Matching response is consumed by the correct request.

[ ] Subsequent requests see no stale response.

[ ] Foreign session IDs remain rejected.

[ ] Genuine response-ID mismatches remain rejected.

[ ] Model verification remains mandatory before prompt.

[ ] Recoverable tool errors remain recoverable.

[ ] BudgetExhausted remains a controlled execution limit.

[ ] Genuine unresolved dispatch remains conservative.

[ ] Real Antigravity preflight succeeds.

[ ] cargo fmt --check passes.

[ ] cargo clippy --all-targets passes.

[ ] cargo test passes.

============================================================
18. STOP AFTER REMEDIATION
============================================================

Do NOT implement the qualification-report feature.

Do NOT start another qualification run.

After remediation report:

1. root cause
2. exact session-ordering fix
3. request-loop behavior
4. files changed
5. tests added
6. real Antigravity preflight result
7. AgentExecution journaling observation
8. validation results
9. remaining limitations

Then stop.

The remediation will be independently reviewed, committed, and only
then will the same engineering task be submitted again as the next
dogfood qualification.
