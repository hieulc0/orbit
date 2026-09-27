## Coding conventions

Write code for future maintainers, not for the implementation plan that produced it.

- Name functions, variables, types, modules, and constants from domain meaning, responsibility, or behavior.
- Do not encode task, phase, step, requirement, or plan numbering into normal production identifiers. Avoid names such as `s8_*`, `r4_s8_*`, `step_4_*`, `phase_b1_*`, `fix_2_*`, `S8_*` or similar temporary planning labels.
- Do not leave comments such as `Phase B1.3`, `Step 8`, `Requirement R4-S8`, or `implement plan item 4` in normal production code.
- Requirement or milestone identifiers are acceptable only where traceability is intentional, such as qualification tests, evidence mapping, compatibility notes, migrations, or dedicated requirements documentation.
- Comments should explain invariants, rationale, safety constraints, compatibility behavior, or other non-obvious design decisions. Prefer explaining why over narrating what the code does.
- Match the naming and documentation conventions already used in the surrounding code instead of introducing a new local convention.
- Remove temporary helpers, scaffolding, debug paths, and implementation-plan terminology before handoff. Before handoff, review the final diff as if the implementation plan and agent conversation did not exist. The resulting code should be understandable on its own.

Planning vocabulary must not become implementation vocabulary.
