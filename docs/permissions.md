# Tool permissions

Lightagent keeps tool availability separate from authorization. Enabled tools
can be listed with `lightagent tools list`, which shows each tool's effective
permission for the active profile:

| Permission | Behavior | Typical tools |
| --- | --- | --- |
| `auto` | Runs without interruption | `fs.read`, `fs.list`, `web.search`, `web.fetch` |
| `ask` | Pauses for an explicit decision on that call | `fs.write`, `terminal.run`, mutating API/MCP tools |
| `block` | Refused without prompting | Privileged tools and denied terminal programs |

The profile's balanced policy auto-approves read-only and read-only network
tools. Sensitive reads may ask. Any tool classified as mutating or executable,
or requiring `fs:write` or `terminal:exec`, asks on **every** call. A remembered
grant, permissive profile, or session-wide relaxation cannot bypass that rule.
Privileged calls always block. Unknown MCP tools are treated as executable
unless their server explicitly marks them read-only; inspect third-party tool
manifests before enabling a server.

`fs.read`, `fs.list`, and `fs.write` resolve paths within the configured
workspace root. Absolute paths, `..` escapes, and writes through a symlink leaf
are refused. `terminal.run` starts one program directly, without a shell, with
its working directory inside that root. It requires `tools.allow_terminal` and
fresh approval before execution. Obvious destructive programs such as `rm`,
`dd`, `mkfs`, and shell wrappers are blocked by a deny-list before prompting.
The working directory and deny-list do **not** sandbox an approved process: a
program can still access files permitted to the Lightagent process. Review the
program and arguments in each approval request; use `tools.terminal_allowlist`
to narrow which programs may run.

Approval decisions are tied to one pending request and its exact tool call.
Invoking a mutating or executable tool without that matching decision fails at
the executor boundary. A denied request never starts its tool. The terminal
UI shows a bounded, redacted argument preview and defaults to not allowing a
pending action.
