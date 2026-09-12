# Worker SDK compatibility contract

The supported worker transport is `orbit/v0`, with additive optional fields for
`orbit/v1` graph, compute and agent execution. This is a wire compatibility
contract, not a claim that Rust struct literals or every SDK language API have
reached a frozen 1.0 release. The crate/package version remains 0.1.0 and no SDK
has been published externally as part of this implementation.

- Existing registration, claim, operation, upload, attempt inspection and artifact
  endpoints retain their meaning. New capabilities are opt-in and authorized by
  the server; a worker cannot obtain authority by advertising a capability.
- Unknown additive response fields must be tolerated. Unsupported definition,
  package or worker protocol versions must be rejected explicitly. Discovery is
  available at `GET /protocol` / SDK `protocol()`.
- Persist request IDs and bodies before sending. Retransmit the same body after
  an uncertain response. A transport retry is never an instruction to execute the
  task or provider effect again.
- Keep heartbeat and work execution independent. Anchor confirmed lease durations
  to send time, reject acknowledgements arriving after the previous lease expires,
  and stop work when authority/deadline cannot be confirmed. The Python SDK is a
  synchronous transport; the runtime owns this scheduling.
- Keep attempt workspaces unique. Publish exact artifact bytes and retain finalize
  request IDs; verify size/SHA-256 on reads. Never reuse a completion with different
  outputs. Recovery starts from immutable inputs, not an old working tree.
- Agent reservations precede external calls, are shared across attempts, and are
  never refunded. `reserve_agent_call` / `CallReservation` and `agent_report` /
  `AgentReport` describe this protocol, not an LLM implementation.
- Isolated coding adds optional `request_digest` to reservations and
  `finish_agent_call` / `CallReceipt` for attempt-bound outcome hashes. Old
  reservations remain valid. Replayed dispatch intent never authorizes repeating
  an effect; unresolved model calls prevent automatic retry. Generic operation
  transports can carry the additive receipt without a new wire version.
- Repository `execution` requirements pin operator `execution_profiles` and require
  server-authorized `execution.podman-v1`. Legacy workers cannot claim those steps.
  Private Git bindings use logical credential names, not embedded secrets. Legacy
  plans without the new fields keep their serialized digests. See the
  [remote coding guide](../docs/guides/remote-coding.md) for the narrow first backend.
- Treat model/provider credentials as runtime configuration. Assignments contain
  scoped contracts and lease tokens, not provider credentials. Never log/export
  lease tokens or include them in agent/container environment payloads.

Rust and Python conformance tests cover transport identity, exact retransmission,
artifact checksums and agent messages. Live PostgreSQL/HTTP qualification exercises
both SDKs, cancellation and real process recovery. The MCP adapter is separately
versioned as `2025-11-25` and is not the internal worker protocol.
