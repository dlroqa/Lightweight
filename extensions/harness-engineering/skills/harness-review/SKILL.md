---
name: harness-review
description: Review an agent harness, tool definition or AGENTS.md against the harness-engineering checklist before shipping or handing it off.
---
# Harness review checklist

A failing item is a blocker; a skipped item needs a written reason. Review
against evidence: read the files, schemas and commands involved (`fs.read`,
`fs.list`) and, where allowed, run the verification commands
(`terminal.run`). Never mark an item passing on assumption.

## How to report

Go section by section. For each item give pass, fail or skipped, with one
line of evidence (a file and line, a command and its result). List the
blockers first, then the rest. End with the removal table: harness
components exist because the model cannot do something yet, so note what
would make each one unnecessary.

## Checklist

### Agent instructions (AGENTS.md)
- [ ] Project overview is accurate and up to date
- [ ] Repository structure reflects the current layout
- [ ] Tool permissions are explicit: allowed, restricted and not allowed
- [ ] Verification gates are defined and their commands are correct
- [ ] No instruction can be read more than one way

### Tool design
- [ ] Each tool has a clear, unambiguous name
- [ ] Schemas are minimal: no optional fields the agent won't use
- [ ] Error messages say what to do next, not only what went wrong
- [ ] Return values have the same shape on success and failure
- [ ] No tool does more than one conceptual thing

### Context delivery
- [ ] Context is scoped to the task, not the whole codebase
- [ ] Long-lived state (plans, decisions, progress) lives in files, not the prompt
- [ ] A compaction strategy exists for multi-session tasks
- [ ] No secrets or credentials in agent-accessible context

### Planning artifacts
- [ ] PLAN.md exists for non-trivial tasks
- [ ] Milestones have explicit verification commands
- [ ] In-scope and out-of-scope are written down
- [ ] IMPLEMENT.md records decisions and deviations as they happen

### Permissions and sandbox
- [ ] The agent runs with the minimum permissions the task needs
- [ ] Destructive operations require explicit confirmation
- [ ] Network access is scoped where possible
- [ ] File system access is scoped to project directories

### Verification loop
- [ ] Tests exist for the agent's outputs
- [ ] The agent can run the verification itself, not only "human review"
- [ ] Verification runs on task completion, not only on PR
- [ ] Eval criteria were written before the task started

### When each component can be removed
| Component | Exists because | Can be removed when |
|---|---|---|
