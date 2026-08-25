# Benchmarks

This directory holds **workload definitions**, not results. A measurement is a
fact about one machine and one engine build; committing one here would make it
look like a property of the software, which is the one thing it is not.

Results are written to the data directory — `benchmarks/` under the platform
data root, alongside `catalog.json` and `conversations/` — as one JSON document
per run, owner-only.

## Running one

```sh
# The whole harness: brings its own engine, reloads between buckets.
hermes bench model.gguf

# A sweep, which is what makes a calibration fit possible.
hermes bench model.gguf --ubatch 128,512 --fit

# Whatever is already loaded, without reloading it, from the panel or the API.
curl -X POST "$BASE/api/v1/benchmarks"
```

`hermes bench` never touches a running gateway: it starts its own engine and
shuts it down afterwards. The gateway's own benchmark is the smaller one — it
measures the resident model at the parameters it is already resident with, and
takes a scheduler slot like any other request.

## The three scenarios

Each is deterministic: temperature zero, a fixed seed, and a prompt built from
a fixed filler pattern sized by asking the engine's own tokenizer. What is
recorded is the length the tokenizer returned, never the length that was asked
for.

| Scenario | What it does | What it measures |
|---|---|---|
| `cold_prefill` | A prompt with a distinct opening per repetition | Prompt processing speed with nothing useful in the cache |
| `cached_prefill` | The same prompt every repetition | What prefix reuse actually saves |
| `decode` | A short prompt and a fixed output budget | Generation speed |

**`cold_prefill` is not perfectly cold, and honestly cannot be.** The model's
own chat template renders an identical preamble ahead of every message, and
those tokens legitimately match the cache. The engine reports them as cached and
they are excluded from the rate, so what is measured is the tokens it actually
evaluated.

## Why a sweep reloads between buckets

`VmHWM` is a high-water mark for the life of a process. A second bucket measured
inside the same engine would inherit the first bucket's peak and report it as
its own, so each bucket gets a fresh engine.

## What a run records, and what it cannot

Every sample carries the exact `RuntimeParams` it ran at, the achieved prompt
length, the engine's own prefill and decode timings, the time to first token
measured by the harness, the engine's processor ticks and the machine's over the
same interval, peak RSS, and the estimate that was predicted for those same
parameters.

There is nowhere in the record to put a prompt, a completion, a filesystem path
or a hostname — the same structural guarantee the metrics module relies on. The
prompts are generated here, so there is no user text to leak in the first place.

## The fit

`--fit` writes `calibration.json` beside the runs. It fits what peak RSS
supports and no more:

The estimator's compute term is
`vocab*ub*4 + activation*ub*embd*4 + scratch*ub*max(embd,ffn)*4` — two free
coefficients, both proportional to `n_ubatch`. From peak RSS alone they are
collinear: samples at one ubatch cannot separate them, and samples across
several determine only their sum. So the fit is a slope (bytes per unit of
ubatch) and an intercept (fixed overhead), and it says so rather than reporting
two coefficients of which one is invented.

**Nothing reads that file yet.** The estimator keeps its shipped defaults and
reports `Coarse`. Deciding when a fit is trustworthy enough to change a verdict
is a separate question from measuring, and belongs to the milestone that makes
that decision.

Fits are keyed by machine fingerprint, engine build and model bucket. A
mismatch is a miss, never an approximation: a coefficient fitted on four cores
without AVX describes four cores without AVX.
