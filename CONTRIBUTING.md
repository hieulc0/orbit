# Contributing

Read [the architecture/code map](docs/architecture/README.md), then the contract
for the behavior you are changing. [AGENTS.md](AGENTS.md) applies to coding agents.

Use `bash scripts/check.sh` for regular Rust/Python/UI checks. The full ignored
suite needs deliberately provisioned disposable services; follow
[testing](docs/development/testing.md). Do not make a production system a fixture.

Keep patches focused. Add regression tests for changes to scheduling, leases,
authorization, storage, protocol compatibility or lifecycle. Preserve old plan
digests and explicit wire versions. Update the relevant reference and operational
instructions when behavior changes, rather than rewriting archived evidence.

Before review, report checks run/skipped and known limits. Never commit runtime
configuration, credentials, developer agent settings, generated workspaces or
unreviewed evidence. Publication and deployment are separate authorized actions.

New dependencies should have a concrete need, compatible licensing and a locked
resolution. The project is Apache-2.0; do not submit code you cannot license under
the repository license. Security-sensitive reports belong in a private channel,
not public issues; see [security](SECURITY.md).
