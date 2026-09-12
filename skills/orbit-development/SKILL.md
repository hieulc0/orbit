---
name: orbit-development
description: Implement or review Orbit engine, worker, API, SDK or console changes using the repository contracts and relevant checks.
---

Read the repository AGENTS.md, then docs/architecture/README.md for module ownership.
Use docs/README.md to select only the reference for the requested behavior; use
docs/ROADMAP.md for current gates. Do not use archive counts as current evidence.

Scheduling, cancellation, storage and authorization changes need invariants and
race/recovery tests, not only happy-path examples. Preserve immutable legacy
digests and post-I/O ownership checks. A UI or SDK change must not create a second
execution or policy authority.

Use bash scripts/check.sh rust, python, or ui for the relevant surface; use the
complete check before handoff when practical. Database/process behavior requires
the disposable setup in docs/development/testing.md. Update the canonical
reference when the contract changes and report any unverified gate explicitly.
