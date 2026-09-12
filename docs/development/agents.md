# Agent onboarding

AGENTS.md is the repository entry point: it links the current architecture,
priorities and tests. Start there; no previous conversation or full vision read
is required for ordinary implementation. Follow the reference relevant to the
requested behavior and inspect the owning module/tests before editing.

Two small workflow skills are authored in [skills/](../../skills/):

- [orbit-development](../../skills/orbit-development/SKILL.md)
- [orbit-qualification](../../skills/orbit-qualification/SKILL.md)

They deliberately reference canonical docs and scripts rather than copy them.
Their names/descriptions identify when they apply; essential invariants remain
in AGENTS.md and are not conditional on skill selection.

Codex discovers repository skills under `.agents/skills`. The development
environment used for this increment protects `.agents` as read-only even after
a scoped write approval, so automatic installation was not performed. In a normal
writable checkout, create `.agents/skills`, then link each skill directory, e.g.
`ln -s ../../skills/orbit-development .agents/skills/orbit-development` and the
equivalent for `orbit-qualification`. Do not overwrite an existing installation.
Alternatively invoke the source skill by its explicit path. Keep `.codex/` local.

See official [repository instructions](https://learn.chatgpt.com/docs/agent-configuration/agents-md)
and [skill discovery](https://learn.chatgpt.com/docs/build-skills). Test onboarding
by starting a fresh session and asking it to locate a contract, the relevant
test command and the current unverified gates; it should not need milestone history.
