@AGENTS.md

# Claude Code

Skills in `.claude/skills/` wrap the playbooks above: `parity-check`, `port-upstream-commit`, `node-compat-gap`, `ci-triage`, and `ship-release`.

Subagents in `.claude/agents/`:

- `parity-auditor` runs the parity report and confirms each finding. It reads only.
- `upstream-porter` ports one upstream commit with tests.
- `ci-triager` finds the cause of a CI failure and proposes the fix.

Give a subagent one task with exact files in scope. Run at most one writing agent per crate at a time.
