# Phase 3 developer contract

Phase 3 adds journal streaming, JSONL and reusable worker transports. It preserves
the existing unprefixed HTTP routes, `orbit/v0` worker protocol and both definition
versions. YAML and JSON definitions use the same parser and validation rules.
These SDKs target the existing repository capabilities; adding arbitrary compute
capabilities and execution runtimes remains subsequent roadmap work.

## Compatibility

Existing request fields, enum spellings and successful response shapes are retained.
New optional response fields may be added; clients should ignore unknown response
fields. Strict definition and request schemas remain strict. Breaking wire changes
require a new explicit protocol version, not reinterpretation of `orbit/v0`.
The crate is pre-1.0; this wire compatibility commitment does not promise Rust ABI
stability or rolling mixed-version database upgrades.

All HTTP routes require bearer authentication. Operator and worker credentials are
distinct. Operator routes are `/runs` (GET/POST), `/runs/{id}` (GET),
`/runs/{id}/events` (GET), `/runs/{id}/events/stream` (GET),
`/runs/{id}/cancel` (POST), `/runs/{id}/signals` (POST), and `/limits` (GET/POST).
Worker routes are `/worker/register`, `/worker/claim`, `/worker/operate`,
`/worker/upload` (POST) and `/worker/runs/{run}/attempts/{attempt}` (GET).
`/runs/{run}/artifacts/{artifact}` returns binary bytes to operators or authorized
workers. Worker bodies and lifecycle are defined in [worker protocol](WORKER_PROTOCOL.md).

Domain errors retain `{"error":"message"}`: 401 for authentication/authorization,
409 for request conflicts, 429 for admission backpressure (with `Retry-After: 1`),
and 400 for other rejected domain requests. Framework extraction errors (invalid
JSON/query/path/body size) can have plain-text bodies; clients must use the HTTP
status and must not parse human error messages as machine codes. Successful worker
operation responses also carry a `status`: HTTP 200 alone does not grant ownership.
Duplicates return the original receipt. Do not infer exactly-once external effects.

## Durable journal cursors and SSE

`GET /runs/{id}/events` retains its full JSON array response. Adding `?after=N`
returns at most 256 records with `sequence > N`, ordered ascending. Cursor values
are nonnegative signed 64-bit integers, scoped to one run. An empty page means
there are currently no later records. The cursor form validates run existence.

`GET /runs/{id}/events/stream?after=N` returns `text/event-stream`:

```text
id: 1
event: journal
data: {"sequence":1,"at":"...","event":{"type":"RUN_ACCEPTED","actor":"operator"}}

```

The `Last-Event-ID` request header takes precedence over `after`; omit both to
replay from the start. Invalid or negative cursors fail before opening the stream.
The stream reads committed PostgreSQL history in pages of 256 and polls every
250 ms when caught up. SSE comment keepalives maintain idle connections. Slow
consumers retain at most one page in application memory; there is no in-memory
event bus or replay buffer to lose on restart. Streams stay open after terminal
run state to allow later audit events. Disconnect explicitly when finished.

On transport/database failure, reconnect with the last processed sequence.
Deduplicate by `(run_id, sequence)` if the consumer can crash between processing
and saving its cursor. A cursor beyond the journal tail waits for it to catch up.
Retained history is required for replay; no retention/compaction policy is added.
Browser clients need a bearer-capable fetch/SSE client; native EventSource cannot
set the Authorization header. Credentials must not be put in query strings.

## CLI

```sh
orbit runs --output jsonl
orbit events RUN_ID --output jsonl
orbit events RUN_ID --after 12 --output jsonl
orbit events RUN_ID --after 12 --follow
```

`--output-format json` is the global default (pretty JSON). `runs` and `events`
also accept `--output json|jsonl`; existing artifact/evidence `--output` paths
retain their meaning. JSONL emits one compact object
per array element, or one compact value for a non-array response. Empty arrays
emit no lines. Diagnostics and submission keys go to stderr. `events --follow`
always emits JSONL, flushes each record, polls the cursor endpoint, and continues
until interrupted; HTTP errors exit nonzero. Resume with the last printed sequence.
Without `--follow`, `--after` returns one bounded page. Existing commands retain
their names and arguments. Clap usage errors exit 2; runtime errors exit nonzero;
successful commands exit 0. Server and worker commands have no result document.

## Rust SDK

Use the local `orbit` crate's `orbit::sdk` module. It exports `Client`, `Assignment`,
`Claim`, `Operation`, `Action`, `Artifact`, `Failure`, `Recovery`, `Registration`,
`Upload`, and `operation`.
`Client::register`, `claim`, `get_attempt`, `send_operation`, `upload`, and
`artifact` cover the worker transport. `operation(&assignment, Action::Heartbeat)`
creates a new request; retain the result when retransmitting. `send_operation`
retries that identical request up to three times and rejects non-accepted receipts.
`artifact` checks size and checksum. The built-in `worker::execute` runtime handles
repository work and independent heartbeats; SDK consumers own their execution loop.
For an executable registration example, run `cargo run --example sdk_register`
with `ORBIT_URL` and a configured `ORBIT_TOKEN` worker credential.

## Python SDK

Install from source with `pip install ./sdk/python`, or set `PYTHONPATH=sdk/python`.
No runtime dependencies beyond Python 3.10+ are required.

```python
from uuid import uuid4
from orbit_worker import Client, operation
import os

client = Client(os.environ["ORBIT_URL"], os.environ["ORBIT_TOKEN"])
client.register(["repository.code"])
claim_id = str(uuid4())  # retain across an uncertain claim response
receipt = client.claim("repository.code", request_id=claim_id)
if receipt["status"] == "accepted":
    assignment = receipt["assignment"]
    # Validate lease/deadline before executing; start must be acknowledged first.
    start = operation(assignment, "start")
    client.send_operation(start)
    # The runtime now owns heartbeat scheduling and stopping on lost ownership.
```

This is a transport SDK, not an automatic Python task executor. Calls are blocking,
with a 30-second default timeout that can be reduced to fit a lease. Python performs
no automatic mutation retries. Preserve claim IDs, prepared artifact receipts, and
operation bodies until acknowledged; retransmit unchanged or inspect the attempt.
For artifact upload, send `prepare_artifact` with SHA-256/size, then build a
`finalize_artifact` operation from its artifact ID and call `upload(operation, bytes)`.
Download through `artifact(run_id, metadata)` to verify size and checksum.
`OrbitError` exposes status/body without printing response contents automatically.
Both SDKs require independent heartbeat scheduling and termination of work before
unconfirmed lease expiry. Neither supplies an untrusted-code sandbox.

## Verification

```sh
cargo fmt --all -- --check
cargo test --locked
cargo clippy --locked --all-targets --all-features -- -D warnings
PYTHONPATH=sdk/python python3 -m unittest discover -s sdk/python -p 'test_*.py'
```

The ignored PostgreSQL test `phase3::journal_stream_replay_and_sdk_contract` covers
operator-only SSE, malformed cursors, replay and resume, typed Rust registration/
idempotent claim/start/inspection, and CLI JSONL parsing. The Python localhost HTTP
test checks authentication, unchanged operation retransmission and corrupt artifact
rejection. Run the full database/process suite using the [runbook](LOCAL_RUNBOOK.md).
`phase3::journal_resume_after_server_kill` reconnects after killing the real server
and checks the resumed stream against the durable journal, including cancellation
committed while the server is down.
These tests do not establish streaming throughput, browser compatibility, or SDK
package publication. SDK packages remain local source artifacts.

Phase 3 qualification is complete; the full case mapping, runtime/fixture fixes,
verified evidence export and final concurrent/serial results are recorded in
[Phase 3 qualification](PHASE_3_QUALIFICATION.md). The built-in Rust runtime uses
the server's remaining-lease duration receipts as described in the
[worker protocol](WORKER_PROTOCOL.md).
