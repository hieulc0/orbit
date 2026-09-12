# Operations console and definition studio

The React/TypeScript client lives in `ui/`. It is a peer over the existing REST
and durable journal APIs; no scheduling or authoritative validation moves into
the browser. Dependencies are locked in `ui/package-lock.json`.

```sh
cd ui
npm ci --ignore-scripts
npm run build
```

Set the server's optional `ui_directory` to the absolute `ui/dist` path, start
the server normally, and open `/console/` (including its trailing slash). The
server serves only that directory, with a same-origin Content Security Policy,
no framing, MIME sniffing or referrer disclosure. Hosting/deployment is not
performed by the build. Use TLS and a trusted origin outside loopback.

For development, `npm run dev` serves `http://127.0.0.1:5173/console/` and proxies
API paths to `http://127.0.0.1:7700`. Set `ORBIT_UI_API` when starting Vite to
change this development-only target. The production client always uses its own
origin. An operator bearer token is entered at connection and held only in
memory, never local/session storage, URLs, cookies or a bundled configuration.
Disconnect/reload clears it. The static page is public; every data/action API
still authenticates. Approval-only credentials use the CLI/API, not this
operator console. Governed user/service-account identities with run-read grants
can connect to their scoped views; worker/queue views require global grants.
Submission can select an explicit `organization/project/environment` scope.

## Operations

The console lists the latest 100 runs, filters by ID/state, inspects immutable
plans, dependency graphs, task/attempt states, failure reasons, budgets, signal
receipts, child IDs and artifacts. Downloads verify SHA-256/size before saving
and never render artifact content as HTML. Worker views show capabilities,
capacity, recent contact and active leases; recent contact is not a health
guarantee. Queues show ready/active counts by capability/pool.

The timeline polls bounded committed journal pages with an exclusive cursor,
deduplicates sequence numbers and retains at most 4,096 entries. It replays from
zero when opened, catching up page by page. The full journal remains available
through CLI/API. Cancellation, ordinary signals and assigned human decisions
require explicit confirmation. Signal/approval and submission request IDs are
retained for retries within the open page; copy them before closing an uncertain
request. The UI does not cancel merely because a page or network connection closes.

## Canonical definition editing

Import/export YAML or JSON, edit source, inspect a dependency graph, select steps,
add/delete steps and change dependency edges. Step panels derive their field
catalog from the server's Rust-generated JSON Schema. Primitive/nested fields
are edited as JSON, with the full schema available for inspection. Structured
edits reserialize the same canonical Definition as YAML; they normalize comments
and formatting. Export the original source first if those must be preserved.
The editor never maintains an alternative visual execution format.

The side-by-side source comparison uses an explicit baseline. Graph layout is
computed locally, is not execution metadata, and is not persisted in definitions.
Invalid/cyclic dependencies remain editable but are rejected by authoritative
server validation. `POST /definitions/validate` parses and validates source;
`GET /definitions/schema` exposes the structural Draft 2020-12 schema. Semantic
rules and configured binding permissions are enforced in Rust, including on
submission. Any source change disables submission until revalidation. Imports
are bounded to 1 MiB/256 steps, with bounded YAML alias expansion.

## Verification

`npm run build` performs strict TypeScript checking and produces a static bundle.
`npm test` runs Playwright Chromium cases for operations/decisions, confirmed
cancellation, credential handling, graph/source/panel synchronization,
validation/submission and mobile navigation. Install the matching test browser
with `npx playwright install chromium --only-shell` first; a local
`PLAYWRIGHT_BROWSERS_PATH` may keep it under disposable `target/`.
All five browser cases pass. The separate PostgreSQL-backed real-browser case
also passes, including schema editing, submission, assigned approval, CSP/static
serving and persisted completion. The complete release suite passes all 41 cases;
see [qualification](../archive/release-qualification-2026-09-12.md). This is not a production accessibility,
large-graph performance or cross-browser certification.
