# Hermes CPU Inference Gateway

A local, **CPU and RAM only** LLM inference platform: an OpenAI-compatible
gateway, GGUF model management, and a desktop UI. No GPU is required anywhere,
and none is used.

**It is a model provider, not part of any agent.** It serves the OpenAI API and
knows nothing about who is calling: an agent harness, a chat UI, an editor
plugin or `curl` are all just clients. Nothing it depends on, builds against, or
needs at runtime comes from a harness — the compatibility tests point a real
client library at it precisely because that is the honest way to check a
provider, and they skip cleanly when that client is not installed.

The inference engine itself sits behind a trait boundary, so the current
llama.cpp backend can later be replaced by a proprietary runtime without
touching the UI or the API.

## Download

Download the current builds from the
[GitHub Releases page](https://github.com/dlroqa/Lightweight/releases).

Every artifact is built and then *run* on the platform it is for, by
[`release.yml`](.github/workflows/release.yml).

| Platform | File |
|---|---|
| macOS, Apple Silicon and Intel | `Lightweight-*-mac-universal.dmg` |
| Windows x86-64 | `Lightweight-Setup-*.exe` |
| Linux x86-64, sandboxed | `Lightweight-*.flatpak` |
| Linux x86-64, portable | `Lightweight-*.AppImage` |
| Command line only | `hermes-*-<target-triple>.tar.gz` / `.zip` |

Two facts worth knowing before the first launch:

- **Nothing is code-signed.** There is no Apple Developer ID and no
  Authenticode certificate for this project, so macOS asks you to right-click →
  Open the first time and SmartScreen offers *More info* → *Run anyway*. Each
  release carries `SHA256SUMS` and a build-provenance attestation naming the
  workflow run that produced the bytes; neither is a code signature.
- **The inference engine is downloaded, not bundled.** On first use Hermes
  fetches the pinned llama.cpp build for your platform and checks it against a
  SHA-256 recorded in this source tree. Nothing is compiled on your machine.

Of the two Linux builds, prefer the Flatpak: it keeps its Chromium sandbox on a
host that does not allow unprivileged user namespaces, where the AppImage
refuses to start rather than run unsandboxed.

## Status

Milestones 0 through 10 are complete. The gateway serves an OpenAI-compatible
API over a supervised llama.cpp child process: streamed and non-streamed chat
completions, **tool calls**, **`/v1/completions`**, `/v1/models`, `/health` and
`/props`, with RAM admission control and GGUF metadata underneath it.

A three-turn streamed conversation through the genuine `openai` Python SDK
against a real model works end to end, and disconnecting mid-stream stops the
engine within milliseconds. A **full agent loop** does too: against Qwen3-1.7B,
the model called a declared tool, the tool ran, and the result came back as an
answer — `finish_reason: "tool_calls"`, arguments that parse as JSON, and 166
of 222 prompt tokens served from the prefix cache on the second turn.

Requests are **scheduled** rather than merely queued: a short request overtakes
a long one that is waiting, a queued streamed request is told where it stands
instead of sitting in silence, and what the gateway is doing is readable at
`/metrics`.

The gateway has a **model manager**. It keeps a catalog of what this machine
has, imports a `.gguf` you already own without copying it, downloads one of the
pinned lightweight models against a digest recorded before the download, or
takes any direct https link — recording per model how much can actually be
promised about its bytes. A running gateway can be told to **load a different
model**: it stops admitting, lets the request in flight finish, admits the new
model against free memory, and re-derives the scheduler's ceilings for the
context it ends up serving.

It also has a face. `/api/v1` reports the machine it runs on, the gateway's own
configuration, a live event stream and the log file, and stores conversations
and settings in the two directories M0 chose for them. On those seams sits a
**control panel** — React and TypeScript, eight screens — served by the gateway
itself, so the panel and the API are the same origin and no CORS layer exists
anywhere. A browser on another machine reaches it over the exposed bind for
free.

Around both is a **desktop shell**. Electron: it attaches to a gateway already
serving or starts one of its own, stops only what it started, and keeps serving
after its window is closed. Keys are the gateway's own — hashed on disk, created
in the panel or with `hermes key create`, and shown once — so a key shared with a
remote agent survives a restart of the shell.
`npm run package` builds this platform's installers — a Flatpak and an AppImage
on Linux, a universal DMG on macOS, an NSIS installer on Windows — each carrying
the release binary and the built panel.

The three quantities that decide whether a model runs at all — the KV cache, the
memory budget and the context window — are now **honest about what they cannot
compute, and reachable by the person they constrain**. An estimate says which
term it could not measure rather than billing it at zero. A refused load arrives
with the numbers to fix it: reduce the context to N tokens, quantize the KV cache
and save this much, free that much. A model swap is judged against the memory the
outgoing engine is about to release — its anonymous set, not its whole resident
set, because the weights are mmapped and the kernel already counts them. And the
panel offers a context and a KV cache type per load, priced by the gateway before
the button is pressed.

And it can now be **measured rather than described**. `hermes bench` brings its
own engine, sizes its prompts by asking the tokenizer, and records what this
machine did with a model — prefill, decode, what prefix reuse saves, cores
actually kept busy, peak memory against what was predicted — beside a
fingerprint of the machine and the engine build, because a throughput figure
without those is not a measurement of anything. The knobs that move those
numbers are reachable at last: the physical batch, priced by the estimator
before the load; prompt-processing threads; polling; prefix-cache reuse; CPU
affinity; and how the weights are brought into memory, which is checked against
the kernel's locked-memory allowance before an engine is started rather than
warned about on its stderr afterwards.

And it now serves **more than one client at a time, correctly**. That sentence
was not true before M9 and had never been checked: `--ctx-size` is a total the
engine divides across its slots, so raising `--concurrency` had been quietly
handing every client a fraction of the window every endpoint advertised. The
context is multiplied where the engine's convention is known and nowhere else,
the engine's own answer is read back and believed, and the slot count is derived
from the machine — one slot per four cores, because a single generation was
measured to keep close to four busy, and fewer if a full-sized window for every
client would not fit. The queue is fair between callers as well as between
requests, keyed on the address the kernel put on the connection: observed, never
claimed, and never logged, labelled or stored. And the panel can finally show
what is running *while* it is running.

And it now **runs on the three platforms it ships to, and can calibrate itself
on the machine it lands on**. All four platform runners execute the same
`scripts/check.sh` the contributor runs — no YAML copy of it — and each artifact
is built and then started on its own platform, including the x86-64 half of the
universal DMG on genuine Intel hardware. The estimator will take a compute model
measured here in place of its shipped guesses, but only when the fit is
trustworthy: it is scoped to this machine, this engine build and this ggml
variant, and it is refused when the model behind it does not hold. On the
development machine no fit earns that trust, and the conservative shipped
numbers stand — which is the design working, not a gap in it.

The approved plan M0–M10 is complete. See
[docs/PROGRESS.md](docs/PROGRESS.md) for the current checkpoint, and
[docs/M10-PLAN.md](docs/M10-PLAN.md) for what the last milestone deliberately
left undone.

## Verified constraints

These were measured on the development machine, not assumed. They shape several
design decisions and are worth knowing before changing anything.

| Fact | Consequence |
|---|---|
| The dev CPU (Pentium Silver J5005) has **SSE4.2 only — no AVX, AVX2, FMA or F16C** | The engine must be dispatched at runtime, never built with `-march=native` |
| Official llama.cpp Linux/Windows builds use `GGML_BACKEND_DL=ON GGML_CPU_ALL_VARIANTS=ON` | Stock prebuilt binaries run here via `libggml-cpu-sse42.so`; **no compilation, no cmake, no sudo** |
| Measured `ggml_backend_score()` on this CPU | `sse42` = 5, `x64` = 1, every AVX-and-above variant = 0 |
| `sudo` is unavailable | Nothing may require a system package |

The last of those is why the dependency policy below exists.

## Dependency policy

Enforced by `scripts/check-deps.sh`, which CI runs. Violating it makes the
workspace **unbuildable** on the target machine, and the failure surfaces as an
inscrutable build-script error rather than as "you broke the policy".

- **No cmake.** `rustls` defaults its crypto provider to `aws-lc-rs`, whose
  build requires cmake. Pin `ring` instead: `reqwest` with `rustls-no-provider`,
  plus an explicit `rustls::crypto::ring` provider at startup.
- **No OpenSSL.** `libssl-dev` is absent, so `native-tls` and `openssl-sys` are
  banned. rustls end to end.
- **No libclang**, therefore no `bindgen` and no llama.cpp FFI. The engine is a
  supervised child process instead — which also means a backend crash is
  recoverable rather than fatal.
- **No nightly**, and no crates with a heavy proc-macro compile cost
  (`polars`, `tonic`, `sqlx`). Builds run on 4 cores at 1.5 GHz.

## Build and test

```sh
cargo test --workspace       # 777 tests, no network, no model downloads
./scripts/check.sh           # fmt, clippy, tests, contract suite, dependency + secret gates
./scripts/contract-test.sh   # the real openai SDK against the gateway
```

The contract suite is the highest-value tier: it points the genuine `openai`
Python package at a real gateway backed by a deterministic mock engine and
asserts what the **client** ends up with — content assembled, tool-call
arguments that parse as JSON, `usage` populated from the terminal chunk — rather
than what we intended to send. It also imports Hermes' own
`parse_context_limit_from_error` and checks that our context-overflow message
parses back to the right number.

`HERMES_TEST_MODEL=<path.gguf>` additionally enables the real-engine tests,
which download the pinned engine, load a genuine model, stream a completion and
prove that a dropped stream stops the engine decoding.

`HERMES_TEST_NETWORK=1` enables the model-download tier, which fetches a real
100 MB model from HuggingFace and checks what arrives: the digest matches the
one recorded in the manifest, an interrupted transfer resumes and still
verifies, a tampered digest is refused and the bytes discarded, and a link that
is not a GGUF is deleted rather than catalogued.

`scripts/fetch-real-headers.sh` captures the headers of six real models from
HuggingFace with HTTP range requests — a few megabytes rather than the tens of
gigabytes the full models would cost. The tests that use them skip when they are
absent, so a clean checkout still passes; set `HERMES_REQUIRE_REAL_MODELS=1` to
turn that skip into a failure, which is what CI does.

## CLI

```sh
cargo run -p lightweight-cli -- sysinfo
cargo run -p lightweight-cli -- inspect model.gguf
cargo run -p lightweight-cli -- estimate model.gguf --ctx 8192 --kv-type q8_0

# Acquire the engine, admit the model, load it, and serve the OpenAI API.
# With no --ctx, the largest context that fits this machine is chosen.
cargo run -p lightweight-cli -- serve model.gguf

# Or start with nothing loaded and choose a model over the control API.
cargo run -p lightweight-cli -- serve
```

### Benchmarks

```sh
hermes bench model.gguf                          # its own engine, then gone
hermes bench model.gguf --ubatch 128,512 --fit   # a sweep, and a calibration fit
curl -X POST "$BASE/api/v1/benchmarks"           # measure what is already loaded
```

`hermes bench` never disturbs a running gateway: it starts its own engine,
reloads between buckets — `VmHWM` is a high-water mark for the life of a
process, so a second bucket in the same engine would inherit the first one's
peak — and shuts down after. The gateway's own benchmark is the smaller one, and
takes a scheduler slot like any other request.

Three scenarios: a prefill the cache has not seen, the same prompt again to
measure what reuse saves, and a decode. Prompts are built from a fixed filler
and **sized by asking the engine's own tokenizer**; what is recorded is the
length that came back, never the length that was asked for.

A run holds no prompt, no completion, no filesystem path and no hostname —
there is nowhere in the record to put one, which is the same structural
guarantee the metrics types rely on. Results go to the data directory, and
never into this repository: a throughput figure is a fact about one machine and
one engine build, and both travel with every run.

`--fit` writes `calibration.json` beside the runs. It fits a slope against
`n_ubatch` and an intercept, and no more: the estimator's two free compute
coefficients are collinear in peak RSS, so a fit that reported both would be
inventing one of them. **Nothing reads that file yet** — deciding when a fit is
trustworthy is a separate question from measuring, and belongs to the milestone
that makes it. See [benchmarks/](benchmarks/).

### Models

```sh
hermes models list                    # what this machine has
hermes models available               # the pinned models, with sizes
hermes models add qwen3-1.7b-q4_k_m   # download one, digest checked
hermes models add --url https://huggingface.co/owner/repo/resolve/main/m.gguf
hermes models import ~/models/mine.gguf   # referenced where it is, not copied
hermes models remove <id> [--delete]
```

A HuggingFace link is verified against the sha256 the site publishes for the
file. Any other link is **recorded, not verified** unless you pass `--sha256`,
and the catalog says which of the two it was rather than showing one tick for
both.

### Control API

Under `/api/v1`, deliberately never mixed in with the OpenAI routes:

```sh
curl 127.0.0.1:8737/api/v1/models          # the catalog, with load state
curl 127.0.0.1:8737/api/v1/catalog         # what can be downloaded
curl -X POST 127.0.0.1:8737/api/v1/models/<id>/load -d '{"ctx":8192}'
curl -X POST 127.0.0.1:8737/api/v1/models/unload
curl -N   127.0.0.1:8737/api/v1/jobs/<n>/events   # progress, as SSE
```

Loading and downloading return a job immediately rather than holding the socket
open for minutes. Swapping a model waits for the request in flight to finish —
nothing is preempted — and a request that arrives during the swap queues rather
than being refused.

Then point any OpenAI client at `http://127.0.0.1:8737/v1`:

```sh
curl 127.0.0.1:8737/v1/models
curl -N -H 'Authorization: Bearer no-key-required' \
     -d '{"model":"<id from /v1/models>","messages":[{"role":"user","content":"hi"}],
          "stream":true,"stream_options":{"include_usage":true}}' \
     127.0.0.1:8737/v1/chat/completions
```

Authentication is off for a loopback bind — a client that sends
`Bearer no-key-required`, or no header at all, is accepted — and is **forced on**
the moment any bind is reachable from another machine.

`--concurrency` decides how many requests run at once, and defaults to `auto`.
It is one number on purpose: it sizes the engine's slots, the gateway's queue
and the RAM estimate together, because the KV cache is per sequence and four
concurrent sequences cost four caches — so each client keeps the whole context
the gateway advertises rather than a quarter of it.

`auto` takes the smaller of two answers. **Cores**: one slot per four, because a
single generation was measured to keep 3.0 to 3.8 of this machine's four cores
busy — a second slot takes a core rather than finding one. **Memory**: at that
many slots a full-sized window for every client must still fit, and a machine
that cannot hold them serves fewer clients well rather than more badly. On the
development box that is one slot, which is what the flag has always defaulted
to; on a sixteen-core machine it is four. `hermes serve` prints which rule
decided, and a number overrides both.

The slot count follows the *engine*, not the command line: it is re-derived on
every model load, beside the band ceilings, because a count that is right for
one model is not right for the next.

## Waiting, and being told about it

The engine generates for one request at a time, and on a slow CPU one turn can
take minutes. Which request goes next is therefore the whole scheduling problem,
and first-come-first-served answers it badly: an agent that sends a 20-token
title generation alongside a 5,596-token turn watches the small request sit
behind the large one until its own timeout fires. That happened here, and it is
what the scheduler is for.

- **A request is classified from what has been measured**, never from what it
  claims. The two numbers are the prompt the engine counted — tool declarations
  included — and the output budget the client asked for. A priority that can be
  requested is a priority every caller requests, so there is no way to ask for
  one.
- **A short request overtakes a long one that is waiting.** The ceilings come
  from the context the model was loaded with, which was itself chosen from this
  machine's free memory.
- **Nobody waits forever.** Once a request has waited past the starvation
  ceiling it can no longer be overtaken. Being long costs a bounded delay, never
  a denial.
- **Nothing is preempted.** A band decides who *starts* next; a generation
  already running is never paused, because the engine cannot pause one and
  restarting it would throw away the prefill that dominates the cost.
- **The queue is fair between callers, not only between requests.** Bands answer
  "what does this cost?" and cannot answer "whose turn is it?" — ten requests
  from one client and one from another are all in the same band. Each caller's
  first waiting request competes with every other caller's first, so a client
  that sends twenty puts one in front of yours and nineteen behind. The key is
  the address the kernel put on the connection: observed, never claimed, and
  never logged, labelled or stored.

A streamed request that arrives to a busy gateway is answered immediately and
waits inside its own response, so `curl -N` shows it moving up the queue:

```
: queued position=1 waited=15s
: queued position=0 waited=30s
data: {"choices":[{"delta":{"role":"assistant"},...
```

`GET /api/v1/requests` answers the question the counters never could: what is
being served *right now*, with the band and the prompt each request carries, and
what is queued behind it. A generation only reaches `/api/v1/events` once it has
finished, so a gateway serving two clients for two minutes each used to look
much like an idle one until they both finished at once.

Those are SSE comments, which every client's decoder discards — a queued request
has produced no tokens, and a `data:` frame would be a completion that is not
one. A request that cannot stream still waits at the door and is refused with a
503 and `server_busy` if the queue timeout runs out; a streamed one that waits
too long gets the same code in the terminal error chunk, because which side of
the response headers a client happened to be on is our detail, not theirs.

## Metrics

```sh
curl 127.0.0.1:8737/metrics          # Prometheus text
curl 127.0.0.1:8737/api/v1/metrics   # the same snapshot as JSON
```

Requests by endpoint and outcome, generations by finish reason, tokens by kind —
prompt, completion, cached, actually prefilled, decoded — queue depth by band,
scheduler events including how many times a waiting request was overtaken, and
four timings: queue wait, time to first token, prefill and decode. Prefill and
decode are the engine's own measurements; the queue wait and the time to first
token are measured here, because only the gateway can see them. They are
reported separately rather than added, since a slow first token and a busy queue
are different problems with different fixes.

Each of the four is a **histogram**, so `histogram_quantile()` has buckets to
work with: on a gateway where one slow request in fifty is the whole complaint,
the mean moves by milliseconds while the tail moves by minutes. The ladder ends
at two minutes rather than ten seconds, because a prefill on a CPU without AVX
genuinely reaches it and a web-shaped ladder would file every one of them under
`+Inf`.

The **engine's own processor time** is there too, in kernel clock ticks and
unconverted — a rate needs two readings and an interval, and converting would
divide by a `USER_HZ` this process would have to guess at. The panel differences
two readings against `/proc/stat` over the same interval and reports cores: a
ratio of two tick counts, with the unit cancelled rather than assumed. Beside it
sit the few counters the engine knows and the gateway cannot compute — the
longest sequence it has served, its decode steps, slots busy per decode, and
what it deferred internally.

Every field is a number. No prompt text, no completion text, no filesystem path
— metrics are the easiest accidental route out for exactly the content Privacy
Mode protects, so the types have nowhere to put it. Both endpoints sit behind
the API key when one is configured.

A generation the client abandoned is counted as **cancelled**, not as an error:
closing a laptop lid is a normal act, and counting it beside a crashed engine is
how an operator ends up chasing a failure that never happened. Per-token timings
are enabled upstream, so an abandoned generation still reports what it had cost
— the chunk that used to carry that number is the one such a request never
receives.

## The two generation endpoints

They are different things, not two spellings of one. `/v1/chat/completions`
renders a conversation through the model's own chat template;
`/v1/completions` continues raw text with **no template at all**. Anything
filling in a form, continuing a document, or driving a base model that has no
chat template needs the second, and given only the first it would get an answer
to a conversation it never had.

```sh
# Continues the sentence: " Paris. The capital city of the United States is…"
curl "$BASE/v1/completions" -H 'Content-Type: application/json' \
  -d '{"model":"'"$MODEL"'","prompt":"The capital city of France is","max_tokens":10}'
```

`prompt` may be an array, and `n` asks for more than one completion each; both
expand to one choice per prompt, numbered in prompt order, sharing a single
`usage`. They run one at a time because the gateway holds one slot per request
— not because the endpoint says so, so raising the slot count later makes them
concurrent without changing the endpoint.

Four parameters are **refused by name** rather than ignored, because ignoring
one returns a well-formed reply to a different request: `logprobs`, `best_of`,
`suffix`, and a pre-tokenized `prompt`. A client that asked for `logprobs` and
received `null` could not tell whether the model had nothing to say or the
gateway never asked.

### Tool calls

`tools`, `tool_choice` and `parallel_tool_calls` are acted on, which is what
makes an agent loop possible at all: a gateway that accepts `tools` and does
not forward them tells the model nothing, so the model never calls anything and
the loop never starts.

Tool declarations **cost prompt tokens**, because the template renders every one
of them into the prompt, and they are counted as such. Measured against a
tool-capable template, one small tool moved the count from 9 tokens to 157 — the
same +148 the real generation reported. A token count taken without them would
leave the overflow check short by an entire toolset, and the overflow would then
surface from the engine in wording no client can parse.

An unusable tool declaration is a 400 that names the entry, rather than being
skipped. Skipping it would leave the client believing the model had been told
about a tool it had not, and the symptom — a model that never calls that one
tool — points nowhere near the cause. The same goes for a `tool_choice` naming a
function `tools` does not declare, which is what a half-finished rename looks
like.

## Remote access

The gateway serves whichever addresses it is told to. It draws exactly one
distinction — **loopback or not** — and knows nothing about interfaces,
networks, or which product assigned an address, so a plain LAN, Tailscale,
Headscale, Netmaker, ZeroTier, Nebula and a hand-rolled WireGuard are all the
same case:

```sh
hermes key create --name my-agent          # printed once; stored hashed
hermes serve model.gguf --host <address-or-name>

# A machine holding several addresses can serve on each of them, with one
# engine and one queue behind them all:
hermes serve model.gguf --host <lan-name> --host <mesh-name>
```

A remote agent then authenticates with the key it was given:

```sh
export OPENAI_BASE_URL=http://<address-or-name>:11434/v1
export OPENAI_API_KEY=sk-lw-…               # the key hermes key create printed
```

`--host` takes an address in either family or a **name**, resolved at startup.
Prefer the name: an overlay network can reissue an address, and a name usually
survives it. The default port is **11434** — the common local-LLM port — so a
client that assumes it finds the gateway without being told.

### Keys, and where the configuration lives

Two files under the config directory (`~/.config/CpuInferenceGateway` on Linux,
or wherever `HERMES_GATEWAY_HOME` points), both owner-only:

* **`api.json`** — the bind hosts and port. Written by `hermes config` or by the
  panel's *Serve on* control, and read beneath the command-line flags: a typed
  `--host`/`--port` always wins, and the file speaks only when one was not given.
* **`api-keys.json`** — the API keys, stored as SHA-256 hashes and a display
  prefix. A key is shown once, when it is created, and never again; a lost key is
  revoked and replaced, not recovered. `--api-key` and `HERMES_API_KEY` still
  work as a single static key alongside the named ones.

```sh
hermes key create --name ci --per-minute 60 --per-day 2000
hermes key list                # names, prefixes and limits — never the secret
hermes key revoke <id>
hermes config show             # what api.json currently holds
```

Per-key limits are enforced live: a key over its ceiling gets a `429` with a
`Retry-After`, while the machine's own loopback callers (the panel, a local
script) are never metered.

The panel's **Access & Keys** screen does all of this with a copy-paste
connection block, and lists the machine's reachable addresses with the reserved
range each falls in — a Tailscale/CGNAT address reads *shared range*, a
unique-local IPv6 address *unique-local* — so choosing which to serve on is a
click rather than a reading of `ip addr`. Creating and revoking keys, and
widening the bind set, are refused from a remote session: those take access to
the machine itself.

Every command above is also available as `lightweight` — the same tool, which
prints a small feather on an interactive terminal to confirm you are in.
`hermes` is unchanged.

Ask the machine what it can be reached at rather than guessing:

```sh
hermes sysinfo            # the Network section lists every bindable address
hermes sysinfo --json     # same, under "reachable_addresses", for scripts
```

### The one that catches everyone

`hermes serve --host "$(hostname)"` is the obvious way to ask for remote access,
and on most Linux installs it serves **nobody**. Debian and Ubuntu write
`127.0.1.1 <hostname>` into `/etc/hosts` at install time, and that entry beats
whatever a LAN or an overlay network publishes for the same name. Every signal
then reads as success — the name resolves, the bind succeeds, the gateway prints
that it is serving — while authentication is silently *off*, because the bind
really is local. The only symptom is that the other machine cannot connect,
which looks like a firewall or a port long before it looks like the name.

So the gateway now says so, on stderr, right where it prints what it is serving:

```
warning: --host "hermes" resolved only to 127.0.1.1, which is loopback.
  Only this machine can reach the gateway, and authentication stays off because
  nothing is exposed. Many Linux installs map the hostname to a loopback address
  in /etc/hosts, and that entry wins over any name the network publishes for it.

  Addresses another machine could reach this one at:
    --host 192.0.2.10
    --host 198.51.100.4

  Bind one of those, or a name that resolves to one, and create a key with `hermes key create`.
```

It warns rather than refuses: a name that resolves to loopback is unusual but
not invalid, and breaking a configuration somebody has working is too high a
price for making a point. `--host localhost`, anything under `.localhost`, and a
literal address are never second-guessed — a literal is what it says it is, and
`localhost` is an explicit request for loopback.

A second trap sits next to it, and no software can detect this one for you: the
name your **overlay network** knows a machine by is not necessarily its local
hostname. If the name was already taken in the network, the machine will have
been given another — a host whose `hostname` is `hermes` can be `hermes-1` on
the mesh, with `hermes` belonging to somebody else entirely. `hermes sysinfo`
reports addresses, which are unambiguous; check the name against the network's
own listing before trusting it.

Without a key, a bind that other machines can reach is refused rather than
silently exposed, and the refusal prints a generated key to use. An
unauthenticated caller is not shut out of everything: `/health` and `/props`
still answer with what a client needs to size a prompt, minus anything about
this machine.

### What address to serve on

This is the one surprising part, and it is a property of the **client**, not of
this gateway. Hermes raises its stream read timeout from 120 s to 1800 s, and
its stale-stream window from 180 s to 900 s, only for hosts it considers local
— which matters enormously when prefill on a CPU can take minutes:

| Reached at | Long-prefill timeouts | Typical source |
|---|---|---|
| `10/8`, `172.16/12`, `192.168/16` | **yes** | LAN, Netmaker, ZeroTier, Nebula, WireGuard |
| `100.64/10` | **yes** | Tailscale, Headscale |
| IPv6 unique-local `fd00::/8` | **yes** | Tailscale v6, ZeroTier 6PLANE |
| a name with no dots | **yes** | any of the above |
| any dotted FQDN, including `.local` and `*.ts.net` | no | mDNS, MagicDNS, TLS proxies |
| a routable public address | no | port forwarding, cloud hosts |

If a deployment has to use an FQDN, set `HERMES_STREAM_READ_TIMEOUT` and
`HERMES_LOCAL_STREAM_STALE_TIMEOUT` on the client rather than hoping.

**Encryption is the network's job, not this gateway's.** WireGuard-based
overlays and ZeroTier already encrypt the hop; on a plain LAN, HTTP is in the
clear, so the API key protects against *use*, not against *reading*. On an
untrusted network the honest answers are an overlay, an SSH tunnel, or a
TLS-terminating proxy — the last two usually meaning an FQDN, with the
consequence in the table above.

### A client may demand more context than the machine can give

The gateway advertises the context it is **actually** serving, which is the
only number that prevents overflow — but a client is entitled to have its own
floor. One agent harness tested against refuses any model advertising under
64,000 tokens, and said so rather than failing later:

```
Model … has a context window of 8,192 tokens, which is below the
minimum 64,000 required by … Choose a model with at least 64K context.
```

That is a client policy, not a fault in either side. Serving such a client
means loading a model at 64K, and the KV cache for that is measured in
gigabytes — `hermes estimate model.gguf --ctx 65536` says whether this machine
can, before anything is loaded. On a machine that cannot, the honest options
are a client with a lower floor or more memory; advertising a window the
gateway is not serving would trade a clear refusal for silent truncation.

### Keeping it running

`hermes serve` in the foreground needs nothing from any platform and is the
portable answer. For a machine that should serve after logout,
`packaging/systemd/` holds a Linux `systemd --user` example whose every value —
model, addresses, key — comes from a file outside the repository. M10 shipped the
per-platform installers; a macOS launchd and a Windows service wrapper are still
left to a later pass.

`estimate` exits non-zero when a model would not fit, so scripts can branch on
the verdict without parsing the report. Add `--json` to any command for machine
-readable output.

## Layout

| Crate | Responsibility |
|---|---|
| `lightweight-core` | Domain types, the actionable-error contract, privacy primitives. Pure: no I/O |
| `lightweight-gguf` | Bounded, panic-free GGUF header reader. Never reads tensor data |
| `lightweight-system-info` | CPU topology, ISA detection, memory probing, data directories |
| `lightweight-memory` | RAM estimation and admission verdicts |
| `lightweight-inference` | The backend abstraction. Contains no engine, by design |
| `lightweight-backend-llamacpp` | Acquires and supervises `llama-server`, and translates its SSE into our events |
| `lightweight-backend-mock` | A deterministic backend, so the layers above are testable without an engine |
| `lightweight-download` | Resumable, digest-verified HTTP downloads. Shared by the engine installer and the model catalog, and knows about neither |
| `lightweight-catalog` | What models this machine has and how each one arrived. Atomic writes; integrity recorded per model, never rounded up |
| `lightweight-store` | What the user accumulates: conversations and settings. Owner-only, because a model can be downloaded again and a conversation cannot |
| `lightweight-api` | OpenAI request and response types, and the SSE chunk codec |
| `lightweight-gateway` | The HTTP surface: routes, auth, streaming, cancellation, the scheduler, metrics, the control API and the panel it serves |
| `lightweight-observability` | Structured logging, rotation, privacy-mode wiring |
| `lightweight-bench` | Measures what this machine does with a model, and records it so it can be believed rather than assumed |
| `lightweight-cli` | Command-line access to the above |

Two parts of the product are not crates:

| Package | Responsibility |
|---|---|
| `frontend/` | The control panel: a React and TypeScript SPA, built by Vite and served by the gateway at `/` |
| `apps/desktop/` | The desktop shell: an Electron window onto a gateway, and a supervisor for one |

And one is not code. `icon/source.png` is the application's artwork; every icon
the product ships — the window, the tray, the packaged application, the browser
tab and the panel's own rail — is cut from it by `scripts/build-icons.py`,
which re-frames the mark for the size it will be seen at rather than trusting a
crop guessed once. The outputs are committed, so nothing at build or run time
needs the script or Pillow; re-running it is only needed when the artwork
changes.

## Designed for the machine it runs on

Limits are derived at runtime, never frozen as constants measured during
development. Thread count comes from detected physical cores; the RAM safety
margin is `max(512 MiB, 15% of available)`, so it grows with the machine; the
ggml CPU variant is chosen by detected instruction set, so one artifact is
optimal on a modern CPU and functional on an old one; and `serve` with no
`--ctx` picks the largest context that still loads safely, which is 8K on a
small laptop and the model's full maximum on a capable machine.

Every performance knob ships with the **engine's own default**, so nothing
changes until someone chooses. Where a knob has a plausible better setting on
some machine and none that can be established on the development one — prefill
threads on a processor with SMT, polling on an idle box, pinning on hybrid
cores — it is made reachable and measurable rather than given a default derived
from four slow cores. The two defaults this project *did* settle in M8 were
settled by a measurement: a benchmark showed a larger physical batch worth
offering, and another showed the gateway's own worker threads cost 0.06% of
what the engine costs, so they were left alone.

The development machine is deliberately a constrained one — four slow cores, no
AVX, most of its memory already spoken for — but it is one end of the range, not
the design point. Nothing in the repository is tuned to it; `CARGO_BUILD_JOBS`
is documented for constrained hosts rather than pinned in `.cargo/config.toml`.

## Two invariants worth preserving

**Errors are actionable by construction.** Every error type implements
`hermes_core::Actionable`, so adding a failure mode without stating what the
user can do about it does not compile.

**The client's contract is pinned by tests, not by intent.** The chunk order,
the tool-call id-once discipline, the usage chunk's empty `choices`, and the
exact bytes of a stream are all asserted — the last of those by byte-exact
golden transcripts. Where a client behaviour matters, the test runs the client's
own code: the real `openai` package, and Hermes' own error parser.

**Prompts cannot be logged by accident.** User text is wrapped in
`hermes_core::Private`, which redacts through both `Display` and `Debug` — so
`tracing`'s `?field` capture is safe — and has no `Serialize` impl, so it cannot
be swept into a JSON log line by an enclosing `derive`. Contents come out only
via `.reveal()`, a single greppable token. Privacy Mode is a one-way latch:
once set, prompt logging cannot be re-enabled for the life of the process.
