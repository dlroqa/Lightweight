---
name: harness-plan
description: Create or update a PLAN.md (milestones with verification gates, scope, risks) before a non-trivial multi-step task.
---
# Planning artifact: PLAN.md

A plan kept in a file survives the end of a conversation and a full context
window; a plan kept only in your head does not. Write one before any task with
more than a couple of steps, and keep it current while you work.

## Steps

1. If a `PLAN.md` already exists, read it first (`fs.read`) and update it
   rather than starting over.
2. Fill the template below. Keep the task to one sentence. Give every
   milestone a concrete verification: a command to run or a check to make.
3. Write it into the workspace with `fs.write` when that tool is available;
   otherwise put the finished plan in your answer so the user can save it.
4. While working: mark a milestone `[x]` only after its verification passes,
   and append decisions to Notes (never rewrite earlier notes). Record
   deviations in `IMPLEMENT.md` too (see the `harness-log` skill).

Ask the user about anything under Open questions that blocks the first
milestone instead of guessing.

## Template

```markdown
# PLAN.md

## Task
One sentence: what is being built or fixed.

## Context
Why this task exists, what triggered it, what success looks like.

## Approach
High-level strategy: what will change and why; key trade-offs considered.

## Milestones
Mark `[x]` only when the verification passes.
- [ ] **M1: <name>** — <what done looks like> | verify: `<command or check>`
- [ ] **M2: <name>** — <what done looks like> | verify: `<command or check>`
- [ ] **Final: all tests pass** — verify: `<test command>`

## Scope boundaries
In scope:
-
Out of scope (explicitly excluded):
-

## Open questions
- [ ] Question — where/how to resolve it

## Risks
What could go wrong; what assumptions the plan depends on.

## Notes
Append-only log of significant decisions made during execution.

---
*Created: YYYY-MM-DD*
```
