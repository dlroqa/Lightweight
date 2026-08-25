//! What the gateway measured, rather than what it hopes.
//!
//! Every number here is counted at the moment it happens, on the request path,
//! with atomics and no lock — a metric that costs a lock is a metric that
//! changes the thing it measures.
//!
//! Three rules shaped this module.
//!
//! **Nothing here may carry text.** Prompts, completions, tool arguments and
//! model paths are not counted, sampled or labelled. Metrics are the easiest
//! accidental route out for exactly the content section 26 protects, so the
//! types simply have nowhere to put it: every field is a number, and the only
//! strings are fixed label values known at compile time.
//!
//! **Timings come from the engine where the engine knows better.** Prefill and
//! decode are the engine's own measurements, forwarded as they are; the gateway
//! measures what only it can see — how long a request waited for a slot, and
//! how long the client waited for its first token, which includes that wait.
//! The two are reported separately rather than added together, because a slow
//! first token and a busy queue are different problems with different fixes.
//!
//! **One snapshot, two renderings.** [`MetricsSnapshot`] is taken once and
//! rendered as either Prometheus text or JSON, so the scrape surface and the
//! UI's surface cannot drift apart.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use hermes_inference::EngineCounters;
use hermes_inference::generation::FinishReason;
use serde::Serialize;

use crate::scheduler::{Band, QueueSnapshot};
use crate::system::Probed;
use hermes_core::units::Bytes;

/// Which endpoint served a request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Endpoint {
    ChatCompletions,
    Completions,
}

impl Endpoint {
    pub const ALL: [Self; 2] = [Self::ChatCompletions, Self::Completions];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ChatCompletions => "chat_completions",
            Self::Completions => "completions",
        }
    }

    const fn index(self) -> usize {
        match self {
            Self::ChatCompletions => 0,
            Self::Completions => 1,
        }
    }
}

/// How a request ended.
///
/// `Busy` is separated from the other client-visible failures on purpose: it is
/// the one that means *this gateway* could not keep up, and reading it as just
/// another 4xx would hide the only signal that the queue needs attention.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    ClientError,
    ServerError,
    Busy,
}

impl Outcome {
    pub const ALL: [Self; 4] = [Self::Ok, Self::ClientError, Self::ServerError, Self::Busy];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::ClientError => "client_error",
            Self::ServerError => "server_error",
            Self::Busy => "busy",
        }
    }

    const fn index(self) -> usize {
        match self {
            Self::Ok => 0,
            Self::ClientError => 1,
            Self::ServerError => 2,
            Self::Busy => 3,
        }
    }
}

/// What one finished generation cost.
///
/// Assembled by the request path as the generation runs and handed over once,
/// so a generation contributes to the counters exactly once no matter which way
/// it ended.
#[derive(Clone, Copy, Debug, Default)]
pub struct GenerationRecord {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub cached_tokens: u32,
    pub finish_reason: Option<FinishReason>,
    /// How long the request waited for a slot.
    pub queue_wait: Duration,
    /// Client-visible latency to the first token, queue wait included.
    pub time_to_first_token: Option<Duration>,
    /// The engine's own prefill time, when it reported one.
    pub prefill: Option<Duration>,
    /// The engine's own decode time, when it reported one.
    pub decode: Option<Duration>,
    /// Wall-clock from admission to the last byte.
    pub total: Duration,
    /// Which queue this request was classified into.
    ///
    /// `Option`, because the classification belongs to the request path and a
    /// record assembled without one is still a complete record of a
    /// generation. It stays `Copy`: a two-variant enum, not a label.
    pub band: Option<Band>,
}

/// Upper bounds of the latency buckets, in milliseconds.
///
/// Chosen for the machines this runs on rather than copied from a web service.
/// A ladder ending at ten seconds would put every prefill on a CPU without AVX
/// into the overflow bucket and measure nothing about it; the top of this one
/// is two minutes, which a long agentic prompt genuinely reaches. The cost of
/// the wide end is one `u64` per tally on a machine fast enough never to use
/// it.
///
/// Milliseconds because that is the unit `observe` is handed, so a bucket is
/// chosen by integer comparison and no float rounding decides which side of a
/// boundary an observation falls on.
pub const LATENCY_BOUNDS_MS: [u64; 11] = [
    50, 100, 250, 500, 1_000, 2_500, 5_000, 10_000, 30_000, 60_000, 120_000,
];

/// Buckets, plus the `+Inf` overflow that every histogram ends with.
const BUCKETS: usize = LATENCY_BOUNDS_MS.len() + 1;

/// A counter that also remembers its largest single observation, and the shape
/// of the observations it has seen.
///
/// The distribution is what a mean cannot say: on a gateway where one slow
/// request in fifty is the whole complaint, the mean moves by milliseconds
/// while the tail moves by minutes.
#[derive(Debug, Default)]
struct Tally {
    count: AtomicU64,
    total_ms: AtomicU64,
    max_ms: AtomicU64,
    /// Observations falling in each bucket, **not** cumulative.
    ///
    /// Cumulative counts are what Prometheus wants and what [`Self::read`]
    /// produces, but storing them that way would cost one atomic add per bucket
    /// on every observation instead of one in total. The accumulation is done
    /// once per scrape rather than once per request, on the side of the fence
    /// where nobody is waiting.
    buckets: [AtomicU64; BUCKETS],
}

impl Tally {
    fn observe(&self, value: Duration) {
        let ms = value.as_millis() as u64;
        self.count.fetch_add(1, Ordering::Relaxed);
        self.total_ms.fetch_add(ms, Ordering::Relaxed);
        self.max_ms.fetch_max(ms, Ordering::Relaxed);
        // The first bucket whose bound this observation does not exceed, or the
        // overflow when it exceeds them all.
        let index = LATENCY_BOUNDS_MS.partition_point(|bound| *bound < ms);
        self.buckets[index].fetch_add(1, Ordering::Relaxed);
    }

    fn read(&self) -> TallySnapshot {
        let mut buckets = [Bucket::default(); BUCKETS];
        let mut running = 0;
        for (index, bucket) in buckets.iter_mut().enumerate() {
            running += self.buckets[index].load(Ordering::Relaxed);
            *bucket = Bucket {
                le_ms: LATENCY_BOUNDS_MS.get(index).copied(),
                count: running,
            };
        }
        TallySnapshot {
            count: self.count.load(Ordering::Relaxed),
            total_ms: self.total_ms.load(Ordering::Relaxed),
            max_ms: self.max_ms.load(Ordering::Relaxed),
            buckets,
        }
    }
}

/// One cumulative histogram bucket.
///
/// `le_ms` is the bucket's upper bound and `None` is the `+Inf` bucket, so the
/// series describes itself: a reader does not have to hold a copy of
/// [`LATENCY_BOUNDS_MS`] to know what it is looking at, and the panel cannot
/// drift out of step with the gateway by keeping its own.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Bucket {
    pub le_ms: Option<u64>,
    pub count: u64,
}

/// A tally as read out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct TallySnapshot {
    pub count: u64,
    pub total_ms: u64,
    pub max_ms: u64,
    /// Cumulative counts: each entry is every observation at or below its
    /// bound, which is the convention Prometheus histograms are read with.
    pub buckets: [Bucket; BUCKETS],
}

impl Default for TallySnapshot {
    fn default() -> Self {
        let mut buckets = [Bucket::default(); BUCKETS];
        for (index, bucket) in buckets.iter_mut().enumerate() {
            bucket.le_ms = LATENCY_BOUNDS_MS.get(index).copied();
        }
        Self {
            count: 0,
            total_ms: 0,
            max_ms: 0,
            buckets,
        }
    }
}

impl TallySnapshot {
    /// The mean, or `None` when nothing has been observed.
    ///
    /// An average of no samples is not zero, and reporting it as zero is how a
    /// dashboard ends up showing a gateway that answers instantly because it
    /// has never answered at all.
    pub fn mean_ms(&self) -> Option<f64> {
        (self.count > 0).then(|| self.total_ms as f64 / self.count as f64)
    }

    /// The bound of the first bucket holding the given share of observations.
    ///
    /// A quantile read off buckets is a bound, not a value: "at least 95% of
    /// requests finished within this many milliseconds" is what the data
    /// supports, and interpolating inside a bucket would invent precision the
    /// counters never had. `None` when nothing has been observed, and `None`
    /// for the overflow bucket - "longer than the longest bound" is honest
    /// where a number would not be.
    pub fn quantile_ms(&self, share: f64) -> Option<u64> {
        if self.count == 0 {
            return None;
        }
        let target = (self.count as f64 * share).ceil() as u64;
        self.buckets
            .iter()
            .find(|bucket| bucket.count >= target)
            .and_then(|bucket| bucket.le_ms)
    }
}

/// One finished generation, as it is published to watchers.
///
/// The same facts the closing log line already carries, and no more: the
/// prompt, the completion and the API key are absent here exactly as they are
/// absent there. This is the record the Dashboard's live feed and the API
/// Gateway screen's recent-requests list are both drawn from — one stream,
/// because they are the same data twice.
#[derive(Clone, Debug, Serialize)]
pub struct RequestEvent {
    /// Milliseconds since the Unix epoch, so a client can place it on a clock
    /// without knowing when this process started.
    pub at_unix_ms: u64,
    pub id: Option<String>,
    pub model: Option<String>,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub cached_tokens: u32,
    /// `None` is a generation the client walked away from, never an error.
    pub finish_reason: Option<FinishReason>,
    pub queue_wait_ms: u64,
    pub time_to_first_token_ms: Option<u64>,
    pub total_ms: u64,
}

impl RequestEvent {
    /// Built from the measurements plus the two names only the caller knows.
    ///
    /// The names are passed in rather than carried on [`GenerationRecord`],
    /// which is `Copy` and is meant to stay that way: it is written to on the
    /// per-token path, and putting two heap allocations in it to serve one
    /// display feed would be paying for the feed in the hot loop.
    fn new(record: &GenerationRecord, id: Option<&str>, model: Option<&str>) -> Self {
        Self {
            at_unix_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|since| since.as_millis() as u64)
                .unwrap_or_default(),
            id: id.map(str::to_owned),
            model: model.map(str::to_owned),
            prompt_tokens: record.prompt_tokens,
            completion_tokens: record.completion_tokens,
            cached_tokens: record.cached_tokens,
            finish_reason: record.finish_reason,
            queue_wait_ms: record.queue_wait.as_millis() as u64,
            time_to_first_token_ms: record
                .time_to_first_token
                .map(|ttft| ttft.as_millis() as u64),
            total_ms: record.total.as_millis() as u64,
        }
    }
}

/// Decrements the in-flight gauge when dropped.
#[derive(Debug)]
pub struct InFlightGuard {
    metrics: Arc<Metrics>,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        // Saturating: a decrement below zero would wrap to `u64::MAX` and
        // report a gateway serving eighteen quintillion requests.
        let _ =
            self.metrics
                .in_flight
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                    Some(current.saturating_sub(1))
                });
    }
}

/// How many finished generations a slow watcher may fall behind before it is
/// told it missed some.
///
/// Small on purpose. This is a live feed, not a log: a client that cannot keep
/// up with sixty-four generations on a machine that produces one every several
/// seconds is not going to be rescued by a larger buffer, and the log file is
/// where the complete record already lives.
const EVENT_BACKLOG: usize = 64;

/// Every counter the gateway keeps.
#[derive(Debug)]
pub struct Metrics {
    started: Instant,
    /// Publishes each finished generation. Never awaited on the recording
    /// path: `send` on a broadcast channel does not block, and a send with no
    /// receivers is a no-op rather than an error worth reporting.
    events: tokio::sync::broadcast::Sender<RequestEvent>,
    /// HTTP requests being served this instant, across every endpoint.
    ///
    /// **Requests, not connections, and the difference is not pedantry.** With
    /// keep-alive one client holds one connection across many requests, and an
    /// idle client holds a connection while this reads zero. Counting
    /// connections would mean owning the accept loop, which `axum::serve` owns;
    /// counting requests is what this process can actually observe, so it is
    /// what is reported and what it is called.
    in_flight: AtomicU64,
    requests: [[AtomicU64; 4]; 2],
    generations: AtomicU64,
    finish_stop: AtomicU64,
    finish_length: AtomicU64,
    finish_tool_calls: AtomicU64,
    finish_error: AtomicU64,
    finish_cancelled: AtomicU64,
    prompt_tokens: AtomicU64,
    completion_tokens: AtomicU64,
    cached_tokens: AtomicU64,
    queue_wait: Tally,
    time_to_first_token: Tally,
    prefill: Tally,
    decode: Tally,
    /// Tokens decoded during the time `decode` accounts for.
    ///
    /// Kept alongside rather than divided on the spot: a rate averaged over
    /// requests weights a three-token reply the same as a thousand-token one,
    /// and the number anybody wants is total tokens over total time.
    decoded_tokens: AtomicU64,
    prefilled_tokens: AtomicU64,
    /// Generations and waiting, split by the band that decided who started.
    ///
    /// The distribution stays whole in `queue_wait` above; what is split here
    /// is the question the bands exist to answer - whether a short request
    /// actually gets through sooner than a long one, which a single average
    /// over both cannot show.
    band_generations: [AtomicU64; Band::ALL.len()],
    band_queue_wait: [Tally; Band::ALL.len()],
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Metrics {
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
            events: tokio::sync::broadcast::Sender::new(EVENT_BACKLOG),
            in_flight: AtomicU64::new(0),
            requests: Default::default(),
            generations: AtomicU64::new(0),
            finish_stop: AtomicU64::new(0),
            finish_length: AtomicU64::new(0),
            finish_tool_calls: AtomicU64::new(0),
            finish_error: AtomicU64::new(0),
            finish_cancelled: AtomicU64::new(0),
            prompt_tokens: AtomicU64::new(0),
            completion_tokens: AtomicU64::new(0),
            cached_tokens: AtomicU64::new(0),
            queue_wait: Tally::default(),
            time_to_first_token: Tally::default(),
            prefill: Tally::default(),
            decode: Tally::default(),
            decoded_tokens: AtomicU64::new(0),
            prefilled_tokens: AtomicU64::new(0),
            band_generations: Default::default(),
            band_queue_wait: Default::default(),
        }
    }

    /// Count one request's outcome.
    pub fn record_request(&self, endpoint: Endpoint, outcome: Outcome) {
        self.requests[endpoint.index()][outcome.index()].fetch_add(1, Ordering::Relaxed);
    }

    /// Count one finished generation.
    /// Watch finished generations as they happen.
    pub fn watch_requests(&self) -> tokio::sync::broadcast::Receiver<RequestEvent> {
        self.events.subscribe()
    }

    /// Count one request as being served until the returned guard is dropped.
    ///
    /// A guard rather than a pair of calls, so that a handler which returns
    /// early - every `401`, every `400`, every `?` - still decrements. A
    /// hand-balanced counter would drift upward on exactly the paths that are
    /// hardest to notice, and a gauge that only ever climbs is worse than none.
    pub fn enter_request(self: &Arc<Self>) -> InFlightGuard {
        self.in_flight.fetch_add(1, Ordering::Relaxed);
        InFlightGuard {
            metrics: Arc::clone(self),
        }
    }

    /// Requests being served this instant.
    pub fn in_flight(&self) -> u64 {
        self.in_flight.load(Ordering::Relaxed)
    }

    pub fn record_generation(&self, record: &GenerationRecord) {
        self.record_generation_as(record, None, None);
    }

    /// Record a generation, naming the completion and the model it served.
    ///
    /// Additive: [`Self::record_generation`] is unchanged for every caller that
    /// has nothing to add, and an unnamed generation still counts everywhere it
    /// counted before.
    pub fn record_generation_as(
        &self,
        record: &GenerationRecord,
        id: Option<&str>,
        model: Option<&str>,
    ) {
        // Published here rather than from the handlers, because this is the one
        // place every generation passes through - including the ones that ended
        // because the client walked away, which a publisher on the happy path
        // would miss and which are the ones worth watching.
        let _ = self.events.send(RequestEvent::new(record, id, model));

        self.generations.fetch_add(1, Ordering::Relaxed);
        match record.finish_reason {
            Some(FinishReason::Stop) => &self.finish_stop,
            Some(FinishReason::Length) => &self.finish_length,
            Some(FinishReason::ToolCalls) => &self.finish_tool_calls,
            Some(FinishReason::Error) => &self.finish_error,
            // A generation with no finish reason is one the client walked away
            // from. Counting it as an error would put a normal, deliberate act
            // — closing a laptop lid, pressing Ctrl-C — in the column an
            // operator reads as "the gateway is failing".
            None => &self.finish_cancelled,
        }
        .fetch_add(1, Ordering::Relaxed);

        self.prompt_tokens
            .fetch_add(u64::from(record.prompt_tokens), Ordering::Relaxed);
        self.completion_tokens
            .fetch_add(u64::from(record.completion_tokens), Ordering::Relaxed);
        self.cached_tokens
            .fetch_add(u64::from(record.cached_tokens), Ordering::Relaxed);

        self.queue_wait.observe(record.queue_wait);
        if let Some(band) = record.band {
            self.band_generations[band.index()].fetch_add(1, Ordering::Relaxed);
            self.band_queue_wait[band.index()].observe(record.queue_wait);
        }
        if let Some(ttft) = record.time_to_first_token {
            self.time_to_first_token.observe(ttft);
        }
        if let Some(prefill) = record.prefill {
            self.prefill.observe(prefill);
            // Only the tokens the engine actually processed: a cached prefix
            // was not prefilled, and counting it would report a prefill rate
            // that rises with cache hits rather than with speed.
            self.prefilled_tokens.fetch_add(
                u64::from(record.prompt_tokens.saturating_sub(record.cached_tokens)),
                Ordering::Relaxed,
            );
        }
        if let Some(decode) = record.decode {
            self.decode.observe(decode);
            self.decoded_tokens
                .fetch_add(u64::from(record.completion_tokens), Ordering::Relaxed);
        }
    }

    /// Read every counter at once.
    pub fn snapshot(
        &self,
        queue: QueueSnapshot,
        model: Option<ModelSnapshot>,
        bands: BandSnapshot,
        engine: Probed<EngineMemory>,
        engine_cpu: Probed<EngineCpu>,
        engine_counters: Probed<EngineCounters>,
    ) -> MetricsSnapshot {
        let mut requests = Vec::with_capacity(Endpoint::ALL.len() * Outcome::ALL.len());
        for endpoint in Endpoint::ALL {
            for outcome in Outcome::ALL {
                requests.push(RequestCount {
                    endpoint: endpoint.as_str(),
                    outcome: outcome.as_str(),
                    count: self.requests[endpoint.index()][outcome.index()].load(Ordering::Relaxed),
                });
            }
        }
        MetricsSnapshot {
            uptime_seconds: self.started.elapsed().as_secs(),
            in_flight: self.in_flight.load(Ordering::Relaxed),
            requests,
            generations: self.generations.load(Ordering::Relaxed),
            finish_reasons: FinishReasonCounts {
                stop: self.finish_stop.load(Ordering::Relaxed),
                length: self.finish_length.load(Ordering::Relaxed),
                tool_calls: self.finish_tool_calls.load(Ordering::Relaxed),
                error: self.finish_error.load(Ordering::Relaxed),
                cancelled: self.finish_cancelled.load(Ordering::Relaxed),
            },
            tokens: TokenCounts {
                prompt: self.prompt_tokens.load(Ordering::Relaxed),
                completion: self.completion_tokens.load(Ordering::Relaxed),
                cached: self.cached_tokens.load(Ordering::Relaxed),
                prefilled: self.prefilled_tokens.load(Ordering::Relaxed),
                decoded: self.decoded_tokens.load(Ordering::Relaxed),
            },
            bands_served: Band::ALL.map(|band| BandCount {
                band: band.as_str(),
                generations: self.band_generations[band.index()].load(Ordering::Relaxed),
                queue_wait: self.band_queue_wait[band.index()].read(),
            }),
            queue_wait: self.queue_wait.read(),
            time_to_first_token: self.time_to_first_token.read(),
            prefill: self.prefill.read(),
            decode: self.decode.read(),
            queue,
            model,
            bands,
            engine,
            engine_cpu,
            engine_counters,
        }
    }
}

/// What one band was served.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct BandCount {
    pub band: &'static str,
    pub generations: u64,
    pub queue_wait: TallySnapshot,
}

/// One cell of the request matrix.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct RequestCount {
    pub endpoint: &'static str,
    pub outcome: &'static str,
    pub count: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct FinishReasonCounts {
    pub stop: u64,
    pub length: u64,
    pub tool_calls: u64,
    pub error: u64,
    /// Generations whose client disconnected before they finished.
    pub cancelled: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct TokenCounts {
    pub prompt: u64,
    pub completion: u64,
    /// Prompt tokens served from the engine's prefix cache.
    pub cached: u64,
    /// Prompt tokens the engine actually had to process.
    pub prefilled: u64,
    pub decoded: u64,
}

/// What is loaded, for a scrape that wants to know what these numbers describe.
///
/// The model's *id*, which is the name the gateway advertises — never its path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ModelSnapshot {
    pub id: String,
    pub n_ctx: u32,
}

/// Everything, read at one instant.
#[derive(Clone, Debug, Serialize)]
pub struct MetricsSnapshot {
    pub uptime_seconds: u64,
    /// HTTP requests being served at the instant this was read. See
    /// [`Metrics::in_flight`] for why this is not a count of clients.
    pub in_flight: u64,
    pub requests: Vec<RequestCount>,
    pub generations: u64,
    pub finish_reasons: FinishReasonCounts,
    pub tokens: TokenCounts,
    /// What each band was actually served, as opposed to what it was promised.
    pub bands_served: [BandCount; Band::ALL.len()],
    pub queue_wait: TallySnapshot,
    pub time_to_first_token: TallySnapshot,
    pub prefill: TallySnapshot,
    pub decode: TallySnapshot,
    pub queue: QueueSnapshot,
    pub model: Option<ModelSnapshot>,
    /// The interactive band's ceilings as they stand right now.
    ///
    /// Derived from the context of the model that is loaded, so they change
    /// when a model is swapped. Exposed because they are the scheduler's whole
    /// policy: without them, "why did that request queue behind this one?" is
    /// unanswerable from outside.
    pub bands: BandSnapshot,
    /// What the engine process is holding.
    ///
    /// `Probed` rather than `Option`, because "there is no engine" and "this
    /// platform cannot read one" are different answers and the panel shows
    /// them differently. The backend trait collapses both into `Ok(None)`, so
    /// the gateway tells them apart from the engine's health rather than by
    /// changing the trait.
    pub engine: Probed<EngineMemory>,
    /// What the engine process has been spending the processor on.
    ///
    /// Read from the same single probe as `engine`, so a scrape costs one pass
    /// over `/proc` rather than two. It carries its own `Probed` state because
    /// it can be absent while `engine` is present: the memory fields and the
    /// processor-time fields live in different files, and only one of them may
    /// be readable.
    pub engine_cpu: Probed<EngineCpu>,
    /// What the engine reports about its own work.
    ///
    /// Scraped from the engine's private Prometheus endpoint, which has been
    /// enabled since M2 and read by nothing. Only counters the gateway cannot
    /// compute for itself appear here; everything it already measures is
    /// measured once.
    pub engine_counters: Probed<EngineCounters>,
}

/// Processor time the engine process has been charged for.
///
/// The counterpart to [`EngineMemory`], and the number that was missing while
/// the product's entire premise was CPU inference: `rss` says how much of the
/// machine the engine is holding, and this says how much of it the engine is
/// *using*.
///
/// Ticks, unconverted, exactly as `/proc/<pid>/stat` publishes them and for the
/// same reason `hermes_system_info::load` publishes `/proc/stat` unconverted —
/// a rate needs two readings and an interval, so this crate publishes the
/// counter and lets the caller difference it rather than inventing a percentage
/// from one sample.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct EngineCpu {
    pub user_ticks: u64,
    pub system_ticks: u64,
}

/// What the engine process is holding, right now.
///
/// A reading rather than a rate, which is why it can be answered honestly from
/// a single pull: `rss` is a level, and a level at an instant is a fact. See
/// [`hermes_system_info::load`] for why the figures that *are* rates are not
/// turned into percentages here.
///
/// `anon_rss` is separated out because it is the only part a model swap may
/// spend: the weights are mmapped, so the rest is file-backed page cache the
/// kernel already counts as available.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct EngineMemory {
    pub rss: Bytes,
    /// High-water mark since the engine started. The number a `Coarse`
    /// estimate is checked against.
    pub peak_rss: Bytes,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub anon_rss: Option<Bytes>,
}

/// The ceilings that decide which band a request lands in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct BandSnapshot {
    pub interactive_prompt_tokens: u32,
    pub interactive_output_tokens: u32,
}

impl MetricsSnapshot {
    /// Prefill throughput over every request that reported one.
    pub fn prefill_tokens_per_second(&self) -> Option<f64> {
        rate(self.tokens.prefilled, self.prefill.total_ms)
    }

    /// Decode throughput over every request that reported one.
    ///
    /// The number that tells a user what this machine is: on the box this was
    /// built on it is under one token per second, and on a laptop with AVX2 it
    /// is several times that. Reported rather than assumed, so nobody has to
    /// take a benchmark's word for it.
    pub fn decode_tokens_per_second(&self) -> Option<f64> {
        rate(self.tokens.decoded, self.decode.total_ms)
    }

    /// Render as Prometheus text exposition format.
    ///
    /// Hand-written rather than pulled from a crate: the format is a dozen
    /// lines of `writeln!`, and the dependency policy in the workspace manifest
    /// exists precisely so that a convenience like that is weighed rather than
    /// assumed.
    pub fn to_prometheus(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::with_capacity(2048);

        header(
            &mut out,
            "hermes_uptime_seconds",
            "Seconds since the gateway started serving.",
            "gauge",
        );
        let _ = writeln!(out, "hermes_uptime_seconds {}", self.uptime_seconds);

        header(
            &mut out,
            "hermes_requests_total",
            "Requests by endpoint and outcome.",
            "counter",
        );
        for row in &self.requests {
            let _ = writeln!(
                out,
                "hermes_requests_total{{endpoint=\"{}\",outcome=\"{}\"}} {}",
                row.endpoint, row.outcome, row.count
            );
        }

        header(
            &mut out,
            "hermes_generations_total",
            "Generations that reached a finish reason.",
            "counter",
        );
        let _ = writeln!(out, "hermes_generations_total {}", self.generations);

        header(
            &mut out,
            "hermes_finish_reason_total",
            "Generations by the reason they stopped.",
            "counter",
        );
        for (reason, count) in [
            ("stop", self.finish_reasons.stop),
            ("length", self.finish_reasons.length),
            ("tool_calls", self.finish_reasons.tool_calls),
            ("error", self.finish_reasons.error),
            ("cancelled", self.finish_reasons.cancelled),
        ] {
            let _ = writeln!(
                out,
                "hermes_finish_reason_total{{reason=\"{reason}\"}} {count}"
            );
        }

        header(
            &mut out,
            "hermes_tokens_total",
            "Tokens by kind.",
            "counter",
        );
        for (kind, count) in [
            ("prompt", self.tokens.prompt),
            ("completion", self.tokens.completion),
            ("cached", self.tokens.cached),
            ("prefilled", self.tokens.prefilled),
            ("decoded", self.tokens.decoded),
        ] {
            let _ = writeln!(out, "hermes_tokens_total{{kind=\"{kind}\"}} {count}");
        }

        for (name, help, tally) in [
            (
                "hermes_queue_wait",
                "Time a request spent waiting for a slot.",
                self.queue_wait,
            ),
            (
                "hermes_time_to_first_token",
                "Client-visible latency to the first token, queue wait included.",
                self.time_to_first_token,
            ),
            (
                "hermes_prefill",
                "Engine-reported prompt processing time.",
                self.prefill,
            ),
            (
                "hermes_decode",
                "Engine-reported token generation time.",
                self.decode,
            ),
        ] {
            // A histogram in the format's own terms: one `# TYPE` for the
            // family, then `_bucket`, `_sum` and `_count` under it. The two
            // series that existed before keep their names and their values -
            // what changes is that `histogram_quantile()` now has buckets to
            // work with, so a p95 stops being unanswerable.
            header(&mut out, &format!("{name}_seconds"), help, "histogram");
            for bucket in &tally.buckets {
                match bucket.le_ms {
                    Some(le_ms) => {
                        let _ = writeln!(
                            out,
                            "{name}_seconds_bucket{{le=\"{:.3}\"}} {}",
                            le_ms as f64 / 1000.0,
                            bucket.count
                        );
                    }
                    None => {
                        let _ =
                            writeln!(out, "{name}_seconds_bucket{{le=\"+Inf\"}} {}", bucket.count);
                    }
                }
            }
            let _ = writeln!(
                out,
                "{name}_seconds_sum {:.3}",
                tally.total_ms as f64 / 1000.0
            );
            let _ = writeln!(out, "{name}_seconds_count {}", tally.count);
            // Not part of the histogram convention, and kept because the
            // convention has no place for it: the longest single wait is the
            // number an operator asks for first, and it cannot be recovered
            // from buckets.
            header(&mut out, &format!("{name}_seconds_max"), help, "gauge");
            let _ = writeln!(
                out,
                "{name}_seconds_max {:.3}",
                tally.max_ms as f64 / 1000.0
            );
        }

        header(
            &mut out,
            "hermes_queue_slots",
            "Engine slots, in use and configured.",
            "gauge",
        );
        let _ = writeln!(
            out,
            "hermes_queue_slots{{state=\"running\"}} {}",
            self.queue.running
        );
        let _ = writeln!(
            out,
            "hermes_queue_slots{{state=\"capacity\"}} {}",
            self.queue.capacity
        );

        header(
            &mut out,
            "hermes_queue_waiting",
            "Requests queued for a slot, by band.",
            "gauge",
        );
        let _ = writeln!(
            out,
            "hermes_queue_waiting{{band=\"interactive\"}} {}",
            self.queue.waiting_interactive
        );
        let _ = writeln!(
            out,
            "hermes_queue_waiting{{band=\"bulk\"}} {}",
            self.queue.waiting_bulk
        );

        header(
            &mut out,
            "hermes_queue_events_total",
            "Scheduler decisions, by kind.",
            "counter",
        );
        for (kind, count) in [
            ("admitted_immediately", self.queue.admitted_immediately),
            ("queued", self.queue.queued),
            ("timed_out", self.queue.timed_out),
            ("abandoned", self.queue.abandoned),
            ("overtakes", self.queue.overtakes),
        ] {
            let _ = writeln!(out, "hermes_queue_events_total{{kind=\"{kind}\"}} {count}");
        }

        if let Some(rate) = self.prefill_tokens_per_second() {
            header(
                &mut out,
                "hermes_prefill_tokens_per_second",
                "Prompt tokens processed per second, over every request that reported a timing.",
                "gauge",
            );
            let _ = writeln!(out, "hermes_prefill_tokens_per_second {rate:.3}");
        }
        if let Some(rate) = self.decode_tokens_per_second() {
            header(
                &mut out,
                "hermes_decode_tokens_per_second",
                "Tokens generated per second, over every request that reported a timing.",
                "gauge",
            );
            let _ = writeln!(out, "hermes_decode_tokens_per_second {rate:.3}");
        }

        if let Some(model) = &self.model {
            header(
                &mut out,
                "hermes_model_context_length",
                "The context the loaded model is being served with.",
                "gauge",
            );
            let _ = writeln!(
                out,
                "hermes_model_context_length{{model=\"{}\"}} {}",
                escape_label(&model.id),
                model.n_ctx
            );
        }

        // Emitted only when they were read. A scraper seeing no series knows
        // nothing was measured; a scraper seeing zero would record an engine
        // holding no memory, which is never true of a running one.
        if let Probed::Read { reading } = &self.engine {
            header(
                &mut out,
                "hermes_engine_resident_bytes",
                "Resident set of the engine process.",
                "gauge",
            );
            let _ = writeln!(out, "hermes_engine_resident_bytes {}", reading.rss.get());
            header(
                &mut out,
                "hermes_engine_peak_resident_bytes",
                "High-water mark of the engine process since it started.",
                "gauge",
            );
            let _ = writeln!(
                out,
                "hermes_engine_peak_resident_bytes {}",
                reading.peak_rss.get()
            );
        }

        // Ticks rather than seconds, because `USER_HZ` is not this process's to
        // guess at and a scraper computing `rate()` over a counter does not
        // care what the unit is - only that it is constant, which it is. The
        // HELP line names it so nobody has to find that out from the numbers.
        if let Probed::Read { reading } = &self.engine_cpu {
            header(
                &mut out,
                "hermes_engine_cpu_ticks_total",
                "Processor time charged to the engine process, in kernel clock ticks (USER_HZ).",
                "counter",
            );
            let _ = writeln!(
                out,
                "hermes_engine_cpu_ticks_total{{mode=\"user\"}} {}",
                reading.user_ticks
            );
            let _ = writeln!(
                out,
                "hermes_engine_cpu_ticks_total{{mode=\"system\"}} {}",
                reading.system_ticks
            );
        }

        // Carried in the JSON snapshot since M5 and rendered nowhere, which
        // made the scrape and the panel disagree about what the gateway knows.
        // Measured at the moment a slot is granted, which is why it is a
        // separate series from `hermes_queue_wait_seconds`: that one is
        // recorded by the request as it finishes, and a request that gave up
        // never reaches it.
        // What the bands were actually served. `hermes_generations_total` is
        // deliberately left unlabelled - one metric name cannot be both
        // labelled and not - so the split is its own series.
        // Absent when the engine did not report them, present when it did, and
        // never a zero standing in for either.
        if let Probed::Read { reading } = &self.engine_counters {
            if let Some(tokens) = reading.max_sequence_tokens {
                header(
                    &mut out,
                    "hermes_engine_max_sequence_tokens",
                    "Longest sequence the engine has served, prompt and generation together.",
                    "gauge",
                );
                let _ = writeln!(out, "hermes_engine_max_sequence_tokens {tokens}");
            }
            if let Some(calls) = reading.decode_calls {
                header(
                    &mut out,
                    "hermes_engine_decode_calls_total",
                    "Decode steps the engine has run.",
                    "counter",
                );
                let _ = writeln!(out, "hermes_engine_decode_calls_total {calls}");
            }
            if let Some(slots) = reading.busy_slots_per_decode {
                header(
                    &mut out,
                    "hermes_engine_busy_slots_per_decode",
                    "Average slots kept busy per decode step.",
                    "gauge",
                );
                let _ = writeln!(out, "hermes_engine_busy_slots_per_decode {slots:.3}");
            }
            if let Some(deferred) = reading.requests_deferred {
                header(
                    &mut out,
                    "hermes_engine_requests_deferred",
                    "Requests the engine has put aside for lack of a free slot.",
                    "gauge",
                );
                let _ = writeln!(out, "hermes_engine_requests_deferred {deferred}");
            }
        }

        header(
            &mut out,
            "hermes_band_generations_total",
            "Generations that reached a finish reason, by the band they were served in.",
            "counter",
        );
        for row in &self.bands_served {
            let _ = writeln!(
                out,
                "hermes_band_generations_total{{band=\"{}\"}} {}",
                row.band, row.generations
            );
        }
        header(
            &mut out,
            "hermes_band_queue_wait_seconds_sum",
            "Time spent waiting for a slot, by band.",
            "counter",
        );
        for row in &self.bands_served {
            let _ = writeln!(
                out,
                "hermes_band_queue_wait_seconds_sum{{band=\"{}\"}} {:.3}",
                row.band,
                row.queue_wait.total_ms as f64 / 1000.0
            );
        }
        header(
            &mut out,
            "hermes_band_queue_wait_seconds_count",
            "Requests that waited for a slot, by band.",
            "counter",
        );
        for row in &self.bands_served {
            let _ = writeln!(
                out,
                "hermes_band_queue_wait_seconds_count{{band=\"{}\"}} {}",
                row.band, row.queue_wait.count
            );
        }

        header(
            &mut out,
            "hermes_queue_grant_wait_seconds_sum",
            "Time waited by requests that were granted a slot, measured at the grant.",
            "counter",
        );
        let _ = writeln!(
            out,
            "hermes_queue_grant_wait_seconds_sum {:.3}",
            self.queue.wait_ms_total as f64 / 1000.0
        );
        header(
            &mut out,
            "hermes_queue_grant_wait_seconds_max",
            "Longest wait by a request that was granted a slot.",
            "gauge",
        );
        let _ = writeln!(
            out,
            "hermes_queue_grant_wait_seconds_max {:.3}",
            self.queue.wait_ms_max as f64 / 1000.0
        );

        header(
            &mut out,
            "hermes_band_ceiling_tokens",
            "Largest prompt and stated output budget still counted as interactive.",
            "gauge",
        );
        let _ = writeln!(
            out,
            "hermes_band_ceiling_tokens{{kind=\"prompt\"}} {}",
            self.bands.interactive_prompt_tokens
        );
        let _ = writeln!(
            out,
            "hermes_band_ceiling_tokens{{kind=\"output\"}} {}",
            self.bands.interactive_output_tokens
        );

        out
    }
}

/// Write one metric's `HELP` and `TYPE` preamble.
fn header(out: &mut String, name: &str, help: &str, kind: &str) {
    use std::fmt::Write as _;
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} {kind}");
}

/// Tokens per second, or `None` when there is nothing to divide.
fn rate(tokens: u64, total_ms: u64) -> Option<f64> {
    (total_ms > 0 && tokens > 0).then(|| tokens as f64 * 1000.0 / total_ms as f64)
}

/// Escape a Prometheus label value.
///
/// A model id is ours to choose, but it ends up inside quotes in a text format
/// where a stray backslash or quote makes the whole scrape unparseable — the
/// kind of failure that shows up as "monitoring is down" long after the model
/// was renamed.
fn escape_label(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            other => escaped.push(other),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three engine readings a test with no engine has.
    fn no_engine<T>() -> crate::system::Probed<T> {
        crate::system::Probed::Unavailable {
            code: "no_engine_running",
            message: "no engine".to_owned(),
        }
    }

    fn record(queue_wait: Duration, band: Band) -> GenerationRecord {
        GenerationRecord {
            queue_wait,
            band: Some(band),
            ..GenerationRecord::default()
        }
    }

    #[test]
    fn an_observation_lands_in_the_first_bucket_it_does_not_exceed() {
        let tally = Tally::default();
        tally.observe(Duration::from_millis(50));
        tally.observe(Duration::from_millis(51));
        let read = tally.read();

        // Cumulative: 50ms is at the bound, so it counts in `le=50` and in
        // every wider bucket. 51ms misses the first and joins from `le=100`.
        assert_eq!(read.buckets[0].le_ms, Some(50));
        assert_eq!(read.buckets[0].count, 1);
        assert_eq!(read.buckets[1].le_ms, Some(100));
        assert_eq!(read.buckets[1].count, 2);
        assert_eq!(read.buckets.last().expect("overflow bucket").le_ms, None);
        assert_eq!(read.buckets.last().expect("overflow bucket").count, 2);
    }

    #[test]
    fn an_observation_past_every_bound_reaches_only_the_overflow() {
        let tally = Tally::default();
        tally.observe(Duration::from_secs(600));
        let read = tally.read();
        for bucket in read.buckets.iter().filter(|bucket| bucket.le_ms.is_some()) {
            assert_eq!(bucket.count, 0, "a ten-minute wait is under no bound");
        }
        assert_eq!(read.buckets.last().expect("overflow").count, 1);
        assert_eq!(read.count, 1);
    }

    #[test]
    fn a_quantile_is_a_bound_and_never_an_invented_value() {
        let tally = Tally::default();
        // Nineteen fast requests and one slow one: the mean is dragged around
        // by the slow one, and the p95 is what actually names it.
        for _ in 0..19 {
            tally.observe(Duration::from_millis(40));
        }
        tally.observe(Duration::from_secs(45));
        let read = tally.read();

        assert_eq!(read.quantile_ms(0.5), Some(50));
        assert_eq!(read.quantile_ms(0.95), Some(50));
        assert_eq!(read.quantile_ms(0.99), Some(60_000));
        // The mean says two and a quarter seconds, which describes no request
        // that was ever served.
        assert_eq!(read.mean_ms(), Some(2_288.0));
    }

    #[test]
    fn a_quantile_of_nothing_is_not_zero() {
        // Zero would report a gateway that answers instantly because it has
        // never answered at all - the same trap `mean_ms` avoids.
        assert_eq!(TallySnapshot::default().quantile_ms(0.95), None);
    }

    #[test]
    fn generations_are_counted_against_the_band_that_admitted_them() {
        let metrics = Metrics::new();
        metrics.record_generation(&record(Duration::from_millis(10), Band::Interactive));
        metrics.record_generation(&record(Duration::from_millis(4_000), Band::Bulk));
        metrics.record_generation(&record(Duration::from_millis(20), Band::Interactive));

        let snapshot = metrics.snapshot(
            QueueSnapshot::default(),
            None,
            BandSnapshot::default(),
            no_engine(),
            no_engine(),
            no_engine(),
        );

        let interactive = snapshot
            .bands_served
            .iter()
            .find(|row| row.band == "interactive")
            .expect("interactive row");
        let bulk = snapshot
            .bands_served
            .iter()
            .find(|row| row.band == "bulk")
            .expect("bulk row");
        assert_eq!(interactive.generations, 2);
        assert_eq!(bulk.generations, 1);
        // The point of the split: averaged together these two waits describe
        // neither band.
        assert_eq!(interactive.queue_wait.max_ms, 20);
        assert_eq!(bulk.queue_wait.max_ms, 4_000);
        assert_eq!(snapshot.queue_wait.count, 3);
    }

    #[test]
    fn a_generation_with_no_band_still_counts_everywhere_else() {
        // `record_generation` has callers that never classified anything, and
        // an unclassified generation must not vanish from the totals.
        let metrics = Metrics::new();
        metrics.record_generation(&GenerationRecord::default());
        let snapshot = metrics.snapshot(
            QueueSnapshot::default(),
            None,
            BandSnapshot::default(),
            no_engine(),
            no_engine(),
            no_engine(),
        );
        assert_eq!(snapshot.generations, 1);
        assert!(snapshot.bands_served.iter().all(|row| row.generations == 0));
    }
}
