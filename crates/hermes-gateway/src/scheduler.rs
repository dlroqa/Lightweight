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
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::sync::{Notify, oneshot};
use tokio::time::Instant;

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
    enqueued: Instant,
    /// Signals that a slot is now this waiter's.
    ///
    /// A bare grant rather than the permit itself, deliberately: handing an
    /// owned permit through the channel would mean a failed send returns it to
    /// a caller that is holding the queue lock, and dropping it there would
    /// re-enter the release path against a lock it already owns.
    grant: oneshot::Sender<()>,
}

#[derive(Debug, Default)]
struct Inner {
    running: usize,
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
    capacity: usize,
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
            capacity: (capacity.max(1)) as usize,
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
            if self.lock().running == 0 {
                return true;
            }
            // Armed before the second check so a release that lands between the
            // two is not a lost wakeup.
            let waiting = self.drained.notified();
            if self.lock().running == 0 {
                return true;
            }
            if tokio::time::timeout_at(deadline, waiting).await.is_err() {
                return self.lock().running == 0;
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
        if !inner.paused && inner.running < self.capacity && inner.waiting.is_empty() {
            inner.running += 1;
            self.counters
                .admitted_immediately
                .fetch_add(1, Ordering::Relaxed);
            return Some(SlotPermit::new(Arc::clone(self)));
        }
        None
    }

    /// Join the queue.
    ///
    /// Returns a [`Ticket`], which can report its position while it waits and
    /// which releases its place when dropped — including when it is dropped by
    /// a client disconnecting, which is the only reason this is a ticket rather
    /// than a plain future.
    pub fn enqueue(self: &Arc<Self>, band: Band) -> Ticket {
        let (grant, granted) = oneshot::channel();
        let mut inner = self.lock();
        inner.next_id += 1;
        let id = inner.next_id;
        let enqueued = Instant::now();
        inner.waiting.push_back(Waiter {
            id,
            band,
            enqueued,
            grant,
        });
        self.counters.queued.fetch_add(1, Ordering::Relaxed);
        drop(inner);
        Ticket {
            id,
            band,
            enqueued,
            scheduler: Arc::clone(self),
            granted,
            taken: false,
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
        timeout: Duration,
    ) -> Result<SlotPermit, Queued> {
        if let Some(permit) = self.try_admit() {
            return Ok(permit);
        }
        let mut ticket = self.enqueue(band);
        match tokio::time::timeout(timeout, ticket.granted()).await {
            Ok(Some(permit)) => Ok(permit),
            Ok(None) => Err(Queued::Closed),
            Err(_) => {
                self.counters.timed_out.fetch_add(1, Ordering::Relaxed);
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
            capacity: self.capacity as u32,
            running: inner.running as u32,
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

    /// Give the slot back and hand it to whoever should have it next.
    ///
    /// Called from [`SlotPermit`]'s `Drop`, which is the only place a slot is
    /// ever released — there is no path that returns a permit by any other
    /// route, and therefore no path that leaks one.
    fn release(&self) {
        let mut inner = self.lock();
        inner.running = inner.running.saturating_sub(1);
        if inner.running == 0 {
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
            if waiter.grant.send(()).is_ok() {
                inner.running += 1;
                return;
            }
            self.counters.abandoned.fetch_add(1, Ordering::Relaxed);
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
        while inner.running < self.capacity {
            let Some(waiter) = self.take_next(&mut inner, now) else {
                break;
            };
            if waiter.grant.send(()).is_ok() {
                inner.running += 1;
            } else {
                self.counters.abandoned.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Drop a waiter that gave up, and say whether it had already been granted.
    fn withdraw(&self, id: u64) -> bool {
        let mut inner = self.lock();
        if let Some(index) = inner.waiting.iter().position(|waiter| waiter.id == id) {
            inner.waiting.remove(index);
            self.counters.abandoned.fetch_add(1, Ordering::Relaxed);
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
/// Tier 0 is the starvation guarantee: once a request has waited past the
/// ceiling it sorts ahead of every band, and among aged-out requests the
/// longest wait wins. Below that it is band, then arrival order.
fn rank(waiter: &Waiter, now: Instant, ceiling: Duration) -> (u8, Instant) {
    let aged_out = now.saturating_duration_since(waiter.enqueued) >= ceiling;
    let tier = if aged_out { 0 } else { waiter.band.tier() };
    (tier, waiter.enqueued)
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
    granted: oneshot::Receiver<()>,
    taken: bool,
}

impl Ticket {
    pub fn band(&self) -> Band {
        self.band
    }

    /// How long this request has been waiting.
    pub fn waited(&self) -> Duration {
        self.enqueued.elapsed()
    }

    /// How many requests would be served before this one.
    ///
    /// Zero means next. Computed with the same key the scheduler picks by, so
    /// a reported position cannot contradict the order actually served — though
    /// it is a snapshot, and a later arrival in a higher band can still move it.
    pub fn position(&self) -> u32 {
        let inner = self.scheduler.lock();
        let now = Instant::now();
        let ceiling = self.scheduler.config.starvation_ceiling;
        let Some(mine) = inner
            .waiting
            .iter()
            .find(|waiter| waiter.id == self.id)
            .map(|waiter| rank(waiter, now, ceiling))
        else {
            return 0;
        };
        inner
            .waiting
            .iter()
            .filter(|waiter| rank(waiter, now, ceiling) < mine)
            .count() as u32
    }

    /// Wait until this request's turn.
    ///
    /// `None` when the scheduler went away, which only happens at shutdown.
    pub async fn granted(&mut self) -> Option<SlotPermit> {
        match (&mut self.granted).await {
            Ok(()) => {
                // Set before returning and with no await in between, so a
                // future dropped at this point cannot lose the slot: either the
                // permit exists, or `Drop` still finds the grant unclaimed.
                self.taken = true;
                Some(SlotPermit::new(Arc::clone(&self.scheduler)))
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
        if self.scheduler.withdraw(self.id) {
            return;
        }
        // Not in the queue and no permit taken: a slot was granted to a ticket
        // that went away before claiming it. Nothing else knows about that
        // slot, so it has to be given back here or the gateway loses it for the
        // life of the process.
        if self.granted.try_recv().is_ok() {
            self.scheduler.release();
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
}

impl SlotPermit {
    fn new(scheduler: Arc<Scheduler>) -> Self {
        Self { scheduler }
    }
}

impl Drop for SlotPermit {
    fn drop(&mut self) {
        self.scheduler.release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::FutureExt;

    fn scheduler(capacity: u32) -> Arc<Scheduler> {
        Scheduler::new(capacity, SchedulerConfig::default())
    }

    /// Grant a ticket if the slot is already its turn, without waiting.
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

        let mut long = scheduler.enqueue(Band::Bulk);
        let mut short = scheduler.enqueue(Band::Interactive);
        assert_eq!(short.position(), 0, "the short request is next");
        assert_eq!(long.position(), 1, "even though it arrived second");

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
        let mut long = scheduler.enqueue(Band::Bulk);

        tokio::time::advance(Duration::from_secs(61)).await;
        let mut short = scheduler.enqueue(Band::Interactive);
        assert_eq!(long.position(), 0, "it has aged out of being overtakeable");

        drop(running);
        let _long_permit = granted_now(&mut long).expect("the aged-out request runs");
        assert!(granted_now(&mut short).is_none());
    }

    #[tokio::test]
    async fn requests_in_one_band_are_served_in_the_order_they_arrived() {
        let scheduler = scheduler(1);
        let running = scheduler.try_admit().expect("a free slot");
        let mut first = scheduler.enqueue(Band::Interactive);
        let mut second = scheduler.enqueue(Band::Interactive);
        assert_eq!((first.position(), second.position()), (0, 1));

        drop(running);
        let _first_permit = granted_now(&mut first).expect("the earlier request runs");
        assert!(granted_now(&mut second).is_none());
        assert_eq!(scheduler.snapshot().overtakes, 0, "nobody was passed over");
    }

    #[tokio::test]
    async fn a_client_that_disconnects_while_queued_leaves_the_queue() {
        let scheduler = scheduler(1);
        let running = scheduler.try_admit().expect("a free slot");
        let waiting = scheduler.enqueue(Band::Bulk);
        let mut behind = scheduler.enqueue(Band::Bulk);
        assert_eq!(scheduler.snapshot().waiting, 2);

        drop(waiting);
        assert_eq!(scheduler.snapshot().waiting, 1);
        assert_eq!(behind.position(), 0);

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
        let ticket = scheduler.enqueue(Band::Bulk);

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
            .admit(Band::Interactive, Duration::from_millis(20))
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
                .admit(Band::Interactive, Duration::from_secs(5))
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
        let mut queued = scheduler.enqueue(Band::Bulk);
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
        let mut ticket = scheduler.enqueue(Band::Interactive);
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

        let mut first = scheduler.enqueue(Band::Bulk);
        let mut second = scheduler.enqueue(Band::Bulk);
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
        let mut waiters: Vec<_> = (0..3).map(|_| scheduler.enqueue(Band::Bulk)).collect();
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
