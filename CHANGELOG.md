# Changelog

All notable changes to this project are documented in this file.

## [Unreleased]

## [0.3.20] - 2026-09-12

A patch release that hardens the panel's Agent screen against a slow or
outdated local server. When the agent server is stopped, the screen now starts
it and retries on its own instead of waiting for a click, showing a *starting*
status while it comes up; the manual *Start agent server and retry* action
remains. A gateway that answers a health check but does not yet expose the
agent API is verified before use rather than misread as ready, and a run whose
event stream drops mid-flight is reconciled against the run endpoint so a still
working model is no longer shown as idle. A single status badge reports the
service and run state — ready, starting, running, done, offline, failed, or
stopped — and the
non-JSON API error now names the status, content type, and URL so an outdated
or misrouted gateway is diagnosable. Existing sessions, runs, and settings keep
working unchanged.

### Changed

- **The Agent screen recovers a stopped server automatically.** On open, a
  stopped local agent server that can be started is started and retried without
  a click, with a *starting* status and the message box disabled until the API
  answers; the *Start agent server and retry* button stays as a fallback. A
  status badge reports the service and run state (ready, starting, running,
  done, offline, failed, stopped).

### Fixed

- **A dropped event stream no longer looks like a finished run.** A transient
  SSE error reconciles against the run endpoint instead of marking the run
  done, so a still working model keeps streaming; the transcript closes only on
  a real terminal state.
- **A running-but-outdated gateway is caught before use.** After the server
  reports healthy, the Agent screen confirms the agent API responds before
  proceeding, and the non-JSON API error names the response status, content
  type, and URL.

## [0.3.19] - 2026-09-12

A patch release for the Lightagent panel and interactive terminal. The panel's
Agent screen now works with persisted conversations directly: past agent
sessions can be searched, resumed, and deleted, each with its saved transcript
and tool history, and a message sent while a run is in flight queues as a
visible FIFO steer rather than being dropped. When the local agent server is
stopped, the screen can start it and retry, and a dedicated Lightagent settings
panel edits the CLI/TUI configuration used by new runs. In the terminal, a
prompt submitted while Lightagent is working no longer interrupts it: the
message is queued behind the active run, shown with its place in line, and
becomes an ordinary turn once the current work finishes. Existing runs and
settings keep working unchanged.

### Added

- **Agent sessions in the panel.** Search, resume and delete persisted agent
  conversations, with saved transcripts and tool history. Mid-run messages
  queue as visible FIFO steers. The Agent screen can start a stopped local
  agent server and retry; dedicated Lightagent settings edit the CLI/TUI
  configuration used by new runs.

- **Interactive turns can be steered without interruption.** Type and submit a
  message while Lightagent is working to queue it behind the active run.
  Queued messages are visibly numbered and become ordinary turns in arrival
  order after the current work finishes; the prompt tip advertises `↵ steer`.

## [0.3.18] - 2026-09-12

A patch release for the Lightagent interactive terminal. Tools can now be added
without rebuilding: an extension directory is installed by the CLI or from an
open chat, and its MCP server runs from its own installed directory so
relative scripts and assets resolve. A dropped Markdown file can be read for a
single turn or kept as profile guidance. Durable memory retains clearly stated
user facts on its own and can present them as a grouped knowledge page. The
approval boundary is tightened: file mutations and command execution ask on
every call and are checked again at the point of execution, so no policy,
remembered grant, or session-wide relaxation can let one through unattended.
Existing profiles, extensions, and settings keep working; automatic retention
and the new prompts can each be turned off.

### Added

- **Extensions are installable.** `lightagent extensions install <directory>`
  copies a bundle into Lightagent's managed store — staged then renamed, with
  symlinks, path-like names, and malformed MCP entries refused — and
  `lightagent extensions uninstall <name>` removes exactly that copy. `--profile`
  selects the active profile's store instead of the global one, an HTTPS Git URL
  with an optional `#subdir` is fetched into a temporary checkout first, and
  installing a bundle with MCP servers enables MCP. `lightagent extensions show`
  now also lists the skills a bundle contributes.
- **Extensions can be managed from an open chat.** `/extensions`,
  `/extensions install <directory>`, and `/extensions uninstall <name>` update
  the tool registry between turns, and `/reload` picks up changes made in
  another terminal. The startup dashboard gained an Active Extensions section.
- **A manifest can point at a Markdown instructions file.** `instructions_file`
  in `extension.json` loads a bounded Markdown file from inside the bundle,
  keeping long guidance out of the JSON.
- **Dropped Markdown files.** Dropping a `.md` path into the prompt reads that
  file for the turn — quoted paths, escaped spaces, and local `file://` URLs are
  accepted, bounded to 64 KiB. Typing `/onboard ` before the drop installs it as
  the profile's `user-onboarding` guidance instead, replacing any previous copy;
  `/onboard remove` withdraws it.
- **Durable facts are retained automatically.** Clear statements of preference,
  decision, codebase convention, or resolution are saved for the active profile
  with the session and message they came from, so a new session starts with
  established context. The filter is deterministic and conservative: questions,
  temporary requests, code blocks, and likely secrets are skipped, and
  `lightagent config set memory.auto_capture false` turns it off without
  deleting anything. `memory.inject_recent` and `memory.top_k` are now settable
  from `lightagent config` as well.
- **Memory reflection.** `lightagent memory reflect [topic]` and the read-only
  `memory.reflect` tool build a knowledge page grouped into preferences,
  decisions, conventions, resolved issues, and other facts, newest first with
  source citations. The page is derived on demand, so a correction or a
  `forget` changes it immediately.
- **Effective permissions are visible.** `lightagent tools list` and the chat
  `/tools` listing show each tool's resolved `auto`, `ask`, or `block`
  permission for the active profile alongside its risk class.

### Changed

- **Mutations and command execution always ask.** Any call classified as
  mutating or executable, or carrying an `fs:write` or `terminal:exec` scope,
  requires a fresh decision on every call; a remembered grant, a permissive
  profile, or the session-wide relaxation can no longer cover one, and such a
  grant is no longer recorded. Privileged calls are refused outright. The third
  approval choice is now labelled "Allow this call; relax lower-risk tools this
  session" to describe what it actually does.
- **Approvals are enforced at the execution boundary.** A decision is bound to
  one pending request and its exact tool call: resuming a paused run with a
  decision for a different request is rejected, and the executor re-checks the
  policy when the tool is invoked, so an approval-requiring call cannot run
  without its matching one-time grant.
- **`terminal.run` refuses obviously destructive programs.** `rm`, `dd`,
  `mkfs*`, `sudo`, shells and other wrappers are denied by name before any
  prompt is shown, and over-long argument sets are refused as unreviewable. This
  is a review aid, not a sandbox — an approved program still runs with the
  harness's own access.
- **Approval previews are built for review.** An `fs.write` request shows its
  path, mode, and content size instead of a truncated body, and a `terminal.run`
  request shows redacted arguments.
- **`serve` and `acp` resolve tools and settings per run.** Each run reloads the
  configuration and builds its registry from the resolved profile, so a
  profile's own extensions — not only the global ones — contribute their tools,
  and `GET /api/lightagent/v1/tools` reports the registry the server would
  actually use rather than the built-in set.

### Fixed

- **A leaf symlink can no longer redirect a workspace write.** `fs.write`
  refuses a path whose final component is a symlink, including a dangling one,
  so a link inside the workspace cannot be used to write outside it.
- **The startup dashboard no longer hides tools.** A tool or permission line
  wider than the panel is wrapped instead of truncated, so late entries in a
  long list stay visible and every line keeps the panel width.

## [0.3.17] - 2026-09-12

A patch release for the Lightagent interactive terminal. Durable memory and
session history grow up together. A long chat now keeps its recent turns
verbatim and packs selected older excerpts and bounded tool-result evidence
into a share of the model's context, while the saved transcript stays complete;
the same packing reaches the ACP editor integration and the HTTP API. Durable
memory recall is query-aware rather than a fixed recent snapshot, remembers
where each fact came from, and can fuse lexical ranking with a semantic
endpoint when one is configured. Runs that pass no session, and profiles with
no configured embeddings endpoint, keep working exactly as before.

### Added

- **Reviewed facts can be promoted from a saved session.** `lightagent memory
  candidates <session>` lists user statements that look durable and `lightagent
  memory promote <session> <message-number>` saves one of them — optionally
  edited with `--text` — recording the session and message it came from.
  `lightagent memory update <id> <text>` corrects an existing fact in place, and
  a memory carries an optional `updated_at` and `source` in its listing.
- **A run can read a cited source.** The read-only `session.lookup` tool
  retrieves an exact saved message or a bounded tool-result excerpt by id from
  the active profile's sessions, so a recalled memory can be traced back to what
  was actually said.
- **Optional semantic recall.** When `rag.semantic` is configured, `memory
  search` and per-request recall fuse the offline lexical ranking with the
  semantic ranking; a failed or absent embeddings endpoint leaves the lexical
  path fully usable.
- **Saved sessions keep tool evidence.** Each recorded tool call now stores its
  call id, a bounded result excerpt, and any file or URL it was given.
  `lightagent sessions show <id>` numbers every message and prints each tool's
  excerpt and source.

### Changed

- **Long chats are packed into the model context instead of replayed whole.**
  Recent turns are kept verbatim; older material is compacted into brief
  excerpts and the most relevant saved tool evidence, bounded to a share of the
  configured context so the system prompt, tools, and answer keep their room.
  The durable transcript is never modified.
- **Durable memory is injected per request, not as a recent snapshot.** Each
  prompt receives up to three memories selected for the current message rather
  than the most recently written ones; memory written during a terminal chat is
  recallable on the next prompt without restarting. Recall now ranks with a
  BM25-style lexical score with a phrase-match bonus.
- **Memory writes are atomic, locked, and validated.** Writing takes a file
  lock and re-reads before it saves, so concurrent writers merge instead of
  clobbering; an identical fact is merged rather than duplicated; and a
  malformed record is reported with its line number instead of being silently
  discarded.

## [0.3.16] - 2026-09-11

A patch release for the Lightagent interactive terminal. Conversations now keep
their history: a follow-up prompt sees the earlier turns of the same session,
transcripts are saved as the chat proceeds, and a closed session can be
reopened later by id. The same session continuity reaches the ACP editor
integration through `session/load`, the HTTP API through explicit session
handles, and the browser Agent screen, which now remembers its conversation
across reloads. Runs that pass no session stay stateless for backwards
compatibility.

### Added

- **Terminal chat threads a conversation across turns.** Follow-up prompts
  include the previous user and assistant turns in the model context, and the
  transcript is saved after each prompt. `lightagent chat --session <id>`
  reopens a saved session (find its id with `lightagent sessions`), and `/new`
  starts a separate conversation with an empty context, resetting session-only
  approval choices. An explicit "allow without restrictions" approval choice is
  remembered for the resumed session.
- **The ACP integration restores sessions with `session/load`.** The agent now
  advertises the `loadSession` capability, replays a persisted transcript back
  to the editor on reconnect, and threads completed turns through the containing
  session; each prompt remains a distinct managed run.
- **The HTTP API exposes session handles.** `POST
  /api/lightagent/v1/sessions` creates a session whose `id` can be passed as
  `session_id` to `POST /api/lightagent/v1/runs`; such a run restores the prior
  turns and persists the new one, while a second run against a busy session is
  rejected. Runs without a `session_id` stay stateless.
- **The browser Agent screen keeps its conversation.** It retains the session
  across reloads and adds a New session button to start a fresh one.

## [0.3.15] - 2026-09-11

A patch release for the Lightagent interactive terminal. Isolated user profiles
can now be created, switched, and deleted end to end — from a new Profiles
screen in `lightagent setup` and from a singular `lightagent profile` command
for scripts. The `default` profile is retained as the protected main account,
deleting a profile asks for confirmation, and the previous plural `lightagent
profiles` spelling stays available as an alias.

### Added

- **Profiles can be managed from terminal setup.** The new Profiles screen in
  `lightagent setup` can create, activate, and delete isolated user profiles,
  with confirmation before deleting a profile and `default` retained as the
  protected main account. It is also available directly through `lightagent
  setup profiles`.
- **The singular `lightagent profile` command manages profiles from scripts.**
  `profile create` (also `add`), `profile use <name>`, and `profile delete`
  complement list/show operations. The previous plural `lightagent profiles`
  spelling remains as a compatible alias.

## [0.3.14] - 2026-09-11

A patch release for the Lightagent interactive terminal. The startup
dashboard's pixel-art logo now renders from a hand-crafted terminal-native
asset that stays crisp instead of carrying a dark antialiased halo, and the
approval prompt's numbered choices use action-oriented labels. Redirected and
non-interactive I/O is unchanged.

### Changed

- **The terminal logo renders from a terminal-native pixel-art asset.** The
  startup dashboard now embeds a hand-crafted 56×56 RGBA mark with binary
  transparency and a compact, non-dithered palette. Nearest-neighbour sampling
  maps every visible source pixel directly to one terminal pixel, so the gold
  star, cyan lightning and block-pixel `Lightagent` wordmark keep crisp edges
  instead of the dark antialiased halo left by the previous downsampled
  derivative. The matching monochrome fallback remains available.

- **Approval choices use action-oriented labels.** The numbered warning prompt
  now displays `Allow`, `Don't allow`, and `Allow without restrictions`; option
  2 remains the safe default and the underlying approval behavior is unchanged.

## [0.3.13] - 2026-09-11

A patch release for the Lightagent interactive terminal. The startup dashboard
now renders the latest supplied pixel-art logo — the gold star, cyan lightning
and block-pixel `Lightagent` wordmark — completed agent answers are explicitly
labelled `Lightagent`, the initialization notice appears only before the first
model request, and the status row's background and separator end with the
elapsed-time value instead of filling unused terminal columns. Redirected and
non-interactive I/O is unchanged.

### Changed

- **The pixel-art logo replaces the modern mark as the terminal asset.** The
  startup dashboard renders the pixel-art gold star, cyan lightning and
  block-pixel `Lightagent` wordmark from an embedded true-colour RGBA
  derivative, with a matching monochrome fallback.

- **Agent answers are labelled and the status row is trimmed.** Each completed
  agent answer is explicitly labelled `Lightagent` and boxed to its longest
  rendered line, or to the label when that is longer. The initialization notice
  appears only before the first model request, and the status row's background
  and separator stop after the elapsed-time segment rather than filling unused
  terminal columns.

## [0.3.12] - 2026-09-11

A patch release for the Lightagent interactive terminal. The startup dashboard
now renders the supplied modern logo — the faceted gold star, cyan lightning
and rounded `Lightagent` wordmark — from an embedded true-colour RGBA
derivative, and the interactive prompt follows terminal content instead of
reserving bottom rows. Redirected and non-interactive I/O is unchanged.

### Added

- **The supplied modern logo is now the terminal asset.** The startup dashboard
  renders the faceted gold star, cyan lightning and updated rounded
  `Lightagent` wordmark from an embedded true-colour RGBA derivative, with a
  matching monochrome fallback.

### Changed

- **Prompts now follow terminal content.** The first prompt bar sits directly
  below the startup dashboard and later prompts move down after each response,
  allowing the terminal to scroll naturally instead of reserving bottom rows.
  Each submitted prompt has a compact border matching its rendered text width;
  each completed agent answer is boxed to its longest rendered line and expands
  only when terminal-width wrapping requires it.

## [0.3.11] - 2026-09-11

A patch release for the Lightagent interactive terminal. The streamed reasoning
panel can now be hidden behind an animated gold-and-cyan Lightagent star through
a new Terminal UI setup section and the `tui.show_reasoning` config key, tool
approvals gain a session-scoped "allow without restrictions" choice in a compact
numbered prompt, and the startup banner adopts the gold-and-electric-blue mark
with its welcome message pinned above the footer. The setting defaults preserve
existing behavior, and redirected and non-interactive I/O is unchanged.

### Added

- **Reasoning visibility in Terminal UI settings.** The new `Terminal UI`
  setup screen can show the streamed reasoning panel or hide it behind an
  animated gold-and-cyan Lightagent star. The persisted
  `tui.show_reasoning` setting defaults to `true` for compatibility and affects
  attended CLI rendering only.
- **A session-scoped "allow without restrictions" approval choice.** The
  interactive approval prompt is now a compact numbered box offering allow once,
  deny, or allow without further restrictions for the remainder of the session.
  The session grant relaxes only the in-memory policy and is never persisted to
  a profile or `config.json`.

### Changed

- **The startup banner follows the gold-and-electric-blue mark.** The banner's
  former purple sparkle pixels now render as the electric-blue glow of the
  modern Lightagent star/bolt mark, and the welcome message sits directly above
  the pinned footer instead of being stranded above an empty prompt area.

## [0.3.10] - 2026-09-10

A patch release for the Lightagent interactive terminal. The chat prompt is now
a persistent four-row footer pinned to the bottom of an attended terminal while
model and tool output scrolls above it, tool approvals appear in their own
high-contrast bordered panel, and realtime RAG is a first-class, truthful choice
in setup with its own `rag.realtime_enabled` config key. The startup dashboard
spans the detected terminal width and `/tools` reports the session's effective
registry. Redirected and non-interactive I/O behaves as before, and existing
profiles and configuration load unchanged.

### Added

- **A persistent terminal prompt footer.** Interactive chat reserves the bottom
  four terminal rows for the full-width status bar, command tips, editable input
  row, and border while streamed model and tool output scrolls above it.
- **High-contrast approval warnings.** Tool approvals now render in their own
  full-width amber/red bordered panel with the risk class, tool name, wrapped
  argument preview, and the existing safe-default `[y/N]` decision.
- **Realtime RAG in the Tools picker.** `rag.realtime` has its own truthful
  setup toggle and `rag.realtime_enabled` config key; selecting it also enables
  its web search/fetch dependency.

### Changed

- **The startup dashboard now spans the detected terminal width.** Its former
  180-column ceiling and the divider between the modern Lightagent mark and
  live harness information are removed; narrow terminals use a stacked layout.
- **`/tools` reports the session's effective registry.** Its output now matches
  the tools shown at startup, including realtime RAG, memory, MCP, and extension
  tools that were actually loaded.

## [0.3.9] - 2026-09-10

A patch release for the Lightagent harness and the gateway it drives.
OpenAI-compatible chat and text-completion requests may now name
`model: "default"` (or omit the model), which the gateway resolves to its one
resident model at request time and keeps in step across model swaps, while
responses still report the truthful model ID. A headless end-to-end test now
drives a full streamed tool-using run through the real agent API and panel
gateway. Requests that name a model explicitly are unaffected.

### Changed

- **The loaded gateway model is the dynamic default.** OpenAI chat and text
  completion requests may use `model: "default"`; the alias resolves to the
  one resident model at request time and follows model swaps without changing
  the truthful model ID returned in responses. Authenticated regression tests
  cover streamed tool-call deltas and a complete Lightagent tool run over SSE;
  `tool.requested` now reaches live subscribers as well as the buffered run log.

## [0.3.8] - 2026-09-10

A patch release for the Lightagent harness. A run that reaches its time budget
no longer discards tool results the model has not read yet, the `agent` limits
in `config.json` now take effect, and a harness-engineering skills extension
ships in the repository. Existing profiles and configuration load unchanged,
except that `agent.wall_clock_secs` set to `0` is now rejected as invalid.

### Added

- **A `harness-engineering` extension ships in `extensions/`.** Five
  on-demand skills adapted from the CC0 ai-boost/awesome-harness-engineering
  list: `harness-plan` (PLAN.md), `harness-log` (IMPLEMENT.md),
  `harness-agents-md` (AGENTS.md), `harness-review` (the harness checklist) and
  `harness-resources` (a curated reading index by topic). It contributes no
  tools, MCP servers or persona text, so existing tools are untouched; install
  it by copying the directory into `~/.lightagent/extensions/`, and switch it
  off with `lightagent extensions disable harness-engineering`.
- **`/skills` in the terminal chat** lists the skills the session loaded,
  extension skills included.

### Changed

- **Running out of time no longer throws away fetched work.** The wall-clock
  budget is still checked between turns, but a run that reaches it with tool
  results the model has not read yet now takes one final turn with tools
  withheld and answers from what it gathered, instead of ending with no answer.
  The interactive chat pauses there instead and asks whether to continue: yes
  runs for another budget from exactly where it stopped, `a` answers now from
  what it has, and no keeps the paused run so typing `continue` (or
  `/continue`) later picks it up. A new message drops the paused run.

### Fixed

- **The `agent` limits in `config.json` now apply.** `agent.max_turns`,
  `agent.max_tool_calls` and `agent.wall_clock_secs` were validated but never
  used; they now fill any run limit a profile leaves at its default, and can be
  read and written with `lightagent config get/set`. A zero time budget is
  rejected (`agent.wall_clock_secs` empty or `none` means no time limit).

## [0.3.7] - 2026-09-10

A patch release for Lightagent retrieval. It adds one-call realtime RAG tuned
for quantized local models and improves indexed sparse retrieval with BM25.
Existing web, RAG, provider, and profile configuration remains compatible.

### Added

- **One-call realtime RAG for quantized models.** The new `rag.realtime` tool
  searches the configured live backend, fetches candidate pages concurrently
  through the existing redirect and SSRF guards, chunks on natural boundaries,
  ranks with BM25 and an optional bounded semantic pass, suppresses duplicate
  passages, and returns compact citation-ready evidence. This removes the need
  for a small local model to orchestrate a multi-turn search/fetch pipeline.

### Changed

- **Sparse RAG ranking now uses BM25.** Indexed-document retrieval discounts
  corpus-wide words and saturates repetition before reciprocal-rank fusion with
  semantic results, improving exact-term precision without another model call.

## [0.3.6] - 2026-09-09

A patch release focused on the Lightagent terminal harness. It adds a direct
self-update path, a more informative streaming chat interface, and native
no-account web research. Existing configuration and profiles remain compatible.

### Added

- **A built-in CLI updater.** `lightagent update --check` compares the running
  version with the latest published GitHub release, and `lightagent update`
  installs that exact release tag through Cargo into the current installation
  root. JSON output is available for update checks and
  `LIGHTAGENT_INSTALL_ROOT` supports explicit installation layouts.
- **A live terminal startup dashboard and turn status.** Interactive chat now
  places the Lightagent logo beside the release version and date, active
  profile, resolved model, session, enabled tools, and installed skills. During
  a response it streams provider-supplied reasoning separately from the final
  answer and reports context use, output tokens, token rate, and elapsed time.
- **No-account agentic web research.** `lightagent setup web` can now configure
  DuckDuckGo search without an account or API key, while preserving SearXNG and
  compatible JSON endpoints as an alternative. The native `web.search` and
  `web.fetch` tools are exposed only when configured, fetched page text is
  bounded, and the agent follows a search, evaluate, fetch, verify, refine and
  synthesize loop for requests that need current evidence. Terminal chat and
  the Agent API use the same web capabilities and instructions.

### Changed

- **Tool declarations now match the effective configuration.** Terminal chat
  and the Agent API advertise web, filesystem, terminal, and skill tools only
  when their backing capability is enabled and available. This keeps smaller
  local models from selecting tools that cannot run.

### Fixed

- **The Lightagent package smoke test is independent of a developer's running
  gateway.** Its clean-home `doctor` check now targets reserved port zero, so a
  gateway already serving on the normal local port cannot create a false
  packaging failure.

## [0.3.5] - 2026-09-09

A patch release. Terminal Lightagent gains a guided `setup` command and no
longer needs `init` before a first chat, and it now follows the model loaded in
Lightweight instead of a fixed profile value. Strictly additive: existing
configuration and profiles are reused unchanged.

### Added

- **`lightagent setup`, a guided configuration menu.** One interactive command
  configures the gateway and model, local file and terminal tools, web
  fetch/search, and approval prompts, showing current values and writing
  `config.json` for the user. A real terminal gets a keyboard-driven picker
  (arrows, Space, Enter, Escape); a non-TTY falls back to a numbered stdin/
  stdout prompt. A section opens directly with `lightagent setup provider`
  (alias `gateway`/`model`), `tools`, `web`, or `approvals`. The provider picker
  offers local Lightweight, named custom OpenAI-compatible endpoints, manual
  entry, and removal of saved providers; API keys are stored only as
  environment-variable references, never as secrets.
- **Saved providers.** `inference.saved_providers` records named endpoints for
  quick switching, validated for a non-empty name and an http(s) URL and
  redacted like the other keys.

### Changed

- **Lightagent follows the gateway's loaded model.** Before each generation the
  provider resolves the configured model against the gateway's `/v1/models`: an
  advertised explicit model wins, otherwise the sole resident model is used, so
  switching models in the panel is reflected in terminal chat and the Agent API
  without reconfiguration; when no model is loaded it says so. `init` is now
  optional — a fresh installation uses a built-in default profile at
  `http://127.0.0.1:11434`. The `lightagent` welcome mark is redrawn from the
  source logo.

## [0.3.4] - 2026-09-09

A patch release. The command line is now a single `lightweight` binary rather
than `lightweight` plus a `hermes` twin, and the desktop app and CLI archives
now carry the `lightagent` agent binary alongside it, so starting the agent
from the panel works on a fresh install without a separate `lightagent` on the
host.

### Changed

- **One CLI binary, `lightweight`.** The duplicate `hermes` command (the same
  tool under a second name) is removed; `lightweight` is the inference gateway
  CLI. The command-line archive is renamed accordingly — `lightweight-*-<target
  -triple>.tar.gz` / `.zip` instead of `hermes-*` — and the Linux service and
  environment examples become `lightweight-inference-gateway.service` /
  `.env.example`. Anyone invoking `hermes` should switch to `lightweight`; the
  subcommands are unchanged.

### Fixed

- **The agent binary ships with the app, and the gateway can find it.** The
  desktop installers and the CLI archives now include `lightagent` next to
  `lightweight` (both built, version-checked, and — on macOS — `lipo`-merged per
  binary during staging), so *Settings → Lightagent server → Start server*
  works on a fresh install. The gateway resolves `lightagent` from an explicit
  `LIGHTAGENT_BIN`, then beside its own executable, then from `~/.local/bin` on
  Unix — the documented per-user install location, which a desktop launcher's
  smaller PATH would otherwise miss.

## [0.3.3] - 2026-09-09

A patch release. When Agent Tools cannot connect, the agent server can now be
started from the panel itself — the desktop shell no longer requires a separate
terminal to bring the agent screens to life. Strictly additive: a gateway with
no configurable agent origin, or an agent already running, is left exactly as
before.

### Added

- **Start the Lightagent server from Settings.** A new *Lightagent server* card
  shows the agent server's status (checking, running, stopped, starting, failed
  or not responding) and its address, polling every two seconds, and offers a
  *Start server* button that enables the Agent, Agent Tools and Chat screens
  without leaving the panel. The gateway starts the child at its configured
  `http://127.0.0.1:<port>` or `http://localhost:<port>` upstream, resolving the
  `lightagent` binary beside its own executable, then on `PATH`, with
  `LIGHTAGENT_BIN` as an override. The child inherits the gateway's environment
  (including `LIGHTAGENT_HOME`) and stops when the gateway shuts down; an agent
  already answering is left running. New gateway routes
  `GET /api/v1/agent-server` and `POST /api/v1/agent-server/start` back the
  card, and startup progress and any error the agent returns are surfaced in
  Settings. The README documents the flow, and an end-to-end check exercises the
  start path.

## [0.3.2] - 2026-09-09

A patch release. A gateway that fronts the panel without an agent upstream no
longer answers the agent screens with its own HTML, and when something is
genuinely misconfigured the panel now says what to do about it instead of
surfacing a raw parse error.

### Fixed

- **An unconfigured agent proxy returns a setup error, not the panel's HTML.**
  The `/api/lightagent` namespace is now a real route even when no
  `--agent-upstream` is set, answering with a `503` JSON setup error naming the
  fix rather than falling through to the panel fallback and returning
  `index.html` — the `<!doctype …>` the agent screens choked on. A configured
  gateway proxies exactly as before.
- **Agent Tools explains a bad response and offers Retry.** The panel rejects a
  non-JSON response from the agent API with an actionable message (start
  `lightagent serve`, connect the gateway with `--agent-upstream`) and adds a
  Retry that re-runs the load once the connection is corrected, instead of
  showing `… is not valid JSON`. The end-to-end render check now proves both the
  message and the recovery.

## [0.3.1] - 2026-09-06

A patch release. When the control panel is served by the gateway, its agent
screens — Agent, Tools and Chat — now reach the agent API instead of falling
through to the panel's own HTML, so they work the same way the rest of the panel
already did.

### Fixed

- **The panel's agent screens reach the agent API through the gateway.** The
  agent API (`lightagent serve`) runs on its own server and port, and the gateway
  had no route for its `/api/lightagent` prefix, so those calls fell to the panel
  fallback and came back as `index.html` — which the panel's JSON parse rejected.
  The gateway now reverse-proxies `/api/lightagent/*` to the agent server,
  streaming responses (including the run event stream), so the agent, tools and
  chat screens are same-origin with the rest of the panel and need no CORS — the
  same property `--web-root` gives the control API. Configured by `hermes serve
  --agent-upstream <origin>` (default `http://127.0.0.1:8735`, `off` to disable);
  the cross-origin write guard covers the proxied surface, and a gateway with no
  upstream is unchanged.
- **An intermittent failure in the RAG store tests.** Two tests shared a scratch
  directory keyed only on a timestamp, so under parallel execution one could
  delete the other's directory mid-run; each call now gets a unique directory.
  Test-only — no runtime behaviour changed.

## [0.3.0] - 2026-09-04

The first release to include Lightagent, the agent harness, alongside the
Lightweight inference engine. A minor bump: Lightagent is a large, strictly
additive product surface, and the engine's binaries, tests and dependency policy
are untouched, so nothing existing breaks.

### Added

- **Lightagent, the agent harness.** New crates and a new `lightagent` binary
  serving the agent runtime — runs, sessions, tools, approvals and their event
  stream — with agent screens in the shared control panel, added alongside the
  inference engine without changing it.

## [0.2.1] - 2026-09-01

Public reach and multi-model serving. The gateway can now sit behind a trusted
reverse proxy or Cloudflare Tunnel with `--behind-proxy` — reachable at a real
domain, key-required, and no longer fooled into treating a remote caller as
local — and `hermes fleet` runs up to four models at once as isolated
per-tenant gateways. Per-user API keys and their rate limits now take effect the
moment they change instead of at the next restart, and the desktop icons are
rounded to the macOS squircle with a new violet-feather menu-bar mark.

### Added

- **`--behind-proxy` mode** for putting the gateway behind a trusted reverse
  proxy or Cloudflare Tunnel while it stays bound to loopback. It turns on
  API-key auth (refusing to start without a credential) and trusts the proxy's
  `CF-Connecting-IP` header — only from a loopback peer — so a remote caller is
  identified by its real address rather than passing as local. Set it with the
  flag or `HERMES_BEHIND_PROXY`. A plain loopback gateway is unchanged.
- **`hermes fleet`** runs up to four models at once, one isolated gateway per
  model. Each entry in a small JSON manifest gets its own data root, port and
  keys, so one tenant's traffic can never evict or disturb another's. The
  four-model cap and the manifest checks (duplicate ports/names, missing model
  files, a profile with no key) are enforced before anything launches.
- **A public-domain recipe** in the README: reaching the gateway at
  `https://…/v1` over a Cloudflare Tunnel with `--behind-proxy`, and serving
  several models behind per-hostname routing with `hermes fleet`.

### Changed

- **Per-user API keys and limits now take effect live.** Creating a key,
  changing its rate limit, or revoking it through the control API is honoured on
  the next request instead of at the next restart — the gateway reloads its key
  set from the store on each change. A revoked key stops working immediately.
- **The menu-bar (tray) icon** is now its own transparent mark, keyed from a
  dedicated `icon/tray-source.png`, rather than the plated brand icon.
- **The desktop app icons are rounded** to the macOS "squircle" with a
  transparent margin, so the app sits on the dock like a native one instead of a
  hard-edged square. Generated for every packaged size by `scripts/build-icons.py`.

### Fixed

- **An engine launch that loses the ephemeral-port race is retried.** The
  supervisor hands the engine a loopback port it proved free a moment earlier;
  on a busy machine another process can take it in the gap before the engine
  binds. Such a launch is now retried with a fresh port instead of surfacing the
  crash, as the design always intended. Only that transient case is retried — a
  signal, a timeout, or a genuinely unstartable engine is still reported at once.

## [0.2.0] - 2026-08-31

Remote access: the gateway can now be reached from another machine over any
overlay network, authenticated with named API keys that survive a restart and
can be rate-limited per key. A new **Access & Keys** panel and the `hermes key`
/ `hermes config` commands manage it, and the bind hosts and port persist in
`config/api.json`. The default port moves to **11434** to agree with the desktop
app and the common local-LLM clients — a behaviour change for anyone who relied
on the old `8737`.

### Added

- **Named, hashed API keys.** A gateway can now issue a key per consumer, each
  nameable and revocable on its own. Keys are stored as SHA-256 hashes and a
  display prefix in `config/api-keys.json`; the plaintext is shown once, at
  creation, and never again. Create, list and revoke them with `hermes key`, or
  on the panel's new **Access & Keys** screen. The existing `--api-key` /
  `HERMES_API_KEY` static key still works alongside them.
- **Per-key rate limits.** Each key can carry a per-minute and a per-day
  ceiling, enforced live: a key over its limit gets a `429` with a `Retry-After`.
  Loopback and anonymous callers (the panel, a local script) are never metered.
- **Persisted bind configuration** in `config/api.json` — the hosts and port the
  gateway serves on, read beneath the command-line flags so a typed `--host` or
  `--port` always wins. Edit it with `hermes config`, or the panel's *Serve on*
  control, which lists the machine's reachable addresses tagged with the reserved
  range each falls in (a Tailscale/CGNAT address reads *shared range*).
- **The `lightweight` command**, a second entry point identical to `hermes` that
  prints a feather welcome mark on an interactive terminal. `NO_COLOR` and
  `LIGHTWEIGHT_NO_BANNER` are honoured; `--json` and pipes are never decorated.
- `hermes sysinfo` reports an address's reserved-range scope, in the human output
  and as an `addresses` array under `--json`.
- **`hermes serve --port auto`** (equivalently `--port 0`) binds a kernel-assigned
  free port and prints it — the explicit way past a taken 11434 without moving the
  default. With several `--host` values it binds them all to the one shared port.
  It is a per-run choice and is never written to `api.json`.

### Changed

- **The `address in use` message on the default port is now a signpost.** Because
  11434 is also Ollama's default, `hermes serve` names that likely cause and
  suggests `--port auto`, a different `--port`, or stopping the other process,
  rather than failing with a bare "in use". The desktop shell inherits the same
  guidance and points at its own levers (`HERMES_PORT` or the *Serve on* control).

- **The default port is now 11434** (was 8737), so the CLI, the desktop shell and
  the dev proxy agree and a client assuming the common local-LLM port finds the
  gateway. Anyone who relied on the old default must now pass `--port 8737`.
- The desktop shell no longer mints a fresh API key on every launch — the bug
  that broke a key shared with a remote agent. Keys are the gateway's own, and the
  tray's "Copy API key" is now "Manage API keys…", which opens the panel.
- State-changing control endpoints under `/api/v1` now refuse a request from a
  foreign origin, and creating keys or widening the bind set is refused from a
  non-loopback peer: those take access to the machine running the gateway.

## [0.1.2] - 2026-08-30

### Added

- CPU utilization is now reported on macOS and Windows, so the Performance
  page's CPU Usage tile shows a live figure on every supported platform instead
  of only on Linux. It is read through `host_statistics` on macOS and
  `GetSystemTimes` on Windows, and normalised to the same tick units the panel
  already differences.

### Changed

- Completed the rename to **Lightweight**: the native desktop application — the
  window title, tray, menus, dialogs, and the installer and artifact names —
  now reads "Lightweight" instead of "Hermes", following the panel rename in
  0.1.1.
- Renamed the internal workspace crates from `hermes-*` to `lightweight-*`. The
  `hermes` command and its `HERMES_*` environment variables, the `hermes_*`
  metric names, the `hermes::` log targets, and the existing data directory are
  deliberately unchanged, so nothing that scripts, scrapers, or existing
  installs depend on has moved.

### Fixed

- Long file-path values no longer overflow their cards on the Settings and API
  Gateway pages; they wrap within the card instead.

## [0.1.1] - 2026-08-26

### Changed

- Renamed the desktop shell to **Lightweight**: the sidebar brand name and the
  window title bar now read "Lightweight" instead of "Hermes".

## [0.1.0] - 2026-08-25

### Added

- An OpenAI-compatible, CPU-only inference gateway backed by a supervised
  llama.cpp process.
- GGUF model discovery, verified downloads, imports, and live model switching.
- Conservative RAM admission control with context and KV-cache sizing.
- Benchmarking and machine-scoped calibration with trust checks that reject
  unsafe fits.
- A desktop UI and CLI packages for macOS, Windows, and Linux.

### Known limitations

- Calibration is intentionally deferred for pinned llama.cpp `b10590`:
  `hermes bench --fit` safely refuses every honest fit, so the shipped estimates
  remain conservative by 1.37×–2.85×.

[Unreleased]: https://github.com/dlroqa/Lightweight/compare/v0.1.2...HEAD
[0.1.2]: https://github.com/dlroqa/Lightweight/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/dlroqa/Lightweight/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/dlroqa/Lightweight/releases/tag/v0.1.0
