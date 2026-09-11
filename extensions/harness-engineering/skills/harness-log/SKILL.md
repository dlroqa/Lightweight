---
name: harness-log
description: Keep an append-only IMPLEMENT.md log of decisions, deviations and open questions while carrying out a plan.
---
# Implementation log: IMPLEMENT.md

The log is the record of *why* the work ended up the way it did. It is
append-only: add entries, never edit past ones.

## Steps

1. Read the task from `PLAN.md` if one exists; if there is no plan for a
   non-trivial task, write one first (the `harness-plan` skill).
2. Create `IMPLEMENT.md` from the template below if it does not exist yet.
3. Add a log entry whenever you make a real choice, find something that
   changes the plan, or stop for the day. Newest entries go at the top of
   the Log section.
4. When you deviate from the plan, add a row to the deviations table and
   update `PLAN.md` to match, so the two never disagree.
5. Move an open question to the resolved table once it is answered.

Use `fs.read` and `fs.write` when available; otherwise give the entry text in
your answer for the user to paste.

## Template

```markdown
# IMPLEMENT.md

## Task reference
Link to or copy the task from PLAN.md.

---

## Log

### YYYY-MM-DD HH:MM — <brief title>
**What happened:** what was done, found or decided.
**Decision:** what was chosen and why; what was rejected and why.
**Deviation from plan:** if any, describe it and update PLAN.md.
**Next:** what comes immediately next.

---

## Deviations summary
| Deviation | Reason | Plan updated? |
|---|---|---|

## Open questions (unresolved)
- [ ]

## Open questions (resolved)
| Question | Answer | Date |
|---|---|---|
```
