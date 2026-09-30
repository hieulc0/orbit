# Scheduling requirements

- [Idempotency and scheduling races](#idempotency-and-scheduling-races)
- [Credential admission policy](#credential-admission-policy)

## Lease and admission obligations

Claims and active operations require current generation and unexpired confirmed
ownership. Deadlines and attempt budgets do not reset on retry. Missing or late
acknowledgements stop effects before authority expires. Drain stops new claims
without revoking active leases. Capacity and scope come from server configuration;
workers and providers cannot widen them. Unknown provider outcomes require the
configured intervention policy, not automatic repetition.

## Idempotency and scheduling races

### Idempotency and races

| Case | Result |
| --- | --- |
| Same request ID, run, step, and JSON payload | Return the original receipt, including after run termination |
| Same request ID with a different run, step, or payload | Conflict; request IDs are scoped to operator signal operations across runs |
| A different request ID targets an already signaled wait | Conflict, even if the payload is identical |
| Unknown step or a step other than `engine.wait` | Reject without a receipt |
| Signal at or after an established wait deadline | Conflict, even before reconciliation processes expiry |
| Cancellation intent, intervention, or terminal run | Reject new signals; exact accepted-request retries still return the original receipt |
| Signal and cancellation race | Run lock serializes them; accepted intent blocks subsequent signaling and dependency release |
| Server dies before signal commit | No receipt or transition survives; retry can accept the signal |
| Server dies after commit but before response | Retry returns the committed receipt without a second event or transition |

Database time sampled after acquiring the run lock decides deadline validity.
New signal acceptance, journal entries, dependent transitions, and request
deduplication commit together. Cancellation preserves earlier receipts and terminal
tasks. Due timers cannot resume a cancelled run. Intervention pauses timer
completion and new signals, but does not reset existing signal wait deadlines.

## Credential admission policy

### Credential selection

Role runtime resolution applies reset-aware account selection over fresh,
credential-scoped availability evidence. The default safety thresholds are 15%
for a known 5-hour window and 5% for a known 7-day window; exact-threshold
observations remain eligible. Eligible accounts with known safe weekly quota are
ordered by earliest reset, then the role's provider preference, then stable
catalog credential ID and reference. Unknown or inapplicable quota windows remain
eligible under the existing availability policy and rank after known weekly
resets. Codex's configured Luna target uses the `default` bucket and ignores
`gpt-reserve`; Antigravity's Gemini target uses the Gemini group and ignores
the Claude/GPT group. Embedders can override the narrow defaults with
`WorkflowCoordinator::with_quota_selection_policy` and
`RuntimeQuotaSelectionPolicy`. Resolution reasons include observed headroom,
reset, rank and bounded rejection details. Automatic runtime re-resolution after
an execution quota failure is not implemented; use the workflow
[recovery procedures](../architecture/failure-recovery.md#pure-recovery-decisions).
