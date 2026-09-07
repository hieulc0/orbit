# Orbit repository instructions

## Scope

These rules apply to work in this repository. Preserve existing user changes
and avoid broad or destructive commands such as `git reset --hard` or recursive
deletion.

## Project checks

For Rust changes, run the smallest relevant checks and, before handoff when
practical:

```sh
cargo fmt --all -- --check
cargo test --locked
cargo clippy --locked --all-targets --all-features -- -D warnings
```

The PostgreSQL/process qualification tests are ignored by default. Start the
disposable database with `docker compose up -d --wait`, then run:

```sh
ORBIT_TEST_DATABASE_URL=postgres://orbit:orbit-local-test@127.0.0.1:55439/orbit \
ORBIT_EVIDENCE_DIR="$PWD/target/qualification" \
cargo test --locked --features fault-injection --test kernel -- --ignored
```

Do not describe Milestone 1 as accepted from passing tests alone. Check the
acceptance document for the required evidence review, failure-matrix mapping,
and Orbit-on-Orbit dogfooding gates.

## Security and data handling

- Never commit `.env`, runtime server configurations, credentials, lease tokens,
  or generated evidence/workspaces.
- Keep `.codex/` local; it contains developer-machine agent configuration.
- Treat `target/qualification` as disposable local evidence. Review and export
  it before sharing, and inspect command arguments and artifact contents for
  secrets that structured redaction cannot detect.
- Keep repository workers on disposable fixtures or explicitly approved local
  workspaces. Do not push, deploy, or modify a developer checkout as part of
  qualification.

## Change discipline

- Use `apply_patch` for focused file edits.
- Keep commits and patches narrowly scoped to the requested task.
- Update the relevant documentation when behavior, test commands, or milestone
  status changes.
- Report checks run, checks skipped, and any remaining acceptance gaps.
