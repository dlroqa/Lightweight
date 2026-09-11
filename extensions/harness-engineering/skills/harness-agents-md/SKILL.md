---
name: harness-agents-md
description: Write or review an AGENTS.md that gives coding agents a project overview, conventions, explicit tool permissions and verification gates.
---
# Project instructions for agents: AGENTS.md

`AGENTS.md` sits at the repository root and is what an agent reads before
touching anything. Agents do better with explicit boundaries than with vague
cautions, so every section should be concrete and checkable.

## Steps

1. Look before you write: list the repository (`fs.list`), and read the
   README, build files and any existing `AGENTS.md` or `CLAUDE.md`
   (`fs.read`). Do not invent commands; take them from the build files.
2. Fill the template below. Replace every placeholder; delete a section
   rather than leave a placeholder in it.
3. Make permissions three explicit lists: allowed, ask first, not allowed.
4. Give verification gates as exact commands the agent can run itself.
5. Write the file with `fs.write` when available, or return it in your
   answer. When reviewing an existing file, report gaps against the
   `harness-review` checklist instead of rewriting it wholesale.

## Template

```markdown
# AGENTS.md

## Project overview
One paragraph: what this project does, its stack, its primary goals.

## Repository structure
    src/      # application code
    tests/    # test suite

## Conventions
### Code style
Language, formatter, linter, project-specific rules.
### Naming
Files, functions, variables.
### Testing
    # Run all tests
    <command>
    # Run one test file
    <command>
### Commits
Message format, branching, PR conventions.

## Tool permissions
Allowed:
- Read and edit files under `src/`, `tests/`, `docs/`
- Run `<test command>` and `<lint/format command>`
Restricted (ask before proceeding):
- Modifying `<critical config files>`
- Destructive commands (`rm -rf`, database drops, …)
- Pushing to `main` or creating releases
Not allowed:
- Changing CI/CD configuration without explicit instruction
- Installing new dependencies without explicit instruction

## Known constraints
Anything that would surprise an agent here for the first time.

## Verification gates
Before marking any task complete:
- [ ] Tests pass (`<command>`)
- [ ] Linter passes (`<command>`)
- [ ] No new warnings introduced
- [ ] Changed files are within the permitted scope above

## Contact / escalation
If a decision falls outside the permitted scope, stop and describe the
blocker clearly rather than assuming.
```
