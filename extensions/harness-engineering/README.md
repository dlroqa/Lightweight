# harness-engineering extension

Adds harness-engineering practice to a Lightagent run as five on-demand
skills. It contributes no tools, MCP servers or persona instructions: the model
sees one catalog line per skill and loads a skill's body with `skill.read`
only when a task calls for it, so the bundle costs little context until used.

| Skill | Use it to |
|---|---|
| `harness-plan` | write or update a `PLAN.md` with milestones, verification gates and scope |
| `harness-log` | keep an append-only `IMPLEMENT.md` of decisions and deviations |
| `harness-agents-md` | write or review an `AGENTS.md` for a repository |
| `harness-review` | review a harness, tool or `AGENTS.md` against the checklist |
| `harness-resources` | find curated reading on harness design by topic |

## Install

An extension is a directory; installing is copying it into place.

```bash
cp -r extensions/harness-engineering ~/.lightagent/extensions/
lightagent extensions list          # shows harness-engineering as active
lightagent extensions disable harness-engineering   # switch it off
```

It loads into `lightagent chat` (the terminal UI, where `/skills` lists it),
`lightagent serve` and `lightagent acp`. Extensions must be enabled
(`extensions.enabled`, on by default).

## Source and licence

The templates (`AGENTS.md`, `PLAN.md`, `IMPLEMENT.md`, `HARNESS_CHECKLIST.md`)
and the reading index are adapted from
[ai-boost/awesome-harness-engineering](https://github.com/ai-boost/awesome-harness-engineering)
at commit `6015473ad287575fc06d0ddd7835306250a66b9f`, which is dedicated to the
public domain under CC0 1.0. The index is a selection, not the whole list; the
upstream README has every entry.
