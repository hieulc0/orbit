# Private verified package registry

The registry stores immutable `orbit.package/v1` manifests in PostgreSQL. It is
a private catalog and distribution surface, not a public marketplace. Publishing
a package does not load code, pull images, install dependencies or start a run.
Worker provisioning is an explicit operator action outside the server process.

A manifest contains `apiVersion`, namespace, name, numeric `major.minor.patch`
version, description, `capabilities`, and `definitions`. It must contain at least
one capability or definition, with at most 64 of each and 1 MiB total canonical
JSON. Names use bounded ASCII identifiers; namespaces are `name` or
`organization/project`. Mutable version aliases and prerelease/range resolution
are deliberately unsupported.

Definitions are canonical Orbit definitions validated by the existing compiler.
Capability entries describe a pinned OCI worker image, `protocol_version:
orbit/v0`, input/output schema objects and optional UI metadata. Schemas/UI data
are signed metadata for the external worker/client, not executable server hooks
or a claim of exhaustive JSON Schema validation. Images must use SHA-256 digests;
the registry does not fetch or certify their content or behavior. All runtime
capability authorization, bindings, resources and environment policies still
apply when a packaged definition is submitted.

## Signing and trust

Configure `trusted_publishers` as key ID -> `{public_key, namespaces}`. Public keys
are hex-encoded Ed25519 keys; no private signing key is accepted by the server.
Keys are authorized only for their explicit namespaces. There is no default
trusted publisher. The signed envelope contains `manifest`, `digest`, `key_id`
and hex `signature`.

Format v1 canonicalization serializes the typed manifest as compact Rust
`serde_json::Value` JSON, recursively sorting object keys and including typed
default fields. It is not advertised as RFC 8785/JCS. Use Orbit's digest command
or Rust manifest API for the exact bytes, especially with numeric schema metadata.
The Ed25519 message is UTF-8 `orbit.package/v1\n` followed by the lowercase
SHA-256 manifest digest. The domain prefix is part of the signed bytes.

```sh
orbit package-digest manifest.json
# Sign the returned signing_message_hex with your external Ed25519 signing system.
# Assemble a signed envelope in a local file, keeping private keys outside Orbit.
orbit publish-package signed-package.json --scope acme/research/development
orbit packages --scope acme/research/development
orbit package DIGEST --scope acme/research/development
orbit run-package DIGEST DEFINITION_NAME --scope acme/research/development \
  --request-id stable-submission-key
```

The implementation uses Ed25519 [strict signature verification](https://docs.rs/ed25519-dalek/2.2.0/ed25519_dalek/struct.VerifyingKey.html#method.verify_strict).
Signature validity means the trusted key signed the manifest; it does not mean
the worker image is safe, audited, effective or endorsed by Orbit.

## API and lifecycle

`POST /packages` takes `{package, scope}`. `GET /packages` lists the latest 100
versions; `GET /packages/{digest}` returns a verified envelope. Reads accept
`?scope=organization/project/environment`, defaulting to the configured scope.
Governed registries require scope, `package.read/publish` permission, and a
namespace matching that scope's organization/project. Legacy deployments use
operator-only unscoped catalogs.

Version and signature envelope are immutable. Concurrent identical publication
is idempotent; a different envelope for the same version conflicts. Every package
read rechecks its requested digest, current key trust and signature. Removing a
publisher blocks future reads/submissions via `run-package`; listings mark the
package unverified. Already accepted plans remain immutable and are not silently
cancelled by key revocation. No delete/yank API is provided in this increment.

`run-package` fetches a verified digest, extracts a named definition and submits
it through the ordinary API. The accepted plan pins the actual definition,
bindings and scope; the CLI also prints source package digest/request ID to
stderr. There is no separate persisted package-source lineage field on the run.

## Qualification and SDK contract

Regular tests reject altered manifests, mismatched namespaces, revoked/unknown
keys, invalid signatures and noncanonical versions. The PostgreSQL/HTTP case
qualifies concurrent publication, immutable conflicts, reconnect verification,
revocation, unauthorized writes, stored-envelope corruption and the real
`run-package` CLI path. No worker code is executed by that test.

The [worker compatibility contract](../sdk/PROTOCOL_COMPATIBILITY.md) fixes the
additive wire rules shared by Rust/Python runtimes. `GET /protocol`, `orbit
protocol` and the SDK `protocol()` helpers expose supported versions. Public
marketplace operations, package dependency resolution, automatic installation,
signing-key custody and external publishing are outside this private registry.
