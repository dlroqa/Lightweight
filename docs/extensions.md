# Onboarding with installable tools

Lightagent starts with its built-in tools. `lightagent tools list` reports the
tools the active profile can actually call, including tools discovered from
connected MCP servers. The terminal startup dashboard and `/tools` use the same
registry. Use `/skills` to see loaded skills.

An extension is a directory with an `extension.json` manifest. It can add
instructions, skill directories, and MCP servers. The server's own `tools/list`
response supplies each callable tool's name, description, and input schema;
Lightagent validates calls against that schema and applies its normal approval
policy. There is no need to add a Rust implementation for each new MCP tool.

To read a Markdown file for one chat turn, drop its path into the terminal
prompt and press Enter. Lightagent accepts quoted paths, escaped spaces, and
local `file://` URLs. The file must be UTF-8 and at most 64 KiB. Its contents
are sent as that turn's user message; the file is not installed.

To keep an onboarding file as profile guidance, type `/onboard ` and then drop
the `.md` file into the prompt. Lightagent installs it as the
`user-onboarding` profile extension and reloads the runtime. Repeating the
command replaces that bundle; `/onboard remove` removes it. You can also build
an onboarding extension yourself: put the file in a small extension and point
`instructions_file` at it:

```text
my-onboarding/
  extension.json
  ONBOARDING.md
```

```json
{
  "name": "my-onboarding",
  "description": "My personal onboarding guidance",
  "instructions_file": "ONBOARDING.md"
}
```

Install it with `lightagent extensions install ./my-onboarding --profile`
after selecting the intended profile. The Markdown then joins that profile's
system instructions at startup and on each new API/ACP run. `/reload` picks up
edits to the installed bundle in an open TUI. Disable or uninstall the
extension to withdraw the guidance. Keep this file focused on durable
preferences and how to choose tools; procedures needed only for certain tasks
fit better as `SKILL.md` files in the same bundle.

```text
my-tool/
  extension.json
  server.py
  skills/
    my-workflow/
      SKILL.md
```

Example `extension.json` for a local server:

```json
{
  "name": "my-tool",
  "version": "1.0.0",
  "description": "A short description for the extension list",
  "mcp_servers": [
    {
      "transport": "stdio",
      "name": "my-tool",
      "command": "python3",
      "args": ["server.py"]
    }
  ]
}
```

The server starts with the installed extension directory as its working
directory. It must implement MCP `initialize`, `tools/list`, and `tools/call`.
Tools appear as `mcp.my-tool.<tool-name>`. A server reachable over streamable
HTTP can instead use `{"transport":"http","name":"my-tool","url":"https://…"}`.
Do not put API keys in the manifest; use the existing environment or secret
reference mechanisms for the server you install.

Install a reviewed local directory, then verify that its tools connect:

```sh
lightagent extensions install ./my-tool
lightagent extensions show my-tool
lightagent tools list
lightagent
```

If the TUI is already open, type `/reload` between turns. `/tools` then shows
the refreshed list, and the next model turn can call those tools. A server that
fails to connect is reported on stderr and its tools are withheld from the
model. `lightagent extensions disable my-tool` keeps the files installed but
withdraws their contributions after `/reload`; `enable` restores them.

The HTTP API and ACP resolve extensions for each new run, and the API's tools
endpoint reports the active profile's current registry.

You can also run `/extensions install <directory>` or
`/extensions uninstall <name>` in the TUI; those commands reload the registry
automatically. `/extensions` lists installed bundles.

Install globally by default, or add `--profile` to install only for the active
profile. The profile's copy overrides a global extension with the same name.
Uninstall from the same scope you installed into:

```sh
lightagent extensions uninstall my-tool
lightagent extensions uninstall my-tool --profile
```

The `extensions install` command accepts a local directory or an HTTPS Git
repository. Review the repository first. For a reproducible install, check out
a pinned revision locally and install that directory. A `#subdir` fragment
chooses an extension below the repository root:

```sh
lightagent extensions install 'https://example.org/tools.git#extensions/my-tool'
```

The CLI makes a shallow checkout, copies only the selected extension into its
managed store, and discards the temporary checkout. It excludes `.git` and
rejects package symlinks, keeping the installed bundle self-contained and
removable without changing the source repository.

For personal onboarding, set the active profile's model and workspace, enable
only the built-in tool categories you need in `lightagent setup`, and put
workflow guidance in that profile's persona or `skills/` directory. Keep the
onboarding Markdown as a skill when it gives the agent a reusable procedure;
put only always-relevant user preferences in the profile persona. Install a
new MCP extension when a concrete task needs a capability the current
`lightagent tools list` does not offer, verify the tool appears, and remove it
when it is no longer useful.
