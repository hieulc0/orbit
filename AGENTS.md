# Orbit repository instructions

## Scope

These rules apply to work in this repository. Preserve existing user changes
and avoid broad or destructive commands such as `git reset --hard` or recursive
deletion.

## Start here

Read [the documentation index](docs/README.md) and
[current architecture/code map](docs/architecture/README.md). Current priorities
and acceptance gates are in [the roadmap](docs/ROADMAP.md). Read the relevant
reference before changing behavior; archived milestone records are historical,
not current installation instructions.

PostgreSQL owns accepted state. Preserve immutable legacy plan digests, request
deduplication and lease/generation fencing. Do not hold scheduler coordination
locks during storage/provider I/O; recheck authority after I/O. No interface may
bypass the existing authorization/engine boundary. Draining stops new claims,
not active leases. Worker or provider effects are never implicitly exactly-once.

## Project checks

For Rust changes, run the smallest relevant checks and, before handoff when
practical:

```sh
cargo fmt --all -- --check
cargo test --locked
cargo clippy --locked --all-targets --all-features -- -D warnings
```

`bash scripts/check.sh` also runs Python and strict UI checks. Use
`bash scripts/check.sh ui` for browser work. The PostgreSQL/process qualification
tests are ignored by default. Provision only the disposable prerequisites in
[testing](docs/development/testing.md), then use `bash scripts/qualify.sh`.
For a targeted database-only case:

```sh
ORBIT_TEST_DATABASE_URL=postgres://orbit:orbit-local-test@127.0.0.1:55439/orbit \
ORBIT_EVIDENCE_DIR="$PWD/target/qualification" \
cargo test --locked --features fault-injection --test kernel -- --ignored
```

Do not describe Milestone 1 as accepted from passing tests alone. Check the
acceptance document for the required evidence review, failure-matrix mapping,
and Orbit-on-Orbit dogfooding gates.
The acceptance document is [archived here](docs/archive/milestone-1-acceptance.md).

## Security and data handling

- Never commit `.env`, runtime server configurations, credentials, lease tokens,
  or generated evidence/workspaces.
- Keep `.codex/` local; it contains developer-machine agent configuration.
- Repository workflow skills are authored in `skills/` and reference canonical
  docs/scripts. See [onboarding](docs/development/agents.md) for `.agents/skills/`
  discovery. Keep essential instructions here; do not duplicate full docs in skills.
- Treat `target/qualification` as disposable local evidence. Review and export
  it before sharing, and inspect command arguments and artifact contents for
  secrets that structured redaction cannot detect.
- Keep repository workers on disposable fixtures or explicitly approved local
  workspaces. Do not push, deploy, or modify a developer checkout as part of
  qualification.
- `docker-compose.yml` is qualification infrastructure. `deploy/` contains
  deployment templates; never point tests at a live deployment or attach a
  runtime socket to the API server. No image publication or remote deployment is
  authorized merely by permission to build/test packaging.

## Change discipline

- Use `apply_patch` for focused file edits.
- Keep commits and patches narrowly scoped to the requested task.
- Update the relevant documentation when behavior, test commands, or milestone
  status changes.
- Report checks run, checks skipped, and any remaining acceptance gaps.
