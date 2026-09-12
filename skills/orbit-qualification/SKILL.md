---
name: orbit-qualification
description: Run and review Orbit's disposable database, worker, OCI, browser or deployment qualification and its evidence without touching live systems.
---

Read docs/development/testing.md for prerequisites and docs/operations/qualification.md
for the current evidence map. Pick the smallest relevant case, then the complete
suite when the change affects shared execution semantics. scripts/qualify.sh
does not provision services or pull images implicitly.

Resolve the database, runtime, workspace and evidence paths before running a case.
Use disposable fixtures; deployment smoke tests and backup restore drills must
not reuse a live installation. Keep worker runtime access separate from the API.
Do not weaken leases, deadlines or resource bounds to obtain a passing result.

Export evidence into a new destination using orbit export-evidence. Check its
manifest and accepted artifacts, then review raw artifact content and command
arguments for secrets before any sharing. A passing suite or checksum review is
not owner acceptance. Report failed/skipped cases, backend differences and retained
local resources. Use docs/development/dogfooding.md for pinned Orbit-on-Orbit work.
