# Deployable alpha qualification

This is the current implementation record for 2026-09-12, following the committed
Phase 1–9 baseline `9b9e3d252615589104067d2d272124fc795b3f2b`. Historical counts and
owner decisions remain in [the archive](../archive/README.md). This increment does
not assert that the complete long-term vision or production hardening is finished.

## Evidence map

| Gate | Implementation and observed evidence |
| --- | --- |
| Current docs / fresh-agent entry | Indexed architecture/reference/guides/operations; historical records preserved; root AGENTS.md and two validated skills |
| Repeatable checks | 24 regular Rust tests, formatting, Clippy all targets/features with warnings denied; 2 Python SDK + 3 operations tests; strict TypeScript/UI build and 5 mocked Chromium cases |
| Durable regressions | 46 PostgreSQL/process/Podman/S3/real-browser cases passed together in 19.26s; the separate dogfood case also passed |
| Lifecycle / migration | `tests/kernel/operations.rs`: probe/metrics authority, durable drain/replay, bounded server SIGTERM with open stream, route redaction, pre-0006 migration preserving existing runs/journals/workers |
| Command agent | `tests/kernel/command_agent.rs`: actual subprocess, binding rejection before dispatch, durable reservation, credential isolation, typed output rejection, graceful/forced worker SIGTERM |
| Server image | Multi-stage pinned-base build completed; non-root/read-only bundled UI/API served real workflows under both Podman and Docker |
| Restart / backup / rotation | `scripts/backup-drill.py` passed under both runtimes; independent replacement database matched run, journal and accepted artifact bytes; old operator credential rejected after restart |
| Deployment templates | Compose config validated; Quadlet generator dry-run succeeded. Actual host systemd/Quadlet installation was not performed |
| Orbit-on-Orbit | Separately built pinned baseline server/workers produced a README-only patch; independent candidate formatting/regular Rust tests passed; source clone unchanged; final rerun passed in 113.74s |

The 47 ignored cases were qualified as 46 concurrent regressions plus one separate
pinned-baseline case, not as one all-at-once run. See [test commands](../development/testing.md)
and [dogfood reproduction](../development/dogfooding.md). Container deployments
use local artifacts; the separate S3 regression covers object-store semantics,
not S3 backup/disaster recovery.

The image was built by Podman and loaded locally into Docker for the same-image
deployment drill. No image registry publication occurred. CI YAML parses and uses
pinned actions/read-only permissions; hosted GitHub jobs and Docker's own image
build job have not yet run. Templates are not evidence of a boot-installed service.

## Findings and explicit gaps

- Docker server restart/restore passed, but the actual Docker compute-worker
  lifecycle case failed: its attempt remained RUNNING, the local worker stopped,
  and the attempt container was left Created, not running. The exact labelled
  stopped fixture was subsequently removed. Podman compute execution/cancellation/
  kill recovery passed. Do not advertise Docker compute as qualified on this host.
- The deployment fixture now waits for PostgreSQL TCP readiness, avoiding the
  initialization-only Unix-socket server. The artifact fixture uses the documented
  `data` kind. These fixes did not relax production leases or resource limits.
- Dogfood's independent diff check includes the staged patch (`git diff HEAD`);
  the worker intentionally applies accepted patches to the index. Offline coding
  and agent examples are not live model/provider qualification.
- `.agents` is protected read-only in this environment. Skill sources are in
  `skills/`, validated but not automatically installed. See [onboarding](../development/agents.md).
  A genuinely fresh agent session has not been qualified here.
- Physical GPU execution, hostile-agent isolation, HA/performance/retention,
  rolling upgrades, SSO/cloud vaults, cross-browser/accessibility certification and
  public distribution remain outside this increment. No cloud infrastructure is
  invented without a selected hosting requirement.

## Local evidence and acceptance

Raw evidence is retained in `target/qualification-alpha`, the final pinned dogfood
run in `target/qualification-alpha-final`, the failed Docker
compute fixture in `target/qualification-alpha-docker`, and private deployment
drills/backups in `target/deployment-smoke`. Drill containers/anonymous database
volumes were removed; fixture files/backups are retained. The repository's
disposable PostgreSQL/MinIO services and cached images remain available.

`target/qualification-alpha-review` is a separate redacted export. Its initial
1,049 files (2,776,155 bytes) and 141 accepted artifact references were independently
hash/size-checked. Known fixture credentials and structured lease/operator token
fields were absent. The manifest intentionally retains `review_required: true`.
Review arbitrary command arguments/artifact content before sharing: hashes and
structured redaction do not certify that an export is public-safe.

The final dogfood export is `target/qualification-alpha-final-review`: all 10
files (32,267 bytes) were independently hash/size-checked, including corrected
15-second baseline lease metadata. The actual README-only patch, accepted
manifest, test-report exit codes and command arguments were inspected locally.
This supersedes the earlier dogfood record's fixture-default lease metadata;
execution behavior was unchanged. Retained raw fixtures/builds occupy about
9.6 GiB across the two alpha evidence roots. They are disposable local data,
not files to commit or distribute; no automatic recursive cleanup was performed.

The alpha still needs project-owner review/acceptance of the evidence and chosen
operating boundary. No public deployment, paid provider call, repository push,
package publication or acceptance decision was performed by this work.
