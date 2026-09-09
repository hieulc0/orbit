# Phase 3 qualification

Status: complete for the bounded local developer contract on 2026-09-09.

## Final results

All **28 PostgreSQL/process tests passed** with the default concurrent runner in
**13.21 seconds**. The full serial run also passed **28/28** in **50.09 seconds**.
All **7 regular Rust tests**, the Python localhost transport test, formatting,
Clippy with all targets/features and warnings denied, and `git diff --check` passed.
No required Phase 3 check was skipped. Earlier unsuccessful runs remain diagnostic
history; completion is based on these final runs after the fixes below.

Scope: the local developer surface, preserving the existing definition and worker
protocols. See [developer contract](DEVELOPER_SURFACE.md) for API/CLI compatibility,
SSE cursor semantics and SDK lifecycle responsibilities. This record does not
change Milestone 1's separate owner acceptance or claim production readiness.

## Qualification fixes

The initial full suite encountered worker heartbeat and ownership failures. An
unchanged `8ead8c78145e9503f00cde900788736ada97a851` checkout reproduced the graph
worker heartbeat failure. Investigation found synchronous artifact publication
and filesystem sync inside the shared database coordination lock. That could
block both async execution and heartbeats while storage was slow.

Uploads now release the authorization transaction before publication and run
blocking file I/O outside the async executor. Finalization rechecks authority
after publication. Existing-file publication also syncs the directory, so a
concurrent retransmission cannot acknowledge another publisher's unsynced link.
The new stalled-upload test outlives the original lease while renewing it, then
cancels the run and confirms the released upload cannot finalize its object.

The timer cancellation test previously waited for an unrelated run to finish
before asserting cancellation had completed. It now waits for the cancelled
run's own terminal transition. Production cancellation still persists intent
first and completes through reconciliation; the test no longer assumes these
different runs must finish in a particular order. No lease durations, retry
limits or execution deadlines were relaxed.

Further runs identified a distinct heartbeat issue: the worker used its polling
interval as an acknowledgement timeout, even while its confirmed lease was valid.
Start/heartbeat receipts now expose the remaining lease duration. The runtime
anchors that duration before sending the request, waits only within its previously
confirmed lease, and stops on missing/late acknowledgement. Delayed-start and
heartbeat fault tests check both tolerance of responses slower than the polling
interval and actual process termination on loss of confirmed authority.

Manual artifact fixtures now renew leases during publication, like real workers,
instead of assuming several disk syncs always finish within a three-second lease.
The pagination fixture uses transitions of a 256-step graph to generate three
journal pages in a few transactions. It does not create a separate commit flood
alongside unrelated lease tests; no pagination coverage or bounds are reduced.

## Executable evidence mapping

| Required property | Test | Retained scenario |
| --- | --- | --- |
| Operator-only SSE, cursor validation, live delivery, exclusive replay; Rust SDK registration, claim deduplication, start/inspection; CLI JSONL | `phase3::journal_stream_replay_and_sdk_contract` | `phase3-stream-and-sdk` |
| Replay after real server kill, including cancellation committed while down | `phase3::journal_resume_after_server_kill` | `phase3-stream-server-kill` |
| More than 256 real journal records, exact page boundaries, resumed/slow-consumer ordering and flushed CLI follow output | `phase3::journal_pages_and_cli_follow_preserve_order` | `phase3-journal-pages-cli-follow` |
| Python SDK through real HTTP/PostgreSQL: registration/claim/start/heartbeat/inspection, corrupt-upload rejection, concurrent idempotent publication, artifact verification, completion deduplication/conflict, terminal fencing and no-work | `phase3::python_sdk_live_protocol_and_concurrent_upload` | `phase3-python-sdk-live` |
| Upload stalls beyond original lease, independent renewal/cancellation, finalization fencing and duplicate rejection receipt | `phase3::stalled_upload_allows_renewal_and_fences_cancelled_publication` | `phase3-stalled-upload-cancellation` |
| Heartbeat response exceeds polling interval within confirmed lease; lost acknowledgement stops the subprocess | `phase3::worker_uses_confirmed_lease_budget_and_stops_on_lost_ack` | `phase3-delayed-heartbeat`, `phase3-unconfirmed-lease-stop` |
| Late start acknowledgement cannot create a workspace or execute commands | `phase3::worker_never_executes_after_expired_start_ack` | `phase3-expired-start-ack` |
| JSON definition input, JSONL framing, usage exit code, existing artifact/evidence output flags | `cli_jsonl_and_json_definition_contract` | Regular test |
| Python transport authentication, unchanged retransmission and checksum rejection | `ProtocolTest.test_transport_and_retransmission` | Python localhost test |

The remaining 21 PostgreSQL/process cases retain Phase 1/2 worker recovery,
transaction-boundary kills, artifact validation, signals, timers, fan-out, child
execution and cross-server concurrency/admission regression coverage. See
[Phase 2 qualification](PHASE_2_QUALIFICATION.md) for their mapping.

## Commands

```sh
docker compose up -d --wait
cargo fmt --all -- --check
cargo test --locked
cargo clippy --locked --all-targets --all-features -- -D warnings
PYTHONPATH=sdk/python python3 -m unittest discover -s sdk/python -p 'test_*.py'
ORBIT_TEST_DATABASE_URL=postgres://orbit:orbit-local-test@127.0.0.1:55439/orbit \
ORBIT_EVIDENCE_DIR="$PWD/target/qualification-phase3-concurrent" \
cargo test --locked --features fault-injection --test kernel -- --ignored
```

The serial evidence is under `target/qualification-phase3-qualified`, from the
same command with `--test-threads=1` added after `--ignored`.

Run these Cargo commands sequentially. A default-feature build in the same target
directory can replace the fault-enabled `orbit` binary while process tests are
spawning servers. That invalidates the fault-boundary tests. Do not run another
Cargo build/test against the same target directory during process qualification.

The fault-injection feature now permits a test to release a publication barrier
by creating `fault.release` alongside its `fault.marker`. Without that file the
existing process-kill barriers retain their behavior. This feature must remain
disabled in deployed binaries.

## Evidence and boundaries

The concurrent run was exported to `target/qualification-phase3-review`:
**324 manifest-listed files, including 52 artifact files**, with **1,019,184 bytes** covered by
the manifest. Every exported file's SHA-256 checksum was verified. Runtime
fixtures were excluded, exported command arguments were inspected, and all
exported content was scanned for credential fields and known fixture credentials
with no matches. The recovered calculator patch, provenance manifest, test
report and logs were also inspected. This is a local technical review; the
bundle retains `review_required: true` for the operator before any distribution.

No Phase 3 implementation or qualification gate remains open. Phase 4 compute
and artifacts is next. The results do not expand the separate Milestone 1
dogfooding/owner-review record.

The harness checks durable state/journal invariants and retains snapshots,
journals, definitions and artifact bytes. Runtime fixtures and isolated database
schemas remain local. Generated evidence must not be committed. Export excludes
fixtures and redacts structured credentials; command arguments and artifact
contents still require inspection before sharing.

Completion is limited to this local source SDK/API/CLI surface. Rust and Python
SDKs are transport libraries; their consumers still own heartbeat scheduling,
execution and stopping work when authority is lost. Qualification does not
establish throughput, a maximum number of streams/workers, browser compatibility,
rolling upgrades, package distribution/publication, storage-loss recovery or an
untrusted-code sandbox. Python is exercised directly from source, not through a
published package; the qualification environment has no `pip` installed.
