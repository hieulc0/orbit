# Trusted command-agent adapter

`orbit worker --capability agent.run --agent-runtime /absolute/private/runtime.json`
runs one operator-provisioned command per attempt. It selects no model SDK/account
or paid provider. The included Python program is an offline protocol demonstration,
not a reasoning model.

Start from [the runtime example](../../examples/command-agent-runtime.json) and
[Definition](../../examples/command-agent.yaml). Set real absolute executable/script
paths. Put the exact runtime `binding` under its `binding_name` in the server's
`agent_bindings`. Authorize a separate worker identity for `agent.run` and that
binding's runtime capability (`agent.command-demo-v1` here). Use distinct runtime
capabilities for incompatible bindings to avoid cross-claiming. Keep actual config
and credentials private and outside Git.

```sh
ORBIT_URL=http://127.0.0.1:7700 ORBIT_TOKEN_FILE=/absolute/private/agent.token \
  orbit worker --capability agent.run --agent-runtime /absolute/private/runtime.json \
  --workspaces /absolute/disposable/agent-workspaces
```

## Protocol and accounting

Commands get a fresh attempt directory, isolated HOME and cleared environment plus
normal command metadata/PATH. `ORBIT_AGENT_INPUT` points to `agent-input.json`,
format `orbit-command-agent/v1`: task, pinned AgentSpec/binding, run/attempt/generation,
idempotency key and reservation limits. No Orbit bearer credential, lease token or
full server config is included. Binding digest mismatch rejects dispatch.

Before dispatch, the adapter durably reserves `tokens_per_call` and
`cost_microusd_per_call` with the attempt ID as call ID. Replayed reservations never
authorize another dispatch. Retry does not reset budgets. Stdout is exactly JSON:

```json
{"output":{"result":"example"},"delegation_inputs":[]}
```

The adapter wraps/validates a provenance-bound report before publishing. Response
parsing is limited to 1 MiB; report output to 64 KiB. Stderr becomes a logs artifact
and may contain sensitive data. Nonzero exit, timeout, malformed output or uncertain
interruption cannot produce accepted success. Existing retry-exhaustion/intervention
rules apply. SIGTERM uses [bounded worker drain](../operations/observability.md#drain-and-shutdown).

## Provider boundary

`environment` maps approved names to file/environment SecretRef objects, e.g.
`PROVIDER_API_KEY: {"provider":"file","path":"/private/key"}`. Control variables,
loader configuration, HOME and PATH overrides are denied. Credentials resolve in
the trusted worker and are passed only to its command, not the control plane.

The operator's wrapper must pin the provider/model revision, honor supplied
permissions/token/cost ceilings and perform at most the reserved call. Orbit
accounts reservations, not actual provider billing. A subprocess is trusted code,
not a network/security sandbox: it can violate the contract or leak credentials.
This single-call adapter rejects tools. Multi-call/tool-loop runtimes should use
the [SDK reservation protocol](../reference/agents.md), reserving each call.

Before real use, explicitly choose a provider/account and qualify its budget,
timeout/idempotency, secret-handling and uncertain-dispatch behavior. Offline
tests are not live-model qualification.
