//! Which request runs next.
//!
//! The engine serves one generation at a time, so "who goes next" is the whole
//! of the scheduling problem — and on this hardware one turn can take minutes.
//! A queue that is merely first-come-first-served turns that into the failure
//! this module exists to prevent, which was observed rather than imagined: an
//! agent harness sent a 20-token title generation alongside a 5,596-token turn,
//! the small request queued behind the large one, and the harness's own timeout
//! fired before it ever started.
//!
//! Three rules, in order.
//!
//! 1. **Bands are earned, never claimed.** A request is classified from numbers
//!    the gateway has already measured — the prompt the engine counted, and the
//!    output budget the client asked for — never from anything the client says
//!    about its own importance. A priority that can be requested is a priority
//!    every caller requests.
//! 2. **A short request may overtake a long one.** That is the point.
//! 3. **Nobody waits forever.** Once a request has waited past the starvation
//!    ceiling it can no longer be overtaken, so being long costs a bounded
//!    delay rather than becoming a denial.
//!
//! What this module deliberately does not do is **preempt**. A band decides who
//! *starts* next; nothing here stops a generation already running, because the
//! engine cannot pause one and restarting it would discard the prefill that
//! dominates the cost. The consequence is worth stating plainly: a short
//! request still waits for the current turn to finish. What it no longer does
//! is wait for every turn queued ahead of it.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use axum::extract::ConnectInfo;

use tokio::sync::{Notify, oneshot};
use tokio::time::Instant;

/// Which caller a request came from, for scheduling and for nothing else.
///
/// **Observed, never claimed.** The address comes off the connection, where the
/// kernel put it; there is no header to set and no field to send. That is the
/// same standard the bands are held to — a priority that can be requested is a
/// priority every caller requests — applied to identity instead of to cost.
///
/// The IP alone, never the port. A port is unique per connection, so keying on
/// one would give a client that opens four connections four identities and one
/// that reuses a connection a single identity for a hundred requests, which is
/// the opposite of what fairness needs. Canonicalised, so a caller arriving on
/// a dual-stack listener as `::ffff:127.0.0.1` is not a second client.
///
/// `None` is a request that arrived with no connection information at all,
/// which is every in-process test and every future transport that is not a
/// socket. They share one key — the degenerate single-client case, whose order
/// is exactly the order this scheduler served before there were clients in it.
///
/// **It is never logged, never a metric label, never serialized and never
/// stored.** It lives in the queue for as long as a request is waiting and goes
/// away with it. The `Debug` here is what keeps that true when somebody prints
/// the queue while debugging something else.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct PeerKey(Option<IpAddr>);

impl std::fmt::Debug for PeerKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self.0 {
            Some(_) => "PeerKey(<peer>)",
            None => "PeerKey(local)",
        })
    }
}

impl PeerKey {
    /// The key for a request that arrived over a socket.
    pub fn from_socket(address: SocketAddr) -> Self {
        Self(Some(address.ip().to_canonical()))
    }

    /// Whether the peer is this machine: loopback, or a transport with no peer
    /// at all (a test's `oneshot`), which is only ever local.
    ///
    /// This is the one question the control surface asks — whether a
    /// state-changing request came from the machine running the gateway — and
    /// it is answered without exposing the address the rest of this type
    /// deliberately keeps to itself.
    pub fn is_local(&self) -> bool {
        self.0.is_none_or(|ip| ip.is_loopback())
    }

    /// The key for a request, resolving a trusted proxy's forwarded client IP.
    ///
    /// When a trusted proxy (a Cloudflare Tunnel, say) fronts a loopback bind,
    /// every request arrives from `127.0.0.1` and its real origin is in
    /// `CF-Connecting-IP`. Honouring that header is what lets the scheduler tell
    /// remote clients apart again and, more importantly, what stops a remote
    /// caller from looking `is_local()` and reaching the control surface.
    ///
    /// The header is trusted **only** when `trust` is on *and* the socket peer
    /// is loopback. A remote peer can never present a loopback socket address,
    /// and Cloudflare overwrites any `CF-Connecting-IP` a client tries to send,
    /// so this cannot be spoofed from off the machine. With trust off, or from a
    /// non-loopback peer, the header is ignored and the socket address stands —
    /// exactly as before this existed.
    pub fn resolve(
        socket: Option<SocketAddr>,
        cf_connecting_ip: Option<&str>,
        trust: bool,
    ) -> Self {
        if trust
            && socket.is_some_and(|address| address.ip().is_loopback())
            && let Some(ip) = cf_connecting_ip.and_then(|value| value.trim().parse::<IpAddr>().ok())
        {
            return Self(Some(ip.to_canonical()));
        }
        match socket {
            Some(address) => Self::from_socket(address),
            None => Self::default(),
        }
    }
}

impl<S: Send + Sync> axum::extract::FromRequestParts<S> for PeerKey {
    /// Extraction cannot fail, deliberately.
    ///
    /// `ConnectInfo` itself rejects with a 500 when the server was built
    /// without `into_make_service_with_connect_info`, which would make adding
    /// this extractor to a handler a new way for a request to fail. A router
    /// assembled without connection information — a `tower::oneshot` in a test,
    /// or a transport that has no peer — degrades to the shared local key
    /// instead, and serves requests in the order it always did.
    type Rejection = Infallible;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        let socket = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|ConnectInfo(address)| *address);
        // Present, and true, only when the gateway was started behind a trusted
        // proxy; `app` inserts it, so a router assembled without it (a test's
        // `oneshot`) simply never trusts the header.
        let trust = parts
            .extensions
            .get::<crate::TrustForwarded>()
            .is_some_and(|forwarded| forwarded.0);
        let forwarded = parts
            .headers
            .get("cf-connecting-ip")
            .and_then(|value| value.to_str().ok());
        Ok(Self::resolve(socket, forwarded, trust))
    }
}

/// How much work a request is allowed to be and still count as interactive.
///
/// Both numbers are ceilings, and a request has to satisfy both.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BandLimits {
    /// The largest counted prompt an interactive request may carry.
    pub prompt_tokens: u32,
    /// The largest output budget an interactive request may *ask for*.
    ///
    /// A request that names no budget is not judged by this: the gateway
    /// clamps an unset `max_tokens` to the whole remaining window, and holding
    /// that against a client which simply omitted the field would put almost
    /// every request in the slow band. An unstated budget is missing evidence,
    /// not evidence of a long request.
    pub output_tokens: u32,
}

impl BandLimits {
    /// Ceilings derived from the context the model was actually loaded with.
    ///
    /// Fractions of the window rather than constants, because the window is
    /// itself derived from the machine: on a box where 2048 tokens is all that
    /// fits, a 1024-token prompt is half the context and nothing like
    /// interactive, while on a workstation serving 32K it is a rounding error.
    /// The caps stop a very large context from calling a genuinely expensive
    /// prompt small.
    ///
    /// **The floors come from a real client rather than from roundness.** The
    /// auxiliary request this scheduler exists for asks for exactly 64 output
    /// tokens (`agent/title_generator.py:408`), while the agent turn it must
    /// not queue behind asks for 65536 (`agent/run_agent.py:1673`). A ceiling
    /// derived purely as a fraction of the window lands on exactly 64 at a
    /// 2048-token context — so the one request the band exists for would
    /// classify correctly by a single token there, and wrongly on any smaller
    /// window. The floors are twice the observed value, which is the side to be
    /// wrong on: a ceiling that is too high costs some medium request its place
    /// in the fast band and is bounded by the starvation ceiling anyway, while
    /// one that is too low means the feature never fires at all.
    pub fn for_context(n_ctx: u32) -> Self {
        Self {
            prompt_tokens: (n_ctx / 8).clamp(512, 1024),
            output_tokens: (n_ctx / 32).clamp(128, 256),
        }
    }
}

impl Default for BandLimits {
    fn default() -> Self {
        Self {
            prompt_tokens: 1024,
            output_tokens: 256,
        }
    }
}

/// What a request is scheduled as.
///
/// Two bands, because two is what the evidence supports: a short request must
/// not wait behind a long one. A third band would be a number invented here
/// rather than measured.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Band {
    /// Small prompt, small stated budget: seconds of work, and something is
    /// usually blocking on it.
    Interactive,
    /// Everything else. The default, because an unclassified request is more
    /// safely assumed expensive than cheap.
    #[default]
    Bulk,
}

impl Band {
    /// Classify from what has already been measured.
    ///
    /// `prompt_tokens` is the engine's own count with the chat template and
    /// tool declarations applied, and `requested_max_tokens` is what the client
    /// asked for *before* the gateway clamped it — see [`BandLimits`].
    pub fn classify(
        prompt_tokens: u32,
        requested_max_tokens: Option<u32>,
        limits: BandLimits,
    ) -> Self {
        if prompt_tokens > limits.prompt_tokens {
            return Self::Bulk;
        }
        match requested_max_tokens {
            Some(budget) if budget > limits.output_tokens => Self::Bulk,
            _ => Self::Interactive,
        }
    }

    /// The wire and log spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Interactive => "interactive",
            Self::Bulk => "bulk",
        }
    }

    /// Ordering tier: lower runs first.
    const fn tier(self) -> u8 {
        match self {
            Self::Interactive => 1,
            Self::Bulk => 2,
        }
    }

    /// Every band, for iterating counters.
    pub const ALL: [Self; 2] = [Self::Interactive, Self::Bulk];

    /// Position in a fixed-size counter array.
    ///
    /// Deliberately not `as usize` on the enum: the discriminants are an
    /// implementation detail and reordering the variants must not silently
    /// swap two bands' counters, which is a wrong number nobody would notice.
    pub(crate) const fn index(self) -> usize {
        match self {
            Self::Interactive => 0,
            Self::Bulk => 1,
        }
    }
}

/// How the scheduler behaves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SchedulerConfig {
    /// Where the interactive band ends.
    pub interactive: BandLimits,
    /// How long a request may be overtaken before it stops being overtakeable.
    ///
    /// This is the whole of the starvation guarantee. Sixty seconds is chosen
    /// against the measured cost of a turn on the slowest machine this targets
    /// — roughly 26 s quiet, 45-50 s under contention — so a bulk request gives
    /// up its place to at most one or two short ones before it becomes
    /// untouchable.
    pub starvation_ceiling: Duration,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            interactive: BandLimits::default(),
            starvation_ceiling: Duration::from_secs(60),
        }
    }
}

/// Counters the scheduler keeps about itself.
#[derive(Debug, Default)]
struct Counters {
    admitted_immediately: AtomicU64,
    queued: AtomicU64,
    timed_out: AtomicU64,
    abandoned: AtomicU64,
    overtakes: AtomicU64,
    wait_ms_total: AtomicU64,
    wait_ms_max: AtomicU64,
}

/// One request holding a slot, as a reader outside this module sees it.
///
/// Everything here is already published elsewhere - the completion id in the
/// response, the model on `/v1/models`, the band in the metrics - so the roster
/// adds a view rather than a new kind of disclosure. What it deliberately does
/// not carry is the caller's address: that is a scheduling key, and this is a
/// display.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct RunningRequest {
    /// The completion id the client was given, when the slot has been
    /// described. Absent for a slot held by something with no completion - a
    /// benchmark, most obviously.
    pub id: Option<String>,
    pub model: Option<String>,
    pub band: &'static str,
    /// How long this request has held its slot.
    pub running_ms: u64,
    /// The prompt the engine counted, when the slot has been described.
    pub prompt_tokens: u32,
}

/// One request still waiting, as a reader outside this module sees it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct WaitingRequest {
    /// The scheduler's own ticket number.
    ///
    /// Not a completion id: a queued request does not have one yet, because it
    /// has not been given to the engine.
    pub ticket: u64,
    pub band: &'static str,
    pub waited_ms: u64,
    /// How many requests would be served before this one.
    pub position: u32,
}

/// Who is running and who is waiting, at one instant.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Roster {
    pub capacity: u32,
    pub running: Vec<RunningRequest>,
    pub waiting: Vec<WaitingRequest>,
}

/// A point-in-time reading of the queue.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct QueueSnapshot {
    pub capacity: u32,
    pub running: u32,
    pub waiting: u32,
    pub waiting_interactive: u32,
    pub waiting_bulk: u32,
    /// Requests that took a slot without waiting at all.
    pub admitted_immediately: u64,
    /// Requests that had to queue.
    pub queued: u64,
    /// Requests that gave up because the queue timeout ran out.
    pub timed_out: u64,
    /// Requests whose client disconnected while they were still queued.
    pub abandoned: u64,
    /// How many times a waiting request was passed over by a later arrival.
    ///
    /// The direct measure of what the bands are doing. Zero on a gateway that
    /// is never contended, and a number that should stay far below `queued` on
    /// one that is — if it approaches it, the ceilings are wrong.
    pub overtakes: u64,
    pub wait_ms_total: u64,
    pub wait_ms_max: u64,
}

/// One request waiting for a slot.
#[derive(Debug)]
struct Waiter {
    id: u64,
    band: Band,
    /// Which caller this request came from. Never leaves this module.
    peer: PeerKey,
    /// This request's turn in its own caller's rotation.
    ///
    /// The number of requests that caller already had waiting when this one
    /// arrived: its first queued request is round 0, its second is round 1.
    /// Fixed at enqueue and never renumbered — renumbering as requests depart
    /// would make every grant linear in the queue *and* would let a caller's
    /// place improve as its own earlier requests are served, which is exactly
    /// the advantage this exists to remove.
    round: u32,
    enqueued: Instant,
    /// Signals that a slot is now this waiter's, and which slot it is.
    ///
    /// The slot's id rather than the permit itself, deliberately: handing an
    /// owned permit through the channel would mean a failed send returns it to
    /// a caller that is holding the queue lock, and dropping it there would
    /// re-enter the release path against a lock it already owns. An id is a
    /// number, and the sender - which still holds the lock - simply removes
    /// the entry again if nobody was there to receive it.
    grant: oneshot::Sender<u64>,
}

/// One request that holds a slot right now.
///
/// The roster and the running count are the same collection, so they cannot
/// disagree: a slot is held by exactly one [`SlotPermit`], and a permit is
/// exactly one entry here.
#[derive(Debug)]
struct Running {
    /// The permit's own id, so a permit can find its entry to describe it or
    /// to remove it. Not the completion id, which a request does not have
    /// until after it has been admitted.
    id: u64,
    band: Band,
    started: Instant,
    /// The completion id and model, once the request path has said what this
    /// slot is being used for. Absent for a slot taken by something with no
    /// such names - a benchmark, or a test.
    id_and_model: Option<(String, String)>,
    prompt_tokens: u32,
}

#[derive(Debug, Default)]
struct Inner {
    running: Vec<Running>,
    waiting: VecDeque<Waiter>,
    next_id: u64,
    /// While paused, no new request starts.
    ///
    /// Requests still *queue* — a client that arrives during a model swap is
    /// told it is waiting, exactly as it would be behind a long generation,
    /// rather than being refused for something that will be over in seconds.
    paused: bool,
}

/// Admission control for the engine's slots.
#[derive(Debug)]
pub struct Scheduler {
    /// Slots this gateway hands out.
    ///
    /// Atomic rather than fixed because it follows the engine: the slot count
    /// is derived from the machine *and* from the model being loaded, so a
    /// swap to a model whose caches no longer fit must lower it. Inheriting it
    /// across a swap would be the M5 band-ceiling failure in a new place —
    /// a number that is correct for the model it was computed for and wrong
    /// for the one actually running.
    capacity: AtomicUsize,
    config: SchedulerConfig,
    /// The interactive ceilings, live.
    ///
    /// Separate from `config` because they change when a different model is
    /// loaded: the ceilings are derived from the context the engine is actually
    /// running, and a swapped-in model with a different window must not be
    /// judged by the previous one's numbers. Atomics rather than a lock so that
    /// reading them on the request path costs nothing and cannot deadlock
    /// against the queue lock.
    interactive_prompt_tokens: AtomicU32,
    interactive_output_tokens: AtomicU32,
    inner: Mutex<Inner>,
    counters: Counters,
    /// Signalled when the last running request finishes.
    drained: Notify,
}

impl Scheduler {
    pub fn new(capacity: u32, config: SchedulerConfig) -> Arc<Self> {
        Arc::new(Self {
            capacity: AtomicUsize::new(capacity.max(1) as usize),
            config,
            interactive_prompt_tokens: AtomicU32::new(config.interactive.prompt_tokens),
            interactive_output_tokens: AtomicU32::new(config.interactive.output_tokens),
            inner: Mutex::new(Inner::default()),
            counters: Counters::default(),
            drained: Notify::new(),
        })
    }

    pub fn config(&self) -> SchedulerConfig {
        self.config
    }

    /// The ceilings a request is classified against right now.
    pub fn band_limits(&self) -> BandLimits {
        BandLimits {
            prompt_tokens: self.interactive_prompt_tokens.load(Ordering::Relaxed),
            output_tokens: self.interactive_output_tokens.load(Ordering::Relaxed),
        }
    }

    /// Slots this gateway is handing out right now.
    pub fn capacity(&self) -> u32 {
        self.capacity.load(Ordering::Relaxed) as u32
    }

    /// Set the slot count for a newly loaded engine.
    ///
    /// Raising it hands the new slots to whoever is already waiting, exactly as
    /// lifting a pause does — otherwise they would sit in the queue until the
    /// next release, in front of an engine with idle slots. Lowering it
    /// preempts nothing: what is running finishes, and no further slot is
    /// handed out until the count is back under the new limit.
    pub fn set_capacity(self: &Arc<Self>, capacity: u32) {
        let capacity = capacity.max(1) as usize;
        self.capacity.store(capacity, Ordering::Relaxed);
        let mut inner = self.lock();
        if inner.paused {
            // The pause will hand them out when it lifts, in one place.
            return;
        }
        let now = Instant::now();
        while inner.running.len() < capacity {
            let Some(waiter) = self.take_next(&mut inner, now) else {
                break;
            };
            self.hand_over(&mut inner, waiter);
        }
    }

    /// Re-derive the ceilings for a newly loaded model.
    ///
    /// Called on every successful load. Skipping it would serve a new model
    /// under the previous model's ceilings — which is the failure M5 paid to
    /// find, in a new place: limits that are correct by construction and wrong
    /// for the context actually being served.
    pub fn set_band_limits(&self, limits: BandLimits) {
        self.interactive_prompt_tokens
            .store(limits.prompt_tokens, Ordering::Relaxed);
        self.interactive_output_tokens
            .store(limits.output_tokens, Ordering::Relaxed);
    }

    /// Stop admitting requests until the returned guard is dropped.
    ///
    /// What a model swap needs: nothing new starts, what is already running is
    /// left to finish, and the queue keeps its order so that whoever was next
    /// is still next afterwards. Nothing is preempted — that remains true, and
    /// cannot change until the engine can pause a generation.
    pub fn pause(self: &Arc<Self>) -> PauseGuard {
        self.lock().paused = true;
        PauseGuard {
            scheduler: Arc::clone(self),
        }
    }

    /// Wait until nothing is running, for at most `timeout`.
    ///
    /// Returns whether the engine actually went idle. A caller that gets
    /// `false` has a generation still in flight and must decide what to do
    /// about it rather than swapping the model out from under it.
    pub async fn drain(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.lock().running.is_empty() {
                return true;
            }
            // Armed before the second check so a release that lands between the
            // two is not a lost wakeup.
            let waiting = self.drained.notified();
            if self.lock().running.is_empty() {
                return true;
            }
            if tokio::time::timeout_at(deadline, waiting).await.is_err() {
                return self.lock().running.is_empty();
            }
        }
    }

    /// Whether new requests are currently being held.
    pub fn is_paused(&self) -> bool {
        self.lock().paused
    }

    /// Lock the queue, tolerating a poisoned mutex.
    ///
    /// A panic while the queue was locked must not take the gateway with it —
    /// section 27 — and the invariant this guards is a `VecDeque` and two
    /// counters, which cannot be left half-written by an unwind.
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Take a slot if one is free right now.
    ///
    /// The uncontended path, and the common one: no channel, no queue entry,
    /// no wakeup. A gateway serving one client at a time never touches the rest
    /// of this module.
    pub fn try_admit(self: &Arc<Self>) -> Option<SlotPermit> {
        let mut inner = self.lock();
        // No band to state, so the slot is recorded in the default one until
        // whoever holds it says otherwise. Every caller on the request path
        // goes through `admit_or_enqueue`, which knows the band already.
        self.take_free_slot(&mut inner, Band::default())
    }

    /// Take a slot, or join the queue — deciding under one lock.
    ///
    /// The two used to be separate calls with a gap between them, and a slot
    /// released inside that gap found an empty queue, went idle, and left the
    /// request that was on its way in to wait for the *next* release. At
    /// capacity one that is a client waiting out the whole queue timeout in
    /// front of a gateway doing nothing. Deciding under one lock closes it:
    /// either the slot was free when we looked, or we were in the queue before
    /// it could be released.
    pub fn admit_or_enqueue(
        self: &Arc<Self>,
        band: Band,
        peer: PeerKey,
    ) -> Result<SlotPermit, Ticket> {
        let mut inner = self.lock();
        if let Some(permit) = self.take_free_slot(&mut inner, band) {
            return Ok(permit);
        }
        Err(self.push_waiter(&mut inner, band, peer))
    }

    /// Join the queue.
    ///
    /// Returns a [`Ticket`], which can report its position while it waits and
    /// which releases its place when dropped — including when it is dropped by
    /// a client disconnecting, which is the only reason this is a ticket rather
    /// than a plain future.
    pub fn enqueue(self: &Arc<Self>, band: Band, peer: PeerKey) -> Ticket {
        let mut inner = self.lock();
        self.push_waiter(&mut inner, band, peer)
    }

    /// Take a slot if the queue's state allows it, under a lock already held.
    ///
    /// The `waiting.is_empty()` clause is the anti-barging rule, and it is what
    /// gives the round in [`rank`] its meaning: without it a fresh arrival
    /// would take a free slot ahead of a queue it never joined, and no ordering
    /// among the waiters could matter.
    fn take_free_slot(self: &Arc<Self>, inner: &mut Inner, band: Band) -> Option<SlotPermit> {
        if !inner.paused
            && inner.running.len() < self.capacity() as usize
            && inner.waiting.is_empty()
        {
            self.counters
                .admitted_immediately
                .fetch_add(1, Ordering::Relaxed);
            return Some(self.start_running(inner, band));
        }
        None
    }

    /// Record a slot as taken and hand back the permit that holds it.
    ///
    /// The only place a running entry is created, as `release` is the only
    /// place one is removed - which is what keeps the roster and the count of
    /// busy slots the same fact rather than two facts that agree by habit.
    fn start_running(self: &Arc<Self>, inner: &mut Inner, band: Band) -> SlotPermit {
        let id = self.reserve_running(inner, band);
        SlotPermit::new(Arc::clone(self), id)
    }

    /// Record a slot as taken, returning the entry's id.
    fn reserve_running(&self, inner: &mut Inner, band: Band) -> u64 {
        inner.next_id += 1;
        let id = inner.next_id;
        inner.running.push(Running {
            id,
            band,
            started: Instant::now(),
            id_and_model: None,
            prompt_tokens: 0,
        });
        id
    }

    /// Add a waiter, under a lock already held.
    fn push_waiter(self: &Arc<Self>, inner: &mut Inner, band: Band, peer: PeerKey) -> Ticket {
        let (grant, granted) = oneshot::channel();
        inner.next_id += 1;
        let id = inner.next_id;
        let enqueued = Instant::now();
        // This caller's turn in the rotation, counted rather than tracked. A
        // map of per-peer counters would be O(1) instead of this scan, and
        // would have to be evicted — an un-evicted map keyed by address is
        // exactly the identity-at-rest this design promises not to keep.
        let round = inner
            .waiting
            .iter()
            .filter(|waiter| waiter.peer == peer)
            .count() as u32;
        inner.waiting.push_back(Waiter {
            id,
            band,
            peer,
            round,
            enqueued,
            grant,
        });
        self.counters.queued.fetch_add(1, Ordering::Relaxed);
        Ticket {
            id,
            band,
            enqueued,
            scheduler: Arc::clone(self),
            granted,
            taken: false,
            withdrawal: Withdrawal::Abandoned,
        }
    }

    /// Take a slot, waiting up to `timeout` for one.
    ///
    /// The plain path for a caller that has nothing to say while it waits. A
    /// streamed request uses [`Scheduler::enqueue`] instead, so it can tell the
    /// client where it is in the queue.
    pub async fn admit(
        self: &Arc<Self>,
        band: Band,
        peer: PeerKey,
        timeout: Duration,
    ) -> Result<SlotPermit, Queued> {
        let mut ticket = match self.admit_or_enqueue(band, peer) {
            Ok(permit) => return Ok(permit),
            Err(ticket) => ticket,
        };
        match tokio::time::timeout(timeout, ticket.granted()).await {
            Ok(Some(permit)) => Ok(permit),
            Ok(None) => Err(Queued::Closed),
            Err(_) => {
                // Counted by the ticket's own departure rather than here.
                // Counting it in both places is how `timed_out` and
                // `abandoned` came to describe overlapping sets of requests.
                ticket.timed_out();
                Err(Queued::TimedOut)
            }
        }
    }

    /// What the queue looks like now.
    pub fn snapshot(&self) -> QueueSnapshot {
        let inner = self.lock();
        let waiting_interactive = inner
            .waiting
            .iter()
            .filter(|waiter| waiter.band == Band::Interactive)
            .count();
        QueueSnapshot {
            capacity: self.capacity(),
            running: inner.running.len() as u32,
            waiting: inner.waiting.len() as u32,
            waiting_interactive: waiting_interactive as u32,
            waiting_bulk: (inner.waiting.len() - waiting_interactive) as u32,
            admitted_immediately: self.counters.admitted_immediately.load(Ordering::Relaxed),
            queued: self.counters.queued.load(Ordering::Relaxed),
            timed_out: self.counters.timed_out.load(Ordering::Relaxed),
            abandoned: self.counters.abandoned.load(Ordering::Relaxed),
            overtakes: self.counters.overtakes.load(Ordering::Relaxed),
            wait_ms_total: self.counters.wait_ms_total.load(Ordering::Relaxed),
            wait_ms_max: self.counters.wait_ms_max.load(Ordering::Relaxed),
        }
    }

    /// Who holds a slot and who is queued, at this instant.
    ///
    /// Taken under the same lock as the queue itself, so the roster and the
    /// counts in [`Scheduler::snapshot`] cannot describe two different moments.
    pub fn roster(&self) -> Roster {
        let inner = self.lock();
        let now = Instant::now();
        let ceiling = self.config.starvation_ceiling;
        let mut waiting: Vec<WaitingRequest> = inner
            .waiting
            .iter()
            .map(|waiter| WaitingRequest {
                ticket: waiter.id,
                band: waiter.band.as_str(),
                waited_ms: now.saturating_duration_since(waiter.enqueued).as_millis() as u64,
                // The same key the scheduler picks by, so the roster cannot
                // contradict the order actually served.
                position: inner
                    .waiting
                    .iter()
                    .filter(|other| rank(other, now, ceiling) < rank(waiter, now, ceiling))
                    .count() as u32,
            })
            .collect();
        waiting.sort_by_key(|entry| entry.position);
        Roster {
            capacity: self.capacity(),
            running: inner
                .running
                .iter()
                .map(|running| RunningRequest {
                    id: running.id_and_model.as_ref().map(|(id, _)| id.clone()),
                    model: running
                        .id_and_model
                        .as_ref()
                        .map(|(_, model)| model.clone()),
                    band: running.band.as_str(),
                    running_ms: now.saturating_duration_since(running.started).as_millis() as u64,
                    prompt_tokens: running.prompt_tokens,
                })
                .collect(),
            waiting,
        }
    }

    /// Give a waiter the slot, and say whether it took it.
    ///
    /// The entry is made before the grant is sent and removed again if the
    /// send fails, so a slot is never recorded as running for a client that
    /// has gone. What travels through the channel is the entry's id rather
    /// than a permit: sending an owned permit would mean a failed send returns
    /// it to a caller holding the queue lock, and dropping it there would
    /// re-enter the release path against a lock it already owns.
    fn hand_over(&self, inner: &mut Inner, waiter: Waiter) -> bool {
        let id = self.reserve_running(inner, waiter.band);
        if waiter.grant.send(id).is_ok() {
            return true;
        }
        if let Some(index) = inner.running.iter().position(|running| running.id == id) {
            inner.running.remove(index);
        }
        self.counters.abandoned.fetch_add(1, Ordering::Relaxed);
        false
    }

    /// Give the slot back and hand it to whoever should have it next.
    ///
    /// Called from [`SlotPermit`]'s `Drop`, which is the only place a slot is
    /// ever released — there is no path that returns a permit by any other
    /// route, and therefore no path that leaks one.
    fn release(&self, id: u64) {
        let mut inner = self.lock();
        if let Some(index) = inner.running.iter().position(|running| running.id == id) {
            inner.running.remove(index);
        }
        if inner.running.is_empty() {
            // Tell a swap that the engine is now idle. Notifying under the lock
            // is deliberate: the waiter re-checks `running` after being woken,
            // so it cannot observe a stale zero.
            self.drained.notify_waiters();
        }
        if inner.paused {
            // The freed slot stays free. Whoever is queued keeps their place
            // and is granted it when the pause lifts.
            return;
        }
        let now = Instant::now();
        // A grant whose receiver has already gone (a client that disconnected
        // in the microseconds between being picked and being told) must not
        // consume the slot: try the next waiter instead, until one takes it.
        while let Some(waiter) = self.take_next(&mut inner, now) {
            if self.hand_over(&mut inner, waiter) {
                return;
            }
        }
    }

    /// Pop the waiter that should run next, counting who it passed.
    ///
    /// The ordering is one comparison: aged-out requests first, then band, then
    /// arrival. Reading it as a key rather than as branches is what keeps
    /// [`Ticket::position`] and this function from ever disagreeing.
    fn take_next(&self, inner: &mut Inner, now: Instant) -> Option<Waiter> {
        let ceiling = self.config.starvation_ceiling;
        let index = (0..inner.waiting.len()).min_by_key(|&i| {
            let waiter = &inner.waiting[i];
            rank(waiter, now, ceiling)
        })?;
        let chosen_at = inner.waiting[index].enqueued;
        // Everyone who arrived earlier and did not get the slot was overtaken.
        let overtaken = inner
            .waiting
            .iter()
            .filter(|waiter| waiter.enqueued < chosen_at)
            .count();
        if overtaken > 0 {
            self.counters
                .overtakes
                .fetch_add(overtaken as u64, Ordering::Relaxed);
        }
        let waited = now.saturating_duration_since(chosen_at).as_millis() as u64;
        self.counters
            .wait_ms_total
            .fetch_add(waited, Ordering::Relaxed);
        self.counters
            .wait_ms_max
            .fetch_max(waited, Ordering::Relaxed);
        inner.waiting.remove(index)
    }

    /// Start admitting again, and hand out every slot that is now free.
    ///
    /// Up to `capacity` grants rather than one, because a pause can end with
    /// several slots idle and several requests queued; `release` only ever
    /// frees one.
    fn resume(&self) {
        let mut inner = self.lock();
        inner.paused = false;
        let now = Instant::now();
        while inner.running.len() < self.capacity() as usize {
            let Some(waiter) = self.take_next(&mut inner, now) else {
                break;
            };
            self.hand_over(&mut inner, waiter);
        }
    }

    /// Drop a waiter that gave up, and say whether it had already been granted.
    ///
    /// Exactly one counter moves, and which one is the caller's to say: a
    /// request whose wait ran out and a client that closed its laptop are
    /// different facts with different remedies, and they used to be counted as
    /// both or as the wrong one depending on which code path arrived here.
    fn withdraw(&self, id: u64, reason: Withdrawal) -> bool {
        let mut inner = self.lock();
        if let Some(index) = inner.waiting.iter().position(|waiter| waiter.id == id) {
            inner.waiting.remove(index);
            match reason {
                Withdrawal::TimedOut => self.counters.timed_out.fetch_add(1, Ordering::Relaxed),
                Withdrawal::Abandoned => self.counters.abandoned.fetch_add(1, Ordering::Relaxed),
            };
            return true;
        }
        false
    }
}

/// Holds the scheduler paused for as long as it lives.
///
/// A guard rather than a pair of calls so that a swap which fails half way —
/// an engine that will not start, a load that is refused for memory — cannot
/// leave the gateway permanently refusing to admit anything. The `?` that
/// returns early drops this, and admission comes back.
#[derive(Debug)]
pub struct PauseGuard {
    scheduler: Arc<Scheduler>,
}

impl Drop for PauseGuard {
    fn drop(&mut self) {
        self.scheduler.resume();
    }
}

/// The sort key that decides who runs next. Smaller wins.
///
/// Four parts, in order: the starvation guarantee, the band, the caller's turn
/// in the rotation, and arrival order.
///
/// Tier 0 is the starvation guarantee: once a request has waited past the
/// ceiling it sorts ahead of every band, and among aged-out requests the
/// longest wait wins. **The round is zeroed there too**, or an aged-out request
/// could still be overtaken by a fresher aged-out one from a quieter caller,
/// and the guarantee would hold only within a caller.
///
/// The round is what makes the queue fair between callers rather than only
/// between requests. Each caller's first waiting request competes with every
/// other caller's first; its second waits behind every other caller's first. A
/// client that sends twenty requests no longer puts twenty of them in front of
/// somebody else's one — it puts one in front and nineteen behind.
///
/// **With a single caller this is exactly the key it replaces.** Every waiter
/// then shares a peer, so within a band the rounds run 0, 1, 2… in arrival
/// order, and ordering by round is ordering by arrival. That is asserted by a
/// test rather than argued here, because it is the property that makes this
/// safe to add.
fn rank(waiter: &Waiter, now: Instant, ceiling: Duration) -> (u8, u32, Instant) {
    let aged_out = now.saturating_duration_since(waiter.enqueued) >= ceiling;
    let tier = if aged_out { 0 } else { waiter.band.tier() };
    let round = if aged_out { 0 } else { waiter.round };
    (tier, round, waiter.enqueued)
}

/// Why a request did not get a slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Queued {
    /// The wait ran out. The caller turns this into a 503.
    TimedOut,
    /// The gateway is shutting down.
    Closed,
}

/// A place in the queue.
///
/// Exists so that a waiting request can be *told* it is waiting. Dropping it
/// leaves the queue — which is what happens when a client disconnects, and the
/// reason this cannot be a bare future.
#[derive(Debug)]
pub struct Ticket {
    id: u64,
    band: Band,
    enqueued: Instant,
    scheduler: Arc<Scheduler>,
    granted: oneshot::Receiver<u64>,
    taken: bool,
    /// What to count if this ticket leaves the queue without a slot.
    ///
    /// Abandonment is the default because it is what a dropped ticket means
    /// when nobody said otherwise: the future holding it went away, which is a
    /// client that went away. A timeout says so explicitly.
    withdrawal: Withdrawal,
}

/// Why a waiter left the queue without a slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Withdrawal {
    /// The wait ran out and the gateway gave up on it.
    TimedOut,
    /// The client went away while it was still waiting.
    Abandoned,
}

impl Ticket {
    pub fn band(&self) -> Band {
        self.band
    }

    /// How long this request has been waiting.
    pub fn waited(&self) -> Duration {
        self.enqueued.elapsed()
    }

    /// Record that this request gave up because its wait ran out.
    ///
    /// Called by both timeout paths before the ticket is dropped. Without it a
    /// timeout is indistinguishable from a disconnect, and the streamed and
    /// non-streamed timeouts — which are the same event reported two ways —
    /// counted differently from each other.
    pub fn timed_out(&mut self) {
        self.withdrawal = Withdrawal::TimedOut;
    }

    /// How many requests would be served before this one.
    ///
    /// `Some(0)` is next. `None` is not in the queue at all — already granted,
    /// or withdrawn — which used to be reported as `Some(0)` and is a different
    /// fact: one says "you are about to run", the other says "you are not
    /// waiting for anything".
    ///
    /// Computed with the same key the scheduler picks by, so a reported
    /// position cannot contradict the order actually served — though it is a
    /// snapshot, and a later arrival in a higher band can still move it.
    ///
    /// It counts requests, not slots. With four slots free, "three ahead of
    /// you" still means three ahead of you; all four then start together. The
    /// alternative — reporting a place among those about to start — would need
    /// the client to know the capacity, and the capacity is ours.
    pub fn position(&self) -> Option<u32> {
        let inner = self.scheduler.lock();
        let now = Instant::now();
        let ceiling = self.scheduler.config.starvation_ceiling;
        let mine = inner
            .waiting
            .iter()
            .find(|waiter| waiter.id == self.id)
            .map(|waiter| rank(waiter, now, ceiling))?;
        Some(
            inner
                .waiting
                .iter()
                .filter(|waiter| rank(waiter, now, ceiling) < mine)
                .count() as u32,
        )
    }

    /// Wait until this request's turn.
    ///
    /// `None` when the scheduler went away, which only happens at shutdown.
    pub async fn granted(&mut self) -> Option<SlotPermit> {
        match (&mut self.granted).await {
            Ok(slot) => {
                // Set before returning and with no await in between, so a
                // future dropped at this point cannot lose the slot: either the
                // permit exists, or `Drop` still finds the grant unclaimed.
                self.taken = true;
                Some(SlotPermit::new(Arc::clone(&self.scheduler), slot))
            }
            Err(_) => None,
        }
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        if self.taken {
            // The permit is out in the world and will release itself.
            return;
        }
        if self.scheduler.withdraw(self.id, self.withdrawal) {
            return;
        }
        // Not in the queue and no permit taken: a slot was granted to a ticket
        // that went away before claiming it. Nothing else knows about that
        // slot, so it has to be given back here or the gateway loses it for the
        // life of the process.
        if let Ok(slot) = self.granted.try_recv() {
            self.scheduler.release(slot);
        }
    }
}

/// The right to run one request.
///
/// Releasing on `Drop` is the entire contract: the response body owns this
/// through [`crate::stream::RequestGuard`], hyper drops the body when the
/// client disconnects, and the slot comes back without anything having to
/// detect the disconnect.
#[derive(Debug)]
pub struct SlotPermit {
    scheduler: Arc<Scheduler>,
    /// Which running entry this permit holds.
    slot: u64,
}

impl SlotPermit {
    fn new(scheduler: Arc<Scheduler>, slot: u64) -> Self {
        Self { scheduler, slot }
    }

    /// Say what this slot is being used for, for the roster.
    ///
    /// Called once the request path knows the completion id, the model and the
    /// prompt it counted - all of which are decided after admission. A slot
    /// nobody describes still appears in the roster with its band and its
    /// elapsed time, which is what a benchmark or a test looks like.
    pub fn describe(&self, id: impl Into<String>, model: impl Into<String>, prompt_tokens: u32) {
        let mut inner = self.scheduler.lock();
        if let Some(running) = inner
            .running
            .iter_mut()
            .find(|running| running.id == self.slot)
        {
            running.id_and_model = Some((id.into(), model.into()));
            running.prompt_tokens = prompt_tokens;
        }
    }

    /// Record which band this slot was granted for.
    pub fn in_band(&self, band: Band) {
        let mut inner = self.scheduler.lock();
        if let Some(running) = inner
            .running
            .iter_mut()
            .find(|running| running.id == self.slot)
        {
            running.band = band;
        }
    }
}

impl Drop for SlotPermit {
    fn drop(&mut self) {
        self.scheduler.release(self.slot);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_peer_is_local_only_when_loopback_or_absent() {
        use std::net::{Ipv4Addr, SocketAddr};
        // No socket at all (a test oneshot, a peerless transport): local.
        assert!(PeerKey::default().is_local());
        // A loopback socket, including a second loopback address: local.
        assert!(PeerKey::from_socket(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 9)).is_local());
        assert!(
            PeerKey::from_socket(SocketAddr::new(Ipv4Addr::new(127, 0, 0, 2).into(), 9)).is_local()
        );
        // A documentation-range address stands in for a real remote peer.
        assert!(
            !PeerKey::from_socket(SocketAddr::new(Ipv4Addr::new(192, 0, 2, 10).into(), 9))
                .is_local()
        );
    }

    #[test]
    fn a_trusted_loopback_peer_takes_its_ip_from_the_forwarded_header() {
        use std::net::{Ipv4Addr, SocketAddr};
        // Behind a tunnel every request arrives from loopback and the real
        // client is in CF-Connecting-IP. Honouring it is what makes the request
        // non-local, so the control surface stays out of a remote caller's reach.
        // RFC 5737 documentation address stands in for a real client.
        let socket = Some(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 9));
        let peer = PeerKey::resolve(socket, Some("192.0.2.10"), true);
        assert!(
            !peer.is_local(),
            "a forwarded public IP must not read as local"
        );
        assert_eq!(
            peer,
            PeerKey(Some(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10))))
        );
    }

    #[test]
    fn a_forwarded_header_is_ignored_without_trust() {
        use std::net::{Ipv4Addr, SocketAddr};
        // A gateway not started behind a proxy never trusts the header, so the
        // socket address stands and a loopback peer stays local.
        let socket = Some(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 9));
        assert!(PeerKey::resolve(socket, Some("192.0.2.10"), false).is_local());
    }

    #[test]
    fn a_forwarded_header_from_a_non_loopback_peer_is_ignored() {
        use std::net::{Ipv4Addr, SocketAddr};
        // The spoof guard: only the proxy on this machine, reaching us over
        // loopback, may set the client IP. A LAN peer sending the header cannot
        // forge an identity — the socket address it really came from wins.
        let lan = SocketAddr::new(Ipv4Addr::new(10, 0, 0, 1).into(), 9);
        assert_eq!(
            PeerKey::resolve(Some(lan), Some("192.0.2.10"), true),
            PeerKey::from_socket(lan)
        );
    }

    #[test]
    fn a_garbage_forwarded_header_falls_back_to_the_socket() {
        use std::net::{Ipv4Addr, SocketAddr};
        let socket = Some(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 9));
        assert!(PeerKey::resolve(socket, Some("not-an-ip"), true).is_local());
    }

    use futures_util::FutureExt;

    fn scheduler(capacity: u32) -> Arc<Scheduler> {
        Scheduler::new(capacity, SchedulerConfig::default())
    }

    /// Grant a ticket if the slot is already its turn, without waiting.
    /// The peer every single-client test shares.
    ///
    /// Named rather than written as `PeerKey::default()` at thirty call sites,
    /// because what these tests are asserting is that one caller's requests are
    /// ordered exactly as they always were.
    fn one_client() -> PeerKey {
        PeerKey::default()
    }

    fn granted_now(ticket: &mut Ticket) -> Option<SlotPermit> {
        ticket.granted().now_or_never().flatten()
    }

    #[test]
    fn a_request_is_classified_by_what_was_measured() {
        let limits = BandLimits {
            prompt_tokens: 1024,
            output_tokens: 256,
        };
        // The two requests from the acceptance run, by their real numbers: a
        // title generation, and a 5,596-token agent turn.
        assert_eq!(
            Band::classify(300, Some(32), limits),
            Band::Interactive,
            "a short auxiliary request must be interactive"
        );
        assert_eq!(
            Band::classify(5596, Some(32), limits),
            Band::Bulk,
            "a long prompt is bulk however small its output budget"
        );
        assert_eq!(Band::classify(300, Some(4096), limits), Band::Bulk);
    }

    #[test]
    fn an_unstated_output_budget_is_not_held_against_a_request() {
        // The gateway clamps an absent `max_tokens` to the whole remaining
        // window. Classifying on the clamped value would put every client that
        // simply omits the field — which is most of them — into the slow band,
        // and the interactive band would then never be used by anything.
        let limits = BandLimits::default();
        assert_eq!(Band::classify(300, None, limits), Band::Interactive);
    }

    #[test]
    fn band_limits_come_from_the_context_the_model_was_loaded_with() {
        // Proportional to the window, and capped so that a very large context
        // does not start calling genuinely expensive prompts small.
        let small = BandLimits::for_context(2048);
        let large = BandLimits::for_context(131_072);
        assert_eq!(
            large.prompt_tokens, 1024,
            "capped, not proportional forever"
        );
        assert!(large.prompt_tokens > small.prompt_tokens);
        assert_eq!(Band::classify(4000, None, large), Band::Bulk);
    }

    #[test]
    fn the_two_requests_this_scheduler_exists_for_land_in_different_bands() {
        // Measured from the real client rather than imagined: its title
        // generation asks for 64 output tokens
        // (`agent/title_generator.py:408`) and its agent turn asks for 65536
        // (`agent/run_agent.py:1673`). If those two ever land in the same band
        // the scheduler does nothing for the case it was built for — so it is
        // checked at the smallest context this project has served, where the
        // ceilings are tightest.
        let limits = BandLimits::for_context(2048);
        assert_eq!(
            Band::classify(32, Some(64), limits),
            Band::Interactive,
            "the auxiliary request must be interactive at every context size"
        );
        assert_eq!(
            Band::classify(5596, Some(65536), limits),
            Band::Bulk,
            "the agent turn must not be"
        );
    }

    #[tokio::test]
    async fn an_uncontended_request_never_touches_the_queue() {
        let scheduler = scheduler(1);
        let permit = scheduler.try_admit().expect("a free slot");
        assert_eq!(scheduler.snapshot().running, 1);
        assert!(
            scheduler.try_admit().is_none(),
            "the only slot is taken; a second must queue"
        );
        drop(permit);
        assert_eq!(scheduler.snapshot().running, 0);
        assert_eq!(scheduler.snapshot().queued, 0, "nothing ever queued");
    }

    #[tokio::test]
    async fn a_short_request_overtakes_a_long_one_that_is_already_waiting() {
        // The failure this whole module exists for: a 20-token title generation
        // sitting behind a multi-minute agent turn until the client gives up.
        let scheduler = scheduler(1);
        let running = scheduler.try_admit().expect("a free slot");

        let mut long = scheduler.enqueue(Band::Bulk, one_client());
        let mut short = scheduler.enqueue(Band::Interactive, one_client());
        assert_eq!(short.position(), Some(0), "the short request is next");
        assert_eq!(long.position(), Some(1), "even though it arrived second");

        drop(running);
        // Held, not dropped: releasing it here would hand the slot straight to
        // the bulk request and the next assertion would pass for the wrong
        // reason.
        let _short_permit = granted_now(&mut short).expect("the interactive request runs first");
        assert!(
            granted_now(&mut long).is_none(),
            "the bulk request must still be waiting"
        );
        assert_eq!(scheduler.snapshot().overtakes, 1);
    }

    #[tokio::test]
    async fn a_request_that_has_waited_long_enough_cannot_be_overtaken_again() {
        // Priority without this is starvation: a stream of small requests would
        // hold a long one at the back of the queue for as long as it kept
        // arriving.
        tokio::time::pause();
        let scheduler = Scheduler::new(
            1,
            SchedulerConfig {
                starvation_ceiling: Duration::from_secs(60),
                ..SchedulerConfig::default()
            },
        );
        let running = scheduler.try_admit().expect("a free slot");
        let mut long = scheduler.enqueue(Band::Bulk, one_client());

        tokio::time::advance(Duration::from_secs(61)).await;
        let mut short = scheduler.enqueue(Band::Interactive, one_client());
        assert_eq!(
            long.position(),
            Some(0),
            "it has aged out of being overtakeable"
        );

        drop(running);
        let _long_permit = granted_now(&mut long).expect("the aged-out request runs");
        assert!(granted_now(&mut short).is_none());
    }

    #[tokio::test]
    async fn requests_in_one_band_are_served_in_the_order_they_arrived() {
        let scheduler = scheduler(1);
        let running = scheduler.try_admit().expect("a free slot");
        let mut first = scheduler.enqueue(Band::Interactive, one_client());
        let mut second = scheduler.enqueue(Band::Interactive, one_client());
        assert_eq!((first.position(), second.position()), (Some(0), Some(1)));

        drop(running);
        let _first_permit = granted_now(&mut first).expect("the earlier request runs");
        assert!(granted_now(&mut second).is_none());
        assert_eq!(scheduler.snapshot().overtakes, 0, "nobody was passed over");
    }

    /// A second caller, distinct from [`one_client`].
    fn another_client() -> PeerKey {
        PeerKey::from_socket(SocketAddr::from(([10, 0, 0, 1], 50_000)))
    }

    #[tokio::test]
    async fn one_clients_requests_keep_the_order_they_always_had() {
        // The property that makes fair queuing safe to add: with a single
        // caller the rounds run 0, 1, 2 in arrival order, so ordering by round
        // is ordering by arrival and the key is the one it replaced.
        let scheduler = scheduler(1);
        let running = scheduler.try_admit().expect("a free slot");
        let mut tickets: Vec<_> = (0..4)
            .map(|_| scheduler.enqueue(Band::Bulk, one_client()))
            .collect();

        assert_eq!(
            tickets
                .iter()
                .map(Ticket::position)
                .collect::<Vec<_>>()
                .as_slice(),
            [Some(0), Some(1), Some(2), Some(3)]
        );

        drop(running);
        assert!(
            granted_now(&mut tickets[0]).is_some(),
            "the first to arrive should be the first to run"
        );
        assert_eq!(scheduler.snapshot().overtakes, 0, "nobody was passed over");
    }

    #[tokio::test]
    async fn a_second_client_waits_behind_one_request_rather_than_behind_ten() {
        // The failure fair queuing exists to prevent: one caller with a batch
        // of work, and another with a single request, on a shared gateway.
        // Bands cannot help here - every request is in the same band - and
        // first-come-first-served would put the newcomer eleventh.
        let scheduler = scheduler(1);
        let running = scheduler.try_admit().expect("a free slot");
        let mut busy: Vec<_> = (0..10)
            .map(|_| scheduler.enqueue(Band::Bulk, one_client()))
            .collect();
        let mut quiet = scheduler.enqueue(Band::Bulk, another_client());

        assert_eq!(
            quiet.position(),
            Some(1),
            "behind one of the busy client's requests, not behind all ten"
        );

        drop(running);
        let first = granted_now(&mut busy[0]).expect("the busy client's first request runs");
        drop(first);
        assert!(
            granted_now(&mut quiet).is_some(),
            "the quiet client should be served second, not eleventh"
        );
    }

    #[tokio::test]
    async fn a_client_that_keeps_arriving_does_not_keep_the_front_of_the_queue() {
        // Each new request from a caller joins the back of that caller's own
        // rotation, so arriving repeatedly cannot buy a better place.
        let scheduler = scheduler(1);
        let _running = scheduler.try_admit().expect("a free slot");
        let _first = scheduler.enqueue(Band::Bulk, one_client());
        let other = scheduler.enqueue(Band::Bulk, another_client());
        let second = scheduler.enqueue(Band::Bulk, one_client());

        assert_eq!(other.position(), Some(1));
        assert_eq!(
            second.position(),
            Some(2),
            "the busy client's second request sorts behind the other client's first"
        );
    }

    #[tokio::test]
    async fn a_short_request_still_overtakes_a_long_one_from_the_same_client() {
        // The band dominates the rotation: cost is still the first thing the
        // scheduler reads, and the round only breaks ties within a band.
        let scheduler = scheduler(1);
        let running = scheduler.try_admit().expect("a free slot");
        let mut long = scheduler.enqueue(Band::Bulk, one_client());
        let mut short = scheduler.enqueue(Band::Interactive, one_client());

        assert_eq!(short.position(), Some(0), "even at a later round");

        drop(running);
        // Bound, not dropped: an unbound permit is released at the end of the
        // statement and the long request would be granted the slot it just
        // gave back, which would say nothing about the order.
        let _short_permit = granted_now(&mut short).expect("the short request runs");
        assert!(granted_now(&mut long).is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn an_aged_out_request_is_not_overtaken_by_a_quieter_clients_first() {
        // The round is zeroed once a request has aged out, or the starvation
        // guarantee would hold only within a caller: a fresher request at
        // round 0 from a quiet client would still pass a request that had
        // waited past the ceiling at round 3.
        let scheduler = scheduler(1);
        let running = scheduler.try_admit().expect("a free slot");
        let mut ahead: Vec<_> = (0..3)
            .map(|_| scheduler.enqueue(Band::Bulk, one_client()))
            .collect();
        let aged = scheduler.enqueue(Band::Bulk, one_client());
        assert_eq!(aged.position(), Some(3));

        tokio::time::advance(Duration::from_secs(61)).await;
        let mut newcomer = scheduler.enqueue(Band::Interactive, another_client());
        assert_eq!(
            newcomer.position(),
            Some(4),
            "an aged-out request outranks every band, whoever it came from"
        );

        drop(running);
        // Among aged-out requests the longest wait wins, and all four of the
        // busy client's have aged out together - so the first of them runs,
        // not the one at the front of the rotation and not the newcomer.
        let _first = granted_now(&mut ahead[0]).expect("the longest wait runs first");
        assert!(granted_now(&mut newcomer).is_none());
    }

    #[tokio::test]
    async fn four_clients_are_served_at_once_before_anybody_waits() {
        let scheduler = scheduler(4);
        let permits: Vec<_> = (0..4)
            .map(|_| scheduler.try_admit().expect("a free slot"))
            .collect();
        assert_eq!(scheduler.snapshot().running, 4);

        let fifth = scheduler.enqueue(Band::Interactive, another_client());
        assert_eq!(fifth.position(), Some(0), "next, but not yet running");
        assert_eq!(scheduler.snapshot().waiting, 1);
        drop(permits);
    }

    #[tokio::test]
    async fn the_roster_names_the_band_a_slot_was_taken_in() {
        // The band is known at the moment of admission, so the roster carries
        // it from the start rather than after whoever took the slot gets round
        // to saying so.
        let scheduler = scheduler(2);
        let _fast = scheduler
            .admit_or_enqueue(Band::Interactive, one_client())
            .expect("a free slot");
        let _slow = scheduler
            .admit_or_enqueue(Band::Bulk, another_client())
            .expect("a second free slot");

        let bands: Vec<&str> = scheduler
            .roster()
            .running
            .iter()
            .map(|running| running.band)
            .collect();
        assert!(bands.contains(&"interactive"), "{bands:?}");
        assert!(bands.contains(&"bulk"), "{bands:?}");
    }

    #[tokio::test]
    async fn the_roster_holds_nothing_that_names_the_caller() {
        // The address decides who goes next and is never shown. This is the
        // automated half of that promise; the other half is that `PeerKey` has
        // no `Display` and a redacting `Debug`.
        let scheduler = scheduler(1);
        let _running = scheduler
            .admit_or_enqueue(Band::Bulk, another_client())
            .expect("a free slot");
        let queued = scheduler.enqueue(Band::Bulk, another_client());

        let rendered = serde_json::to_string(&scheduler.roster()).expect("the roster serializes");
        assert!(!rendered.contains("10.0.0.1"), "{rendered}");
        drop(queued);
    }

    #[tokio::test]
    async fn raising_the_slot_count_hands_the_new_slots_to_whoever_is_waiting() {
        // A model swap can raise the count. Without this the new slots sit
        // idle in front of a queue until the next release, which on a gateway
        // that has just been given more capacity is the opposite of what the
        // operator asked for.
        let scheduler = scheduler(1);
        let _running = scheduler.try_admit().expect("a free slot");
        let mut waiting: Vec<_> = (0..2)
            .map(|_| scheduler.enqueue(Band::Bulk, one_client()))
            .collect();

        scheduler.set_capacity(3);

        assert_eq!(scheduler.snapshot().capacity, 3);
        assert!(granted_now(&mut waiting[0]).is_some());
        assert!(granted_now(&mut waiting[1]).is_some());
        assert_eq!(scheduler.snapshot().waiting, 0);
    }

    #[tokio::test]
    async fn lowering_the_slot_count_preempts_nothing() {
        // The other direction: a swap to a model whose caches cost more can
        // only lower the count, and lowering it must not disturb a generation
        // that is already running - the engine cannot pause one, and killing
        // it would discard the prefill that dominates its cost.
        let scheduler = scheduler(3);
        let permits: Vec<_> = (0..3)
            .map(|_| scheduler.try_admit().expect("a free slot"))
            .collect();

        scheduler.set_capacity(1);

        assert_eq!(scheduler.snapshot().running, 3, "nothing was preempted");
        let mut queued = scheduler.enqueue(Band::Bulk, one_client());
        drop(permits);
        assert!(
            granted_now(&mut queued).is_some(),
            "the queue should resume once the count is back under the limit"
        );
        assert_eq!(scheduler.snapshot().capacity, 1);
    }

    #[tokio::test]
    async fn a_drain_at_four_slots_waits_for_the_last_of_them() {
        // A model swap must not replace the engine under three generations
        // because the fourth finished.
        let scheduler = scheduler(4);
        let permits: Vec<_> = (0..4)
            .map(|_| scheduler.try_admit().expect("a free slot"))
            .collect();
        let mut permits = permits;

        let last = permits.pop().expect("four permits");
        drop(permits);
        assert!(
            !scheduler.drain(Duration::from_millis(20)).await,
            "three of four finishing is not an idle engine"
        );

        drop(last);
        assert!(scheduler.drain(Duration::from_millis(20)).await);
    }

    #[tokio::test]
    async fn a_slot_released_while_a_request_joins_the_queue_is_not_left_idle() {
        // `try_admit` then `enqueue` were two locks with a gap between them,
        // and a slot released inside that gap found an empty queue, went idle,
        // and left the arriving request waiting for the *next* release. One
        // locked decision closes it.
        let scheduler = scheduler(1);
        let running = scheduler.try_admit().expect("a free slot");
        let mut ticket = match scheduler.admit_or_enqueue(Band::Bulk, one_client()) {
            Ok(_) => panic!("the only slot was taken"),
            Err(ticket) => ticket,
        };
        drop(running);
        assert!(
            granted_now(&mut ticket).is_some(),
            "the freed slot should have found the waiter"
        );
    }

    #[tokio::test]
    async fn a_ticket_that_is_no_longer_queued_reports_no_position() {
        // `Some(0)` says "you are next"; `None` says "you are not waiting for
        // anything". Reporting the first for both is how a granted request
        // came to look like the head of a queue it had already left.
        let scheduler = scheduler(1);
        let running = scheduler.try_admit().expect("a free slot");
        let mut ticket = match scheduler.admit_or_enqueue(Band::Bulk, one_client()) {
            Ok(_) => panic!("the only slot was taken"),
            Err(ticket) => ticket,
        };
        assert_eq!(ticket.position(), Some(0), "next, and still waiting");

        drop(running);
        let _permit = granted_now(&mut ticket).expect("the slot is handed over");
        assert_eq!(
            ticket.position(),
            None,
            "a granted request is not at the head of a queue it has left"
        );
    }

    #[tokio::test]
    async fn a_wait_that_ran_out_is_counted_as_a_timeout_and_not_as_a_disconnect() {
        // These were counted as both: `admit` incremented `timed_out` and the
        // ticket's own departure then incremented `abandoned` for the same
        // request, while the streamed path incremented only `abandoned`. Two
        // counters describing overlapping sets of requests are two numbers
        // nobody can act on.
        let scheduler = scheduler(1);
        let _running = scheduler.try_admit().expect("a free slot");
        let outcome = scheduler
            .admit(Band::Bulk, one_client(), Duration::from_millis(20))
            .await;

        assert_eq!(outcome.err(), Some(Queued::TimedOut));
        let snapshot = scheduler.snapshot();
        assert_eq!(snapshot.timed_out, 1);
        assert_eq!(snapshot.abandoned, 0, "nobody disconnected");
    }

    #[tokio::test]
    async fn a_client_that_disconnects_while_queued_leaves_the_queue() {
        let scheduler = scheduler(1);
        let running = scheduler.try_admit().expect("a free slot");
        let waiting = scheduler.enqueue(Band::Bulk, one_client());
        let mut behind = scheduler.enqueue(Band::Bulk, one_client());
        assert_eq!(scheduler.snapshot().waiting, 2);

        drop(waiting);
        assert_eq!(scheduler.snapshot().waiting, 1);
        assert_eq!(behind.position(), Some(0));

        drop(running);
        assert!(
            granted_now(&mut behind).is_some(),
            "the slot must go to whoever is left, not to the ticket that went away"
        );
    }

    #[tokio::test]
    async fn a_slot_granted_to_a_ticket_that_vanished_is_not_lost() {
        // The one race in this design: the scheduler picks a waiter and hands
        // it the slot in the same instant the client disconnects. Nothing else
        // knows that slot exists, so if the ticket does not give it back the
        // gateway serves nothing for the rest of the process's life.
        let scheduler = scheduler(1);
        let running = scheduler.try_admit().expect("a free slot");
        let ticket = scheduler.enqueue(Band::Bulk, one_client());

        drop(running); // the grant is now sitting in the ticket's channel
        assert_eq!(scheduler.snapshot().running, 1, "reserved for the ticket");

        drop(ticket); // ...and the client goes away without ever claiming it
        assert_eq!(scheduler.snapshot().running, 0, "the slot was given back");
        assert!(scheduler.try_admit().is_some(), "and can be used again");
    }

    #[tokio::test]
    async fn waiting_longer_than_the_timeout_is_a_refusal_not_a_hang() {
        let scheduler = scheduler(1);
        let _running = scheduler.try_admit().expect("a free slot");
        let outcome = scheduler
            .admit(Band::Interactive, one_client(), Duration::from_millis(20))
            .await;
        assert_eq!(outcome.err(), Some(Queued::TimedOut));
        assert_eq!(scheduler.snapshot().timed_out, 1);
        assert_eq!(scheduler.snapshot().waiting, 0, "and it left the queue");
    }

    #[tokio::test]
    async fn a_queued_request_runs_as_soon_as_the_slot_is_free() {
        let scheduler = scheduler(1);
        let running = scheduler.try_admit().expect("a free slot");
        let waiter = Arc::clone(&scheduler);
        let task = tokio::spawn(async move {
            waiter
                .admit(Band::Interactive, one_client(), Duration::from_secs(5))
                .await
                .map(drop)
        });
        tokio::task::yield_now().await;
        drop(running);
        assert!(task.await.expect("the waiting task").is_ok());
        let snapshot = scheduler.snapshot();
        assert_eq!(snapshot.queued, 1);
        assert_eq!(snapshot.running, 0);
        assert_eq!(snapshot.timed_out, 0);
    }

    #[tokio::test]
    async fn a_free_slot_is_not_taken_by_a_new_arrival_while_others_wait() {
        // Barging turns a queue into a lottery, and on a gateway where one turn
        // takes minutes a request that loses that lottery repeatedly is a
        // request that never runs.
        let scheduler = scheduler(1);
        let running = scheduler.try_admit().expect("a free slot");
        let mut queued = scheduler.enqueue(Band::Bulk, one_client());
        drop(running);
        assert!(
            scheduler.try_admit().is_none(),
            "a newcomer must not take a slot reserved for a waiting request"
        );
        assert!(granted_now(&mut queued).is_some());
    }

    #[tokio::test]
    async fn several_slots_are_handed_out_and_returned_independently() {
        // Concurrency is a parameter, and raising it must not need a rewrite:
        // a machine that can batch four sequences configures four.
        let scheduler = scheduler(4);
        let permits: Vec<_> = (0..4)
            .map(|_| scheduler.try_admit().expect("a free slot"))
            .collect();
        assert_eq!(scheduler.snapshot().running, 4);
        assert!(scheduler.try_admit().is_none());
        drop(permits);
        assert_eq!(scheduler.snapshot().running, 0);
    }

    #[tokio::test]
    async fn the_wait_is_measured_for_whoever_waited() {
        tokio::time::pause();
        let scheduler = scheduler(1);
        let running = scheduler.try_admit().expect("a free slot");
        let mut ticket = scheduler.enqueue(Band::Interactive, one_client());
        tokio::time::advance(Duration::from_secs(3)).await;
        drop(running);
        let _permit = granted_now(&mut ticket).expect("granted");
        let snapshot = scheduler.snapshot();
        assert_eq!(snapshot.wait_ms_max, 3000);
        assert_eq!(snapshot.wait_ms_total, 3000);
    }

    // ---- pausing for a model swap ----

    #[tokio::test]
    async fn a_paused_scheduler_admits_nothing_new() {
        let scheduler = Scheduler::new(1, SchedulerConfig::default());
        let guard = scheduler.pause();

        assert!(scheduler.is_paused());
        assert!(
            scheduler.try_admit().is_none(),
            "a request started during a swap"
        );

        drop(guard);
        assert!(!scheduler.is_paused());
        assert!(scheduler.try_admit().is_some(), "admission never came back");
    }

    #[tokio::test]
    async fn a_pause_waits_for_what_is_already_running_rather_than_stopping_it() {
        // Nothing is preempted. The generation in flight when a swap is asked
        // for runs to its end, and the swap waits.
        let scheduler = Scheduler::new(1, SchedulerConfig::default());
        let permit = scheduler.try_admit().expect("permit");
        let _guard = scheduler.pause();

        assert!(
            !scheduler.drain(Duration::from_millis(50)).await,
            "drain claimed the engine was idle while a request was running"
        );

        drop(permit);
        assert!(
            scheduler.drain(Duration::from_millis(500)).await,
            "drain did not notice the request finishing"
        );
    }

    #[tokio::test]
    async fn a_request_that_arrives_during_a_swap_queues_and_then_runs() {
        // The behaviour a client sees: not a refusal, a wait. And its place in
        // the queue survives the swap.
        let scheduler = Scheduler::new(1, SchedulerConfig::default());
        let guard = scheduler.pause();

        let mut first = scheduler.enqueue(Band::Bulk, one_client());
        let mut second = scheduler.enqueue(Band::Bulk, one_client());
        assert_eq!(scheduler.snapshot().waiting, 2);
        assert_eq!(scheduler.snapshot().running, 0);

        drop(guard);

        let granted = first.granted().await.expect("the first waiter runs");
        assert_eq!(scheduler.snapshot().running, 1);
        // One slot, so the second is still waiting - in the order it arrived.
        assert_eq!(scheduler.snapshot().waiting, 1);
        drop(granted);
        assert!(second.granted().await.is_some(), "the second never ran");
    }

    #[tokio::test]
    async fn lifting_a_pause_fills_every_free_slot_not_just_one() {
        // `release` frees one slot and grants one. A pause can end with the
        // whole engine idle and several requests queued, so resuming has to
        // hand out up to capacity.
        let scheduler = Scheduler::new(3, SchedulerConfig::default());
        let guard = scheduler.pause();
        let mut waiters: Vec<_> = (0..3)
            .map(|_| scheduler.enqueue(Band::Bulk, one_client()))
            .collect();
        drop(guard);

        // The permits are held, not dropped: dropping one releases its slot
        // immediately and the count would read zero for the wrong reason.
        let mut permits = Vec::new();
        for waiter in &mut waiters {
            permits.push(
                waiter
                    .granted()
                    .await
                    .expect("a queued request was left waiting on an idle engine"),
            );
        }
        assert_eq!(scheduler.snapshot().running, 3);
        assert_eq!(permits.len(), 3);
    }

    #[tokio::test]
    async fn a_failed_swap_cannot_leave_the_gateway_paused_forever() {
        // The guard exists for exactly this: an early return anywhere in a load
        // must not strand admission.
        let scheduler = Scheduler::new(1, SchedulerConfig::default());

        fn swap_that_fails(scheduler: &Arc<Scheduler>) -> Result<(), &'static str> {
            let _guard = scheduler.pause();
            Err("the engine would not start")
        }

        assert!(swap_that_fails(&scheduler).is_err());
        assert!(!scheduler.is_paused());
        assert!(scheduler.try_admit().is_some());
    }

    #[test]
    fn band_ceilings_follow_the_model_that_is_actually_loaded() {
        // A swap to a model with a different context must re-derive these. The
        // scheduler starts from its configured limits and takes new ones.
        let scheduler = Scheduler::new(1, SchedulerConfig::default());
        assert_eq!(scheduler.band_limits(), BandLimits::default());

        scheduler.set_band_limits(BandLimits::for_context(2048));
        assert_eq!(scheduler.band_limits(), BandLimits::for_context(2048));
        // And the seeded value is genuinely different, so this test can fail.
        assert_ne!(BandLimits::for_context(2048), BandLimits::default());
    }

    #[test]
    fn a_scheduler_starts_from_the_limits_it_was_configured_with() {
        let scheduler = Scheduler::new(
            1,
            SchedulerConfig {
                interactive: BandLimits::for_context(32768),
                ..SchedulerConfig::default()
            },
        );
        assert_eq!(scheduler.band_limits(), BandLimits::for_context(32768));
    }
}
