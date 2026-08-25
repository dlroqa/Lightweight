# M10 — the machines it ships to, and the numbers it guesses

Written against the source rather than against memory of it. Every claim about
what this tree does today was read out of `crates/`, `apps/desktop/` and
`node_modules/app-builder-lib/` on the day it was written, and every fact about
the development machine was measured on it.

M10 is the last milestone of the approved plan, and it carries two things that
look unrelated and are not. **Calibration** turns a benchmark's residuals into
the two estimator coefficients that ship as guesses. **Cross-platform** puts the
product on machines that are not this one. They belong together because a fit is
a fact about a single machine and a single engine build: `MachineFingerprint`
exists so that a coefficient measured on four cores without AVX is never applied
to anything else, and that guarantee is worth exactly nothing until there is a
second machine for it to refuse.

It splits accordingly:

| | |
|---|---|
| **M10a** | the machine can be measured off Linux, the Linux artifact stays sandboxed, one artifact per platform, and the Flatpak stops failing before it starts |
| **M10b** | a fit becomes a compute model, the load paths ask for one, and `Confidence::Measured` becomes reachable |

M10a is delivered and is described in section 2 as history. M10b is the plan.

## 1. What was wrong before this milestone

### The half M10a closed

| # | Where | What it did |
|---|---|---|
| 1 | `hermes-system-info` | `SystemMemoryProbe::snapshot()` returned `UnsupportedPlatform` off Linux, on the unconditional admission path. `serve`, `estimate`, `bench` and the gateway's load path all died on that one line: a macOS or Windows installer would have shipped a program that exits with an error. |
| 2 | `AppImageTarget.js:25`, `appImageUtil.js:213` | The shipped AppImage disabled Chromium's sandbox in two places and said nothing. One of them is a launcher electron-builder regenerates on every build, which no configuration option reaches. |
| 3 | `apps/desktop/package.json` | `extraResources` pointed at `target/release/hermes`, so whatever happened to be there got shipped — a binary predating `--web-root` was packaged once and the shell failed on first run for an unrelated reason. |
| 4 | same | The same path hard-coded a Unix name while `gateway.ts:101` looks for `bin/hermes.exe`. |
| 5 | `build.flatpak.license` | `"LICENSE"` resolves against `apps/desktop/build` and then `apps/desktop`; the only LICENSE is at the repository root. **The Flatpak build failed before flatpak-builder was ever invoked** — the first CI run would have died there. |
| 6 | `packaging/flatpak/README.md` | Named the data directory `~/.var/app/ai.hermes.cpu-inference-gateway/`. `filterFlatpakAppIdentifier` rewrites hyphens to underscores, so that directory can never exist. |
| 7 | `build.flatpak.finishArgs` | No `--talk-name=org.kde.StatusNotifierWatcher`, while `main.ts:211` creates a `Tray`. The tray could not have registered. |

### The half M10b has to close

| # | Where | What it does today |
|---|---|---|
| 8 | `estimator.rs:82` | `Estimator::new(ComputeModel)` — the seam that has existed since M1 — **has no production caller.** Its only caller in the workspace is a test at `estimator.rs:452`. |
| 9 | `manager.rs:607`, `control.rs:755`, `serve.rs:450`, `serve.rs:543`, `bench.rs:70`, `main.rs:466` | Six sites construct an estimator, all of them with `headless()` or `default()`. Every estimate the product has ever produced used the shipped guesses: `activation_factor: 8.0`, `scratch_factor: 4.0`, `engine_baseline: 64 MiB`. |
| 10 | `estimate.rs:107` | `ComputeModel.measured` is `false` in both constructors and set to `true` nowhere outside a test. **`Confidence::Measured` is unreachable**, so `main.rs:777`'s "measured for this model" is dead text, and the panel's `estimate.confidence !== "measured"` notice at `Models.tsx:658` is shown unconditionally — permanently telling the user their estimate is an upper bound. |
| 11 | `fit.rs:157` | `Calibration::find` — machine, engine and bucket matching, written for exactly this — **has no caller outside its own tests.** `hermes bench --fit` writes `calibration.json` (`bench.rs:159-166`) and nothing in the workspace ever reads it back. |
| 12 | `bench.rs:159` | The path is a literal `data_dir().join("calibration.json")` at one site. A second reader means a second literal, and `HermesPaths` is where every other such name already lives (`paths.rs:137-167`). |

Items 8 through 11 are the same defect seen from four sides: **the product measures its own memory behaviour and then ignores the measurement.** That is what M10b is.

## 2. M10a, as delivered

- **M10a.1 — the machine can be measured.** `crates/hermes-sys` is the
  workspace's only platform FFI: memory, CPU topology, disk and addresses on
  macOS and Windows, one `#[allow(unsafe_code)]`, one `SAFETY:` note per call,
  and nothing contributed to a Linux build so `hermes-system-info` keeps its
  `forbid(unsafe_code)`. `check.yml` runs `scripts/check.sh` — it does not
  reimplement it — on four runners.
- **M10a.2 — the Linux artifact stops disabling its own sandbox.** Both
  `--no-sandbox` sources closed; the one that cannot be configured away is
  answered by a refusal in code electron-builder does not write
  (`apps/desktop/src/sandbox.ts`). Flatpak becomes the primary Linux artifact
  because it stays sandboxed on a host without unprivileged user namespaces
  rather than merely refusing there.
- **M10a.3 — one artifact per platform.** `scripts-stage.mjs` stages the binary
  and refuses a version mismatch; universal DMG by `lipo`; `+crt-static` on
  Windows; `release.yml` builds each artifact on its own platform, runs it, and
  drafts a release whose notes say what Gatekeeper and SmartScreen will do.
- **M10a.4 — the Flatpak build stops failing before it starts.** Items 5, 6 and
  7 above, plus `scripts/test-flatpak.sh`, which asserts against an *installed
  bundle*: the sandbox grants that shipped, the wrapper going through zypak with
  no `--no-sandbox`, the packaged binary running inside the runtime, and — the
  highest-risk unknown — that an executable copied into `$XDG_DATA_HOME` inside
  the sandbox will run, because that is what the downloaded engine must do.

**What this machine cannot do, stated once.** `flatpak-builder` runs every build
step inside `bwrap`; here `bwrap` is not setuid,
`kernel.apparmor_restrict_unprivileged_userns = 1`, and no bwrap profile is
loaded, so `bwrap --ro-bind / / true` fails at the uid map. The DMG and the NSIS
installer cannot be built on Linux at all. Those three artifacts are proven by a
CI run or they are not proven.

## 3. M10b — three stages

**Turn a fit into a model, let the load paths ask for one, then say so on
screen.** Each stage is shippable alone and leaves the tree green.

### M10b.1 — a fit becomes a compute model

- **The policy lives in `hermes-bench`, beside the format it reads.**
  `hermes-bench` already depends on `hermes-memory`; the reverse edge would be a
  cycle, and putting the trust rules in the estimator would make the crate that
  must stay honest depend on the crate that produces numbers. A function from
  `(&Calibration, &MachineFingerprint, &EngineFingerprint, &ModelMetadata,
  RuntimeParams)` to `Option<ComputeModel>` is the whole surface.
- **A miss is a miss.** `Calibration::find` already refuses a machine or engine
  that does not match exactly, and `BucketKey` equality already covers
  architecture, quantization, context, slots and both cache types. M10b adds no
  fuzziness to either; what it adds is the refusal to *use* a match that is thin.
- **What makes a fit trustworthy**, and every one of these is a reason a fit is
  ignored rather than a reason to invent a number:
  - both `compute_bytes_per_ubatch` and `overhead_bytes` present — `regress`
    already returns `None` for both unless two *distinct* ubatch values were
    measured;
  - at least three points, so a straight line through two of them is not a
    calibration;
  - neither term negative, which happens when the weights were not all resident
    and is a measurement artefact rather than a coefficient;
  - the resulting model must not predict, for any point in the fit, less than
    that point's own residual. `max_residual_bytes` is already recorded as "a
    safe floor: an estimator that budgeted this much would have been right about
    every sample here", and it becomes exactly that.
- **The fit describes the engine, and only the engine.** `Prediction::exact()`
  is `weights + kv_cache` (`record.rs:307`) and `peak_rss` is the engine
  process's, so the residual is the engine's compute buffers plus its baseline.
  The intercept therefore becomes `engine_baseline`, and **`host_overhead` is
  never touched by a fit** — no benchmark has ever observed the Electron shell,
  and quietly calibrating a term nothing measured is the failure this whole
  design is built to avoid.
- **The slope is one equation and the compute term has two free coefficients.**
  `compute_bytes` is `vocab*ub*4 + activation*ub*embd*4 + scratch*ub*max(embd,ffn)*4`
  and `fit.rs` says in its own header why peak RSS cannot separate the last two.
  So the two are scaled together, their shipped 8:4 ratio held fixed, to
  reproduce the measured slope — and the doc comment says that the ratio is
  inherited, not measured. A slope below the exact logits term alone is not a
  smaller coefficient, it is a sign the run did not measure what it thought;
  that fit is refused.

### M10b.2 — the load paths ask for one

- **`HermesPaths::calibration_file()`**, so the name exists once (item 12).
- **The six construction sites take a calibrated model when one is available**,
  and the same `headless()`/`default()` model when one is not. The behaviour with
  no `calibration.json` — which is every machine until somebody runs
  `hermes bench --fit` — must be **byte-identical to today's**, and that is
  asserted rather than argued: the existing estimator tests do not move.
- **A calibrated estimate is never quietly smaller than an uncalibrated one
  would have been by more than the fit supports.** The floor from M10b.1 is what
  enforces it. The direction matters: an overestimate refuses a load the user can
  force, an underestimate invites the OOM killer, and `ComputeModel`'s own doc
  comment already says which way to err.
- **Reading a calibration must not be able to break a load.** An unreadable or
  unparsable file is already an error rather than a silent reset in
  `Calibration::load`; on the load path that error becomes "no calibration", is
  logged once, and the load proceeds on shipped defaults. A corrupt benchmark
  artefact must never cost somebody their model.

### M10b.3 — say which number it is

- **`Confidence::Measured` becomes reachable**, and only when a fit was actually
  used for that estimate.
- **The panel's permanent notice becomes conditional in fact** rather than in
  form (`Models.tsx:658`), and the measured case gets a sentence naming when the
  measurement was taken.
- **`hermes estimate` says the same thing in the same words** (`main.rs:777`),
  which is a string that has never once printed.
- **`/api/v1/gateway` reports whether a calibration is loaded and what it
  covers**, because a number that changes behaviour and cannot be seen from
  outside is the thing M6b.1 existed to stop.

## 4. What the measurements must decide

Nothing above sets a threshold by argument. Three questions, and the runs that
answer them, on this machine and against SmolLM2-135M as M9's sweep did:

1. **Is the residual affine in `n_ubatch` at all?** A sweep across at least four
   ubatch values in one bucket. If the points do not lie near a line, the fit
   format is describing something it cannot describe, and M10b.1's trust rules
   are the place that has to say so.
2. **How wrong are the shipped defaults?** The same run gives it directly:
   predicted compute plus baseline against the measured residual. This decides
   whether calibration is worth anything on this class of machine, and the answer
   is allowed to be "the defaults are already close", which would be a finding
   rather than a failure.
3. **Does the residual move with `n_ctx` and `n_parallel`?** `BucketKey` includes
   both, so a fit at 1024 tokens never applies at 4096 today. If the residual is
   flat across contexts, a later pass may widen the bucket — **but only that
   measurement may widen it**, and until it exists the narrow key stands.

The numbers, the run ids and the conclusion belong in `PROGRESS.md` when they
exist, next to M8's and M9's, and in the doc comment of any constant they set —
the rule `CORES_PER_SLOT` follows.

## 4b. The gap the matrix found, and what it costs calibration

`ResourceSnapshot` for the *engine process* - its resident set, its high-water
mark and its processor ticks - is read from `/proc/<pid>/status` and
`/proc/<pid>/stat`. M10a.1 gave the workspace macOS and Windows probes for the
**machine**; there is no equivalent yet for a **process**, and the first macOS
run of the matrix said so by failing
`a_dead_engine_is_reported_as_failed_and_cleared`.

The test states the per-platform truth now rather than skipping, so
implementing the reading elsewhere makes it fail until it is updated. What the
gap costs, in order of how much it matters:

1. **`hermes bench --fit` can fit nothing off Linux.** `fit_run` skips any
   sample without a `peak_rss`, so a run on macOS or Windows records timings
   and no residuals, and `calibration.json` stays empty. Everything in M10b
   still behaves correctly there - it reports `NoFit` and the shipped
   coefficients stand - but the milestone's whole point is unavailable on two
   of the three platforms it now ships to.
2. **Engine RSS and peak are absent from `/api/v1/metrics`**, and the two
   Prometheus gauges M7.2 added report nothing.
3. **A swap credits nothing**, because `anon_rss` is the credit and there is
   none to read. The safe direction: a swap is judged against memory the
   outgoing engine still holds, so it is refused more often rather than less.

Closing it is `proc_pid_rusage` on macOS - `ri_resident_size` and
`ri_lifetime_max_phys_footprint`, which is the peak - and
`GetProcessMemoryInfo` plus `GetProcessTimes` on Windows, both in
`crates/hermes-sys` beside the machine probes and behind the same single
`#[allow(unsafe_code)]`. Neither can be executed on the development machine,
so the matrix is what would prove it.

## 5. Deliberately left

- **Fitting `host_overhead`.** Nothing measures the shell. See M10b.1.
- **Separating the activation and scratch coefficients.** Collinear in peak RSS;
  it needs a second observable, not more samples. M8 said this and it is still
  true.
- **Sharing calibrations between machines.** The file is machine-scoped by
  design and `find` refuses a mismatch. A "close enough" fingerprint match is
  precisely how a calibration file becomes a confident wrong number.
- **Calibrating from production loads.** Tempting — every load has a peak RSS —
  and wrong for now: a benchmark controls the workload, and a fit taken from
  whatever the user happened to run is a fit against an unknown.
- **A frontend test runner.** Declined in M7, M8 and M9; declined again here for
  the same reason and no new one.

## 6. What must not be touched

The exact half of the estimate — weights and KV cache come from tensor shapes
and per-layer ggml geometry, and no fit may reach them; the direction of the
error, which is high; existing Prometheus metric names and label sets; the
`/health`, `/version`, `/props` and `/v1/models` bodies; the byte-exact golden
SSE files; `ALLOWED_KV_CACHE_TYPES`; the rule that nothing in a metric or a
benchmark record carries text; the `forbid(unsafe_code)` on every crate that has
it, `hermes-sys` remaining the only exception; and `scripts/check.sh` as the one
definition of the gate, which CI runs rather than reimplements.
