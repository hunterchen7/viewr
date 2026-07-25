//! The engine: worker pool, priority scheduling with cooperative cancellation,
//! outward prefetch planning, and cache filling.
//!
//! Design (from the plan):
//! - Declarative interactive planning: on every navigation the desired display
//!   job set is recomputed and synced; queued interactive jobs outside it
//!   vanish, in-flight jobs outside it get their cancel token flipped, and
//!   jobs still wanted keep running. Epochs make stale heap entries inert.
//!   Folder-wide Browse warming uses a separate persistent lane that survives
//!   navigation replans.
//! - Outward wave, ~3:1 forward-biased: priority sorts by
//!   (class, effective distance, seq) where backward distance counts 3×.
//! - Workers produce only PixelBufs/JPEG bytes into the RamCache and post
//!   small events; the UI thread owns all textures.
//! - Encoded ring-2 JPEGs are written post-orientation, so rehydrates
//!   never rotate.

use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, TryLockError};
use std::time::Duration;

use crate::cache_disk::DiskCache;
use crate::cache_ram::RamCache;
use crate::decode;
use crate::develop::{Quality, develop};
use crate::folder::{FolderEntry, outward_order};
use crate::meta::FileMeta;
use crate::planning::{PlanKind, build_plan_targets};
use crate::resize::apply_orient;
use crate::types::{PixelBuf, Tier};

/// Scheduler identity as `(folder index, render tier)`.
pub type JobId = (usize, Tier);

const HEAVY_WORKERS: usize = 3;
const LIGHT_WORKERS: usize = 2;
/// One worker owns at most one active buffer, plus at most this many pending
/// bytes. 256 MiB fits a typical 61 MP RGBA frame (~230 MiB), so the lane is
/// bounded to at most two such retained frames; larger and excess requests are
/// best-effort cache misses.
const PERSIST_PENDING_BUDGET_BYTES: usize = 256 * 1024 * 1024;
const JPEG_QUALITY_BROWSE: u8 = 87;
const JPEG_QUALITY_FULL: u8 = 90;
const PERSIST_WRITE_ATTEMPTS: usize = 3;
const PERSIST_RETRY_BASE_DELAY: Duration = Duration::from_millis(2);

fn jpeg_quality(tier: Tier) -> u8 {
    match tier {
        Tier::Full => JPEG_QUALITY_FULL,
        Tier::Thumb | Tier::Browse => JPEG_QUALITY_BROWSE,
    }
}

/// Returns the production persistence quality for a benchmark tier.
#[cfg(feature = "benchmarks")]
#[doc(hidden)]
pub fn benchmark_jpeg_quality(tier: Tier) -> u8 {
    jpeg_quality(tier)
}

#[derive(Debug)]
/// Result notification published by [`Engine`] workers.
///
/// Pixel payloads are installed in [`RamCache`] before their ready event is
/// sent, keeping channel messages small. Events do not lease cache entries: a
/// later worker can evict an unpinned payload before the receiver handles the
/// message, so the receiver must tolerate a cache miss and replan.
pub enum Event {
    /// Container metadata became available without decoding a thumbnail.
    MetadataReady {
        /// Index in the engine's immutable folder entry list.
        index: usize,
        /// Extracted container metadata.
        meta: Box<FileMeta>,
    },
    /// A viewport-demanded thumbnail was produced and its metadata is ready.
    ThumbReady {
        /// Index in the engine's immutable folder entry list.
        index: usize,
        /// Metadata extracted during the thumbnail pass.
        meta: Box<FileMeta>,
    },
    /// A developed or rehydrated image was installed in the RAM cache.
    ImageReady {
        /// Index in the engine's immutable folder entry list.
        index: usize,
        /// Resident render tier.
        tier: Tier,
    },
    /// An image decode, development, or rehydration attempt failed.
    ImageFailed {
        /// Index in the engine's immutable folder entry list.
        index: usize,
        /// Tier whose production failed.
        tier: Tier,
        /// Human-readable underlying error.
        error: String,
    },
    /// Metadata extraction failed.
    MetadataFailed {
        /// Index in the engine's immutable folder entry list.
        index: usize,
        /// Human-readable underlying error.
        error: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Current navigation state used to reprioritize image work.
pub struct NavState {
    /// Current folder index. [`Engine::navigate`] clamps it to the last entry.
    pub current: usize,
    /// +1 browsing forward, -1 backward.
    ///
    /// [`Engine::navigate`] normalizes every negative value to `-1` and all
    /// other values to `1`.
    pub direction: i8,
    /// Whether the current view needs Full-tier renders.
    pub zoomed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Prio {
    class: u8,
    eff_dist: u32,
    seq: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Metadata,
    Thumb,
    Develop(Quality),
    /// Develop only to fill ring 2 + disk (P3 folder warm): no RGBA
    /// insert, no event — keeps far images from thrashing ring 1.
    WarmDevelop(Quality),
    Rehydrate,
}

struct QueuedJob {
    prio: Prio,
    epoch: u64,
    id: JobId,
    action: Action,
}

impl PartialEq for QueuedJob {
    fn eq(&self, other: &Self) -> bool {
        self.prio == other.prio
    }
}
impl Eq for QueuedJob {}
impl PartialOrd for QueuedJob {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for QueuedJob {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // BinaryHeap is a max-heap; invert so the smallest Prio pops first.
        other.prio.cmp(&self.prio)
    }
}

#[derive(Default)]
/// Cooperative cancellation flag shared between a queue and one worker.
///
/// Cancellation suppresses stale publication and is checked between expensive
/// pipeline stages. It cannot interrupt a `rawler` decode or demosaic that is
/// already executing.
pub struct CancelToken(AtomicBool);

impl CancelToken {
    /// Marks the associated work as cancelled.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }
    /// Returns whether cancellation has been requested.
    pub fn cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

#[derive(Clone, Copy)]
struct QueuedState {
    epoch: u64,
    action: Action,
}

struct InFlight {
    action: Action,
    token: Arc<CancelToken>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JobCompletion {
    /// The background obligation was either satisfied or made best-effort by
    /// an accepted persistence request.
    Complete,
    /// Cancellation stopped the job before it produced or handed off pixels.
    RetryBackground,
    /// The bounded persistence lane could not accept completed pixels. Park
    /// the job until that lane next frees capacity instead of immediately
    /// developing the RAW again.
    DeferBackground { required_bytes: usize },
}

struct DeferredJob {
    id: JobId,
    action: Action,
    required_bytes: usize,
}

#[derive(Default)]
struct QueueState {
    heap: BinaryHeap<QueuedJob>,
    /// id → epoch of its live heap entry. Entries with a different epoch
    /// are inert and skipped on pop.
    queued: HashMap<JobId, QueuedState>,
    /// Replaceable interactive lane, consumed before the background heap.
    /// Thumbnail viewport updates replace this deque wholesale, so offscreen
    /// requests cannot accumulate ahead of the newest visible items.
    urgent: VecDeque<(JobId, Action)>,
    in_flight: HashMap<JobId, InFlight>,
    /// One-shot folder-wide work. Unlike `heap`, this lane survives
    /// navigation replans and is consumed only when no interactive job is
    /// queued.
    background: VecDeque<(JobId, Action)>,
    /// Background generations remain tracked after a foreground replacement
    /// takes over the same ID. The worker's completion disposition decides
    /// whether cancellation requires retry or accepted persistence completed
    /// the one-shot obligation.
    background_in_flight: HashMap<JobId, InFlight>,
    /// Warm jobs rejected by the nonblocking persistence lane wait here until
    /// the persistence worker frees capacity. Keeping this separate prevents
    /// a tight RAW-develop/reject loop under backpressure.
    background_deferred: VecDeque<DeferredJob>,
    background_initialized: bool,
    epoch: u64,
    seq: u64,
    closed: bool,
}

struct JobQueue {
    state: Mutex<QueueState>,
    cond: Condvar,
}

#[derive(Clone)]
struct PersistenceRequest {
    id: JobId,
    pixels: Arc<PixelBuf>,
    /// Warm-only work populates disk but deliberately skips the RAM JPEG ring.
    insert_ram: bool,
    /// At least one coalesced producer was a persistent folder-warm job. This
    /// bit follows active and pending coalescing through completion so failure
    /// reporting reflects the background obligation honestly.
    warm_completion: bool,
}

impl PersistenceRequest {
    fn retained_bytes(&self) -> usize {
        self.pixels.byte_len()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PersistenceEnqueue {
    Queued,
    Coalesced,
    /// Temporary byte-budget pressure; at least one pending request will free
    /// capacity and wake a deferred warm job.
    Saturated,
    /// This buffer can never fit the configured pending budget. Retrying would
    /// park it forever, so persistence remains explicitly best-effort.
    Oversized,
    Busy,
    Closed,
}

struct ActivePersistence {
    id: JobId,
    insert_ram: bool,
    warm_completion: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PersistenceCompletion {
    insert_ram: bool,
    warm_completion: bool,
}

struct PersistenceState {
    order: VecDeque<JobId>,
    pending: HashMap<JobId, PersistenceRequest>,
    pending_bytes: usize,
    active: Option<ActivePersistence>,
    closed: bool,
}

/// A single-consumer, byte-bounded lane for JPEG encoding and persistence.
///
/// Producers try the state lock first and take it once only on contention;
/// JPEG encode and disk I/O never hold it. Requests for an active or pending
/// ID coalesce. Warm work rejected by temporary byte pressure is parked by the
/// job queue, while oversized buffers remain an explicit best-effort miss.
struct PersistenceQueue {
    state: Mutex<PersistenceState>,
    ready: Condvar,
    pending_budget_bytes: usize,
}

impl PersistenceQueue {
    fn new() -> Self {
        Self::with_budget(PERSIST_PENDING_BUDGET_BYTES)
    }

    fn with_budget(pending_budget_bytes: usize) -> Self {
        Self {
            state: Mutex::new(PersistenceState {
                order: VecDeque::new(),
                pending: HashMap::new(),
                pending_bytes: 0,
                active: None,
                closed: false,
            }),
            ready: Condvar::new(),
            pending_budget_bytes,
        }
    }

    /// Prefer a nonblocking state update. If another producer owns the short
    /// queue-state critical section, retain the completed pixels and take that
    /// mutex once instead of dropping them or re-developing the RAW. JPEG
    /// encode and disk I/O never run under this lock.
    fn enqueue(&self, request: PersistenceRequest) -> PersistenceEnqueue {
        match self.try_enqueue(request.clone()) {
            PersistenceEnqueue::Busy => self.enqueue_after_busy(request),
            outcome => outcome,
        }
    }

    fn try_enqueue(&self, request: PersistenceRequest) -> PersistenceEnqueue {
        let mut state = match self.state.try_lock() {
            Ok(state) => state,
            Err(TryLockError::WouldBlock) => return PersistenceEnqueue::Busy,
            Err(TryLockError::Poisoned(_)) => return PersistenceEnqueue::Closed,
        };
        let outcome = self.enqueue_locked(&mut state, request);
        drop(state);
        if outcome == PersistenceEnqueue::Queued {
            self.ready.notify_one();
        }
        outcome
    }

    fn enqueue_after_busy(&self, request: PersistenceRequest) -> PersistenceEnqueue {
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(_) => return PersistenceEnqueue::Closed,
        };
        let outcome = self.enqueue_locked(&mut state, request);
        drop(state);
        if outcome == PersistenceEnqueue::Queued {
            self.ready.notify_one();
        }
        outcome
    }

    fn enqueue_locked(
        &self,
        state: &mut PersistenceState,
        request: PersistenceRequest,
    ) -> PersistenceEnqueue {
        if state.closed {
            return PersistenceEnqueue::Closed;
        }

        if let Some(active) = state.active.as_mut()
            && active.id == request.id
        {
            active.insert_ram |= request.insert_ram;
            active.warm_completion |= request.warm_completion;
            return PersistenceEnqueue::Coalesced;
        }
        if let Some(pending) = state.pending.get_mut(&request.id) {
            pending.insert_ram |= request.insert_ram;
            pending.warm_completion |= request.warm_completion;
            return PersistenceEnqueue::Coalesced;
        }

        let retained_bytes = request.retained_bytes();
        if retained_bytes > self.pending_budget_bytes {
            return PersistenceEnqueue::Oversized;
        }
        if state.pending_bytes > self.pending_budget_bytes - retained_bytes {
            return PersistenceEnqueue::Saturated;
        }

        state.pending_bytes += retained_bytes;
        state.order.push_back(request.id);
        state.pending.insert(request.id, request);
        PersistenceEnqueue::Queued
    }

    fn pop(&self) -> Option<PersistenceRequest> {
        let mut state = self.state.lock().unwrap();
        loop {
            if let Some(id) = state.order.pop_front() {
                let request = state
                    .pending
                    .remove(&id)
                    .expect("persistence order must reference a pending request");
                state.pending_bytes -= request.retained_bytes();
                debug_assert!(state.active.is_none());
                state.active = Some(ActivePersistence {
                    id,
                    insert_ram: request.insert_ram,
                    warm_completion: request.warm_completion,
                });
                return Some(request);
            }
            if state.closed {
                return None;
            }
            state = self.ready.wait(state).unwrap();
        }
    }

    /// Mark an existing active/pending request as satisfying a folder-warm
    /// obligation. This is the zero-copy coalescing path used before a warm
    /// worker starts another RAW decode. Both states are observed under one
    /// lock so the handoff between them cannot look like an idle gap.
    fn mark_warm_completion_if_present(&self, id: JobId) -> bool {
        let mut state = self.state.lock().unwrap();
        if let Some(active) = state.active.as_mut()
            && active.id == id
        {
            active.warm_completion = true;
            return true;
        }
        if let Some(pending) = state.pending.get_mut(&id) {
            pending.warm_completion = true;
            return true;
        }
        false
    }

    fn available_pending_bytes(&self) -> usize {
        let state = self.state.lock().unwrap();
        if state.closed {
            0
        } else {
            self.pending_budget_bytes
                .saturating_sub(state.pending_bytes)
        }
    }

    /// Finish the active request and return requirements merged from every
    /// duplicate that arrived while it was being persisted.
    fn finish(&self, id: JobId) -> PersistenceCompletion {
        let mut state = self.state.lock().unwrap();
        let active = state
            .active
            .take()
            .expect("a persistence request must be active when it finishes");
        debug_assert_eq!(active.id, id);
        PersistenceCompletion {
            insert_ram: active.insert_ram,
            warm_completion: active.warm_completion,
        }
    }

    /// Reject new work, drain accepted requests, then let the worker exit.
    fn close(&self) {
        self.state.lock().unwrap().closed = true;
        self.ready.notify_all();
    }
}

impl JobQueue {
    fn new() -> Self {
        Self {
            state: Mutex::new(QueueState::default()),
            cond: Condvar::new(),
        }
    }

    /// Replace the interactive job set. In-flight interactive jobs no longer
    /// wanted are cancelled; in-flight jobs still wanted keep running (not
    /// duplicated). A real navigation change can also cancel active
    /// background generations so newly interactive work reaches a worker at
    /// the next cancellation point.
    fn set_plan(&self, plan: Vec<(JobId, u8, u32, Action)>, cancel_background: bool) {
        let mut state = self.state.lock().unwrap();
        if state.closed {
            return;
        }
        state.epoch += 1;
        let epoch = state.epoch;

        // Build the wanted set in the allocation we retain as the queued
        // index. This avoids a temporary HashSet on every navigation.
        state.queued.clear();
        state.queued.reserve(plan.len());
        for (id, _, _, action) in &plan {
            state.queued.insert(
                *id,
                QueuedState {
                    epoch,
                    action: *action,
                },
            );
        }
        if cancel_background {
            for running in state.background_in_flight.values() {
                running.token.cancel();
            }
        }
        for (id, running) in &state.in_flight {
            let is_background = state
                .background_in_flight
                .get(id)
                .is_some_and(|background| Arc::ptr_eq(&background.token, &running.token));
            let wanted_action = state.queued.get(id).map(|wanted| wanted.action);
            if (!is_background && wanted_action != Some(running.action))
                || (is_background && wanted_action.is_some_and(|action| action != running.action))
            {
                running.token.cancel();
            }
        }

        // Reuse the heap's backing allocation, then heapify once in O(P).
        // Repeated push would make each navigation O(P log P).
        let mut jobs = std::mem::take(&mut state.heap).into_vec();
        jobs.clear();
        jobs.reserve(plan.len());
        for (id, class, eff_dist, action) in plan {
            // Skip only if a LIVE instance is already running; a cancelled
            // in-flight instance won't publish, so queue a fresh one.
            if state
                .in_flight
                .get(&id)
                .is_some_and(|running| running.action == action && !running.token.cancelled())
            {
                state.queued.remove(&id);
                continue;
            }
            state.seq += 1;
            let prio = Prio {
                class,
                eff_dist,
                seq: state.seq,
            };
            jobs.push(QueuedJob {
                prio,
                epoch,
                id,
                action,
            });
        }
        state.heap = BinaryHeap::from(jobs);
        drop(state);
        self.cond.notify_all();
    }

    /// Install persistent background work exactly once. The closure keeps the
    /// O(N) folder ordering allocation off every later navigation call.
    fn initialize_background<I>(&self, jobs: impl FnOnce() -> I) -> bool
    where
        I: IntoIterator<Item = (JobId, Action)>,
    {
        let mut state = self.state.lock().unwrap();
        if state.closed || state.background_initialized {
            return false;
        }
        state.background_initialized = true;
        state.background.extend(jobs());
        drop(state);
        self.cond.notify_all();
        true
    }

    /// Append jobs without disturbing the existing plan (used for the
    /// one-shot metadata wave).
    fn extend(&self, jobs: impl IntoIterator<Item = (JobId, u8, u32, Action)>) {
        let mut state = self.state.lock().unwrap();
        if state.closed {
            return;
        }
        let epoch = state.epoch;
        for (id, class, eff_dist, action) in jobs {
            state.seq += 1;
            let prio = Prio {
                class,
                eff_dist,
                seq: state.seq,
            };
            state.queued.insert(id, QueuedState { epoch, action });
            state.heap.push(QueuedJob {
                prio,
                epoch,
                id,
                action,
            });
        }
        drop(state);
        self.cond.notify_all();
    }

    /// Replace the bounded interactive lane without disturbing background
    /// priorities. A queued background copy remains valid until the urgent job
    /// actually starts; dropping an item from this lane therefore demotes it
    /// for free on the next viewport update.
    fn set_urgent(&self, jobs: impl IntoIterator<Item = (JobId, Action)>) -> bool {
        let mut state = self.state.lock().unwrap();
        if state.closed {
            return false;
        }
        state.urgent.clear();
        let mut seen = HashSet::new();
        for (id, action) in jobs {
            if state
                .in_flight
                .get(&id)
                .is_some_and(|running| running.action == action && !running.token.cancelled())
            {
                continue;
            }
            if seen.insert(id) {
                state.urgent.push_back((id, action));
            }
        }
        drop(state);
        self.cond.notify_all();
        true
    }

    /// Block until a valid job is available (or shutdown).
    fn pop(&self) -> Option<(JobId, Action, Arc<CancelToken>)> {
        let mut state = self.state.lock().unwrap();
        loop {
            if state.closed {
                return None;
            }
            while let Some((id, action)) = state.urgent.pop_front() {
                if state
                    .in_flight
                    .get(&id)
                    .is_some_and(|running| running.action == action && !running.token.cancelled())
                {
                    continue;
                }
                // Invalidate a background copy only when this urgent copy is
                // actually claimed. Until then, replacing the urgent viewport
                // leaves the original background priority untouched.
                state.queued.remove(&id);
                if let Some(displaced) = state.in_flight.get(&id) {
                    displaced.token.cancel();
                }
                let token = Arc::new(CancelToken::default());
                state.in_flight.insert(
                    id,
                    InFlight {
                        action,
                        token: token.clone(),
                    },
                );
                return Some((id, action, token));
            }
            while let Some(job) = state.heap.pop() {
                if state.queued.get(&job.id).map(|queued| queued.epoch) != Some(job.epoch) {
                    continue; // superseded by a newer plan
                }
                state.queued.remove(&job.id);
                let token = Arc::new(CancelToken::default());
                state.in_flight.insert(
                    job.id,
                    InFlight {
                        action: job.action,
                        token: token.clone(),
                    },
                );
                return Some((job.id, job.action, token));
            }
            // Persistent folder warming is strictly below both replaceable
            // lanes. Rotate candidates whose foreground generation is still
            // active; `finish` wakes us when one becomes runnable.
            let background_len = state.background.len();
            for _ in 0..background_len {
                let (id, action) = state
                    .background
                    .pop_front()
                    .expect("background length was captured under the queue lock");
                if state.in_flight.contains_key(&id) {
                    state.background.push_back((id, action));
                    continue;
                }
                let token = Arc::new(CancelToken::default());
                state.background_in_flight.insert(
                    id,
                    InFlight {
                        action,
                        token: token.clone(),
                    },
                );
                state.in_flight.insert(
                    id,
                    InFlight {
                        action,
                        token: token.clone(),
                    },
                );
                return Some((id, action, token));
            }
            state = self.cond.wait(state).unwrap();
        }
    }

    #[cfg(test)]
    fn finish(&self, id: JobId, token: &Arc<CancelToken>) {
        self.finish_with(id, token, JobCompletion::Complete);
    }

    fn finish_with(&self, id: JobId, token: &Arc<CancelToken>, completion: JobCompletion) {
        let mut state = self.state.lock().unwrap();
        let retry_action = state.background_in_flight.get(&id).and_then(|background| {
            Arc::ptr_eq(&background.token, token).then_some(background.action)
        });
        if let Some(action) = retry_action {
            state.background_in_flight.remove(&id);
            if !state.closed {
                match completion {
                    JobCompletion::Complete => {}
                    JobCompletion::RetryBackground => state.background.push_back((id, action)),
                    JobCompletion::DeferBackground { required_bytes } => {
                        state.background_deferred.push_back(DeferredJob {
                            id,
                            action,
                            required_bytes,
                        });
                    }
                }
            }
        }
        if state
            .in_flight
            .get(&id)
            .is_some_and(|current| Arc::ptr_eq(&current.token, token))
        {
            state.in_flight.remove(&id);
        }
        drop(state);
        self.cond.notify_one();
    }

    /// Make one backpressured warm job runnable after persistence capacity or
    /// state changes. Releasing one at a time avoids a thundering herd of RAW
    /// decodes that would immediately saturate the byte budget again.
    fn release_one_deferred(&self, available_bytes: usize) -> bool {
        let mut state = self.state.lock().unwrap();
        if state.closed {
            return false;
        }
        let Some(job) = state.background_deferred.front() else {
            return false;
        };
        if job.required_bytes > available_bytes {
            return false;
        }
        let job = state
            .background_deferred
            .pop_front()
            .expect("the deferred front was inspected under the queue lock");
        state.background.push_back((job.id, job.action));
        drop(state);
        self.cond.notify_one();
        true
    }

    /// Cancel active work, discard queued work, and wake every waiter.
    /// `closed` is protected by the same mutex as the condition-variable
    /// predicate, so shutdown cannot lose a wakeup between check and wait.
    fn close(&self) {
        let mut state = self.state.lock().unwrap();
        state.closed = true;
        state.heap.clear();
        state.queued.clear();
        state.urgent.clear();
        state.background.clear();
        state.background_deferred.clear();
        for running in state.in_flight.values() {
            running.token.cancel();
        }
        for running in state.background_in_flight.values() {
            running.token.cancel();
        }
        state.background_in_flight.clear();
        drop(state);
        self.cond.notify_all();
    }
}

#[derive(Default)]
struct NavigationOrder {
    /// Display order after filtering. Empty means identity order.
    indices: Vec<usize>,
    /// The last normalized state planned against `indices`. Keeping both
    /// values under one mutex makes a sequence change atomically invalidate
    /// the cancellation generation.
    last_nav: Option<NavState>,
}

impl NavigationOrder {
    fn update_navigation(&mut self, nav: NavState) -> bool {
        let changed = self.last_nav != Some(nav);
        self.last_nav = Some(nav);
        changed
    }

    fn replace_indices(&mut self, indices: Vec<usize>) {
        self.indices = indices;
        self.last_nav = None;
    }
}

struct Shared {
    entries: Arc<Vec<FolderEntry>>,
    cache: Arc<RamCache>,
    disk: Option<DiskCache>,
    events: Sender<Event>,
    notify: Arc<dyn Fn() + Send + Sync>,
    heavy: JobQueue,
    light: JobQueue,
    persistence: PersistenceQueue,
    /// Display order and its last navigation generation.
    navigation: Mutex<NavigationOrder>,
}

/// Owned worker engine for prioritized RAW, thumbnail, and cache work.
///
/// The folder entry list is immutable for the engine's lifetime, making its
/// indices stable across events and cache keys. Interactive navigation plans
/// replace one another, viewport thumbnail demand is replaceable, and
/// folder-wide Browse warming occupies a persistent lowest-priority lane.
/// Dropping the engine cancels queued work and joins decode and persistence
/// threads; no image worker may outlive the engine. Drop can block until an
/// active non-interruptible `rawler` decode or demosaic finishes and until
/// accepted persistence work has joined. Disk-cache maintenance is detached,
/// single-flight best-effort work and does not delay engine teardown.
pub struct Engine {
    shared: Arc<Shared>,
    workers: Vec<std::thread::JoinHandle<()>>,
    persistence_worker: Option<std::thread::JoinHandle<()>>,
}

fn navigation_pins(
    len: usize,
    current: usize,
    include_full: bool,
    sequence: &[usize],
) -> Vec<JobId> {
    if len == 0 {
        return Vec::new();
    }
    let current = current.min(len - 1);
    let indices: Vec<usize> = if sequence.is_empty() {
        (current.saturating_sub(1)..=(current + 1).min(len - 1)).collect()
    } else {
        let position = sequence
            .iter()
            .position(|&index| index == current)
            .unwrap_or_default();
        let first = position.saturating_sub(1);
        let last = (position + 1).min(sequence.len() - 1);
        let mut indices: Vec<_> = sequence[first..=last]
            .iter()
            .copied()
            .filter(|&index| index < len)
            .collect();
        if !indices.contains(&current) {
            indices.push(current);
        }
        indices
    };
    let mut pins = Vec::with_capacity(indices.len() * (2 + usize::from(include_full)));
    for index in indices {
        pins.extend([(index, Tier::Thumb), (index, Tier::Browse)]);
        if include_full {
            pins.push((index, Tier::Full));
        }
    }
    pins
}

fn background_warm_jobs(len: usize, start: usize) -> impl Iterator<Item = (JobId, Action)> {
    outward_order(len, start)
        .into_iter()
        .map(|index| ((index, Tier::Browse), Action::WarmDevelop(Quality::Browse)))
}

impl Engine {
    /// Spawns the worker pool and queues the outward metadata wave.
    /// `notify` is called after every published result (the app passes
    /// `ctx.request_repaint`). Calls run on engine worker threads, may overlap,
    /// and should return quickly without blocking. Callback panics are caught
    /// and logged so the worker can continue. Container metadata is queued
    /// outward from `start`; preview pixels remain demand-driven.
    ///
    /// # Panics
    ///
    /// Panics if an operating-system thread cannot be spawned.
    pub fn new(
        entries: Arc<Vec<FolderEntry>>,
        start: usize,
        cache: Arc<RamCache>,
        disk: Option<DiskCache>,
        notify: Arc<dyn Fn() + Send + Sync>,
    ) -> (Self, Receiver<Event>) {
        let (events, rx) = std::sync::mpsc::channel();
        let shared = Arc::new(Shared {
            entries,
            cache,
            disk,
            events,
            notify,
            heavy: JobQueue::new(),
            light: JobQueue::new(),
            persistence: PersistenceQueue::new(),
            navigation: Mutex::new(NavigationOrder::default()),
        });

        let persistence_worker = {
            let shared = shared.clone();
            std::thread::Builder::new()
                .name("viewr-persistence".into())
                .spawn(move || persistence_worker(&shared))
                .expect("failed to spawn persistence worker")
        };

        let mut workers = Vec::with_capacity(HEAVY_WORKERS + LIGHT_WORKERS);
        for worker_index in 0..HEAVY_WORKERS {
            let shared = shared.clone();
            workers.push(
                std::thread::Builder::new()
                    .name(format!("viewr-heavy-{worker_index}"))
                    .spawn(move || worker(&shared, false))
                    .expect("failed to spawn heavy worker"),
            );
        }
        for worker_index in 0..LIGHT_WORKERS {
            let shared = shared.clone();
            workers.push(
                std::thread::Builder::new()
                    .name(format!("viewr-light-{worker_index}"))
                    .spawn(move || worker(&shared, true))
                    .expect("failed to spawn light worker"),
            );
        }

        // Background disk-cache GC sweep on open. A session does not own this
        // best-effort maintenance task: a new sweep skips if the same cache
        // root is already being scanned, and engine teardown never waits for
        // it.
        if let Some(disk) = shared.disk.clone()
            && let Err(error) = std::thread::Builder::new()
                .name("viewr-cache-gc".into())
                .spawn(move || {
                    let _ = disk.try_gc_to_budget();
                })
        {
            eprintln!("failed to spawn disk cache GC worker: {error}");
        }

        // Discover embedded ratings and EXIF in the background without
        // decoding every preview JPEG. Thumbnail pixels are requested only by
        // the current viewport through `set_thumbnail_demand`.
        let len = shared.entries.len();
        shared.light.extend(
            outward_order(len, start)
                .into_iter()
                .enumerate()
                .map(|(dist, index)| ((index, Tier::Thumb), 5, dist as u32, Action::Metadata)),
        );

        (
            Self {
                shared,
                workers,
                persistence_worker: Some(persistence_worker),
            },
            rx,
        )
    }

    /// Recomputes and synchronizes the heavy plan for a navigation state.
    ///
    /// Call this after navigation or zoom changes and after image completion
    /// events. Obsolete jobs are cooperatively cancelled. Fit mode omits and
    /// unpins Full-tier work; zoom mode requests Full for the current and near
    /// entries. The first call with a disk cache also installs the one-shot
    /// folder-wide Browse warm lane.
    pub fn navigate(&self, nav: NavState) {
        let len = self.shared.entries.len();
        if len == 0 {
            return;
        }
        let nav = NavState {
            current: nav.current.min(len - 1),
            direction: if nav.direction < 0 { -1 } else { 1 },
            zoomed: nav.zoomed,
        };
        let current = nav.current;
        let cache = &self.shared.cache;

        let disk = &self.shared.disk;
        let (pins, targets, navigation_changed) = {
            let mut navigation = self.shared.navigation.lock().unwrap();
            let navigation_changed = navigation.update_navigation(nav);
            (
                navigation_pins(len, current, nav.zoomed, &navigation.indices),
                build_plan_targets(
                    len,
                    current,
                    nav.direction,
                    nav.zoomed,
                    &navigation.indices,
                    false,
                ),
                navigation_changed,
            )
        };
        // Full buffers are useful only while inspecting at zoom. Filtered
        // navigation pins visible neighbors rather than unrelated raw indices.
        cache.set_pins(pins);
        let mut plan: Vec<(JobId, u8, u32, Action)> = Vec::with_capacity(targets.len());
        for target in targets {
            let id = (target.index, target.tier);
            match target.kind {
                PlanKind::Display => {
                    if cache.has_rgba(id) {
                        continue;
                    }
                    // A configured disk cache is probed by the worker during
                    // rehydrate. Navigation must not issue filesystem calls
                    // for every candidate on the UI thread.
                    let action = if cache.has_jpeg(id) || disk.is_some() {
                        Action::Rehydrate
                    } else {
                        Action::Develop(match target.tier {
                            Tier::Full => Quality::Full,
                            _ => Quality::Browse,
                        })
                    };
                    plan.push((id, target.class, target.effective_distance, action));
                }
                PlanKind::Warm => {
                    // Folder warming lives in the persistent background lane,
                    // so navigation never contributes this O(N) target set.
                }
            }
        }

        let cancel_background = navigation_changed && !plan.is_empty();
        self.shared.heavy.set_plan(plan, cancel_background);
        if disk.is_some() {
            self.shared
                .heavy
                .initialize_background(|| background_warm_jobs(len, current));
        }
    }

    /// Sets the display order followed by navigation and pinning.
    ///
    /// An empty vector restores identity order. Out-of-range entries are
    /// ignored by planning, but callers should normally provide unique valid
    /// indices. Call [`navigate`](Self::navigate) afterwards to apply the new
    /// order.
    pub fn set_sequence(&self, sequence: Vec<usize>) {
        let mut navigation = self.shared.navigation.lock().unwrap();
        navigation.replace_indices(sequence);
    }

    /// Replace the thumbnail viewport demand lane. It is intentionally
    /// separate from the folder-wide metadata wave so rapid scrolling drops
    /// stale urgency. Claiming a thumbnail displaces queued metadata for that
    /// file because the thumbnail decode returns the same metadata.
    ///
    /// Invalid indices and duplicates are discarded. Returns `false` only
    /// after engine shutdown has closed the light-work queue.
    pub fn set_thumbnail_demand(&self, indices: &[usize]) -> bool {
        self.shared.light.set_urgent(
            indices
                .iter()
                .copied()
                .filter(|index| *index < self.shared.entries.len())
                .map(|index| ((index, Tier::Thumb), Action::Thumb)),
        )
    }

    /// Returns the shared RAM cache populated by this engine.
    pub fn cache(&self) -> &Arc<RamCache> {
        &self.shared.cache
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.shared.heavy.close();
        self.shared.light.close();
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
        self.shared.persistence.close();
        if let Some(worker) = self.persistence_worker.take() {
            let _ = worker.join();
        }
    }
}

/// Builds the production metadata queue and returns its resident job count.
///
/// This is exposed only to the Criterion harness so folder-open queue
/// construction can be measured without starting decoder threads or touching
/// the filesystem.
#[cfg(feature = "benchmarks")]
#[doc(hidden)]
pub fn benchmark_metadata_queue_setup(len: usize) -> usize {
    let queue = JobQueue::new();
    queue.extend((0..len).map(|index| {
        (
            (index, Tier::Thumb),
            5,
            index.min(u32::MAX as usize) as u32,
            Action::Metadata,
        )
    }));
    queue.state.lock().unwrap().heap.len()
}

/// Production priority-queue synchronization isolated from decoder threads.
#[cfg(feature = "benchmarks")]
#[doc(hidden)]
pub struct BenchmarkNavigationQueue {
    queue: JobQueue,
    len: usize,
}

#[cfg(feature = "benchmarks")]
impl BenchmarkNavigationQueue {
    /// Creates an empty production queue for a synthetic folder.
    pub fn new(len: usize) -> Self {
        Self {
            queue: JobQueue::new(),
            len,
        }
    }

    /// Replans one fit-mode navigation and returns the queued target count.
    pub fn navigate(&self, current: usize) -> usize {
        let plan = build_plan_targets(self.len, current, 1, false, &[], false)
            .into_iter()
            .filter(|target| target.kind == PlanKind::Display)
            .map(|target| {
                (
                    (target.index, target.tier),
                    target.class,
                    target.effective_distance,
                    Action::Develop(Quality::Browse),
                )
            })
            .collect();
        self.queue.set_plan(plan, true);
        self.queue.state.lock().unwrap().heap.len()
    }
}

fn worker(shared: &Shared, light: bool) {
    let queue = if light { &shared.light } else { &shared.heavy };
    while let Some((id, action, token)) = queue.pop() {
        let deferred_bytes = execute_claimed_job(
            queue,
            id,
            action,
            &token,
            || run_job(shared, id, action, &token),
            |event| publish(shared, event),
        );
        // Close the race where persistence frees capacity immediately before
        // this worker parks its rejected warm item. A later completion also
        // runs the same one-at-a-time admission check.
        if let Some(required_bytes) = deferred_bytes {
            let available_bytes = shared.persistence.available_pending_bytes();
            if required_bytes <= available_bytes {
                queue.release_one_deferred(available_bytes);
            }
        }
    }
}

/// Run one claimed job without allowing a decoder panic to strand its queue
/// identity or permanently remove a worker from the pool.
///
/// A panicking background job is completed rather than retried: repeating an
/// identical panic would otherwise create an unbounded retry loop. Queue
/// cleanup happens before failure publication so even an unexpected reporting
/// failure cannot leave the job in `in_flight`.
fn execute_claimed_job(
    queue: &JobQueue,
    id: JobId,
    action: Action,
    token: &Arc<CancelToken>,
    run: impl FnOnce() -> JobCompletion,
    emit: impl FnOnce(Event),
) -> Option<usize> {
    let (completion, panic_payload) =
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)) {
            Ok(completion) => (completion, None),
            Err(payload) => (JobCompletion::Complete, Some(payload)),
        };
    let deferred_bytes = match completion {
        JobCompletion::DeferBackground { required_bytes } => Some(required_bytes),
        JobCompletion::Complete | JobCompletion::RetryBackground => None,
    };
    queue.finish_with(id, token, completion);
    if let Some(payload) = panic_payload {
        emit(worker_panic_event(id, action, payload.as_ref()));
    }
    deferred_bytes
}

fn worker_panic_event(
    (index, tier): JobId,
    action: Action,
    payload: &(dyn std::any::Any + Send),
) -> Event {
    let detail = if let Some(message) = payload.downcast_ref::<&str>() {
        *message
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.as_str()
    } else {
        "non-string panic payload"
    };
    let error = format!("worker panicked: {detail}");
    match action {
        Action::Metadata => Event::MetadataFailed { index, error },
        Action::Thumb | Action::Develop(_) | Action::WarmDevelop(_) | Action::Rehydrate => {
            Event::ImageFailed { index, tier, error }
        }
    }
}

fn publish(shared: &Shared, event: Event) {
    let _ = shared.events.send(event);
    notify_safely(shared.notify.as_ref());
}

fn notify_safely(notify: &(dyn Fn() + Send + Sync)) {
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(notify)).is_err() {
        eprintln!("engine notification callback panicked; worker is continuing");
    }
}

fn retry_persistence_write(
    attempts: usize,
    mut write: impl FnMut() -> std::io::Result<()>,
    mut backoff: impl FnMut(usize),
) -> std::io::Result<()> {
    let attempts = attempts.max(1);
    for attempt in 0..attempts {
        match write() {
            Ok(()) => return Ok(()),
            Err(error) if attempt + 1 == attempts => return Err(error),
            Err(_) => backoff(attempt + 1),
        }
    }
    unreachable!("at least one persistence attempt always runs")
}

fn persistence_retry_backoff(retry: usize) {
    let multiplier = 1_u32 << retry.saturating_sub(1).min(2);
    std::thread::sleep(PERSIST_RETRY_BASE_DELAY.saturating_mul(multiplier));
}

fn persistence_worker(shared: &Shared) {
    while let Some(request) = shared.persistence.pop() {
        // This lane is intentionally single-threaded and yields before CPU
        // work so interactive develop workers retain scheduling priority.
        std::thread::yield_now();
        let quality = jpeg_quality(request.id.1);
        let encoded = encode_jpeg(&request.pixels, quality);
        let mut persistence_error = None;
        let encode_error = encoded.as_ref().err().cloned();
        if let Ok(bytes) = &encoded
            && let Some(disk) = &shared.disk
        {
            let key = DiskCache::key(&shared.entries[request.id.0], request.id.1);
            if let Err(error) = retry_persistence_write(
                PERSIST_WRITE_ATTEMPTS,
                || disk.put(&key, bytes),
                persistence_retry_backoff,
            ) {
                persistence_error = Some(error.to_string());
            }
        }

        let completion = shared.persistence.finish(request.id);
        // One completed request admits at most one parked warm retry. Waiting
        // until completion also covers producers that collided with `pop` or
        // `finish` without creating a two-job admission burst.
        let available_bytes = shared.persistence.available_pending_bytes();
        shared.heavy.release_one_deferred(available_bytes);
        if completion.insert_ram
            && let Ok(bytes) = encoded
        {
            shared.cache.insert_jpeg(request.id, Arc::new(bytes));
        }
        if let Some(error) = persistence_error {
            let context = if completion.warm_completion {
                "background disk-cache warm"
            } else {
                "disk cache"
            };
            eprintln!("{context} write failed after {PERSIST_WRITE_ATTEMPTS} attempts: {error}");
        }
        if let Some(error) = encode_error {
            let context = if completion.warm_completion {
                "background disk-cache warm"
            } else {
                "disk cache"
            };
            eprintln!("{context} JPEG encode failed: {error}");
        }
    }
}

fn run_job(shared: &Shared, id: JobId, action: Action, token: &CancelToken) -> JobCompletion {
    let (index, tier) = id;
    match action {
        Action::Metadata => run_metadata(shared, index, token),
        Action::Thumb => run_thumb(shared, index),
        Action::Rehydrate => run_rehydrate(shared, index, tier, token),
        Action::Develop(quality) => {
            let _ = run_develop(shared, index, tier, quality, token, false);
        }
        Action::WarmDevelop(quality) => {
            return run_warm_develop(shared, index, tier, quality, token);
        }
    }
    JobCompletion::Complete
}

fn run_metadata(shared: &Shared, index: usize, token: &CancelToken) {
    if token.cancelled() {
        return;
    }
    match decode::metadata(&shared.entries[index].path) {
        Ok(meta) if !token.cancelled() => publish(
            shared,
            Event::MetadataReady {
                index,
                meta: Box::new(meta),
            },
        ),
        Ok(_) => {}
        Err(error) if !token.cancelled() => publish(
            shared,
            Event::MetadataFailed {
                index,
                error: error.to_string(),
            },
        ),
        Err(_) => {}
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DevelopCompletion {
    Finished,
    Cancelled,
    Persistence {
        outcome: PersistenceEnqueue,
        retained_bytes: usize,
    },
}

fn warm_job_completion(completion: DevelopCompletion) -> JobCompletion {
    match completion {
        DevelopCompletion::Cancelled => JobCompletion::RetryBackground,
        DevelopCompletion::Persistence {
            outcome: PersistenceEnqueue::Busy | PersistenceEnqueue::Saturated,
            retained_bytes,
        } => JobCompletion::DeferBackground {
            required_bytes: retained_bytes,
        },
        DevelopCompletion::Finished
        | DevelopCompletion::Persistence {
            outcome:
                PersistenceEnqueue::Queued
                | PersistenceEnqueue::Coalesced
                | PersistenceEnqueue::Oversized
                | PersistenceEnqueue::Closed,
            ..
        } => JobCompletion::Complete,
    }
}

/// Probe background cache and persistence state away from the navigation/UI
/// thread. An active or pending encode satisfies this one-shot attempt without
/// starting another RAW decode; backpressure parks the item until capacity
/// changes.
fn run_warm_develop(
    shared: &Shared,
    index: usize,
    tier: Tier,
    quality: Quality,
    token: &CancelToken,
) -> JobCompletion {
    if token.cancelled() {
        return JobCompletion::RetryBackground;
    }
    if shared.cache.has_jpeg((index, tier)) {
        return JobCompletion::Complete;
    }
    let Some(disk) = &shared.disk else {
        return JobCompletion::Complete;
    };
    if disk.has(&DiskCache::key(&shared.entries[index], tier)) {
        return JobCompletion::Complete;
    }
    if shared
        .persistence
        .mark_warm_completion_if_present((index, tier))
    {
        return JobCompletion::Complete;
    }
    warm_job_completion(run_develop(shared, index, tier, quality, token, true))
}

fn run_thumb(shared: &Shared, index: usize) {
    let path = &shared.entries[index].path;
    complete_thumb_attempt(
        index,
        decode::thumb_and_meta(path, 360),
        || decode::metadata(path),
        |thumb| {
            shared
                .cache
                .insert_rgba((index, Tier::Thumb), Arc::new(thumb));
        },
        |event| publish(shared, event),
    );
}

/// Publish a thumbnail attempt while preserving metadata coverage when a RAW
/// container has readable metadata but no usable embedded preview. The
/// fallback is lazy so a successful thumbnail never parses metadata twice.
fn complete_thumb_attempt(
    index: usize,
    attempt: Result<decode::ThumbResult, decode::DecodeError>,
    fallback_metadata: impl FnOnce() -> Result<FileMeta, decode::DecodeError>,
    install_thumb: impl FnOnce(PixelBuf),
    mut emit: impl FnMut(Event),
) {
    match attempt {
        Ok(result) => {
            install_thumb(result.thumb);
            emit(Event::ThumbReady {
                index,
                meta: Box::new(result.meta),
            });
        }
        Err(error) => {
            emit(Event::ImageFailed {
                index,
                tier: Tier::Thumb,
                error: error.to_string(),
            });
            match fallback_metadata() {
                Ok(meta) => emit(Event::MetadataReady {
                    index,
                    meta: Box::new(meta),
                }),
                Err(error) => emit(Event::MetadataFailed {
                    index,
                    error: error.to_string(),
                }),
            }
        }
    }
}

fn run_develop(
    shared: &Shared,
    index: usize,
    tier: Tier,
    quality: Quality,
    token: &CancelToken,
    warm_only: bool,
) -> DevelopCompletion {
    let path = &shared.entries[index].path;
    let fail = |e: String| {
        if !token.cancelled() {
            publish(
                shared,
                Event::ImageFailed {
                    index,
                    tier,
                    error: e,
                },
            );
        }
    };
    if token.cancelled() {
        return DevelopCompletion::Cancelled;
    }
    let decoded = match decode::load(path) {
        Ok(d) => d,
        Err(e) => {
            fail(e.to_string());
            return if token.cancelled() {
                DevelopCompletion::Cancelled
            } else {
                DevelopCompletion::Finished
            };
        }
    };
    let meta = FileMeta::from_metadata(&decoded.metadata);
    if token.cancelled() {
        return DevelopCompletion::Cancelled;
    }
    let (buf, _) = match develop(decoded.raw, quality) {
        Ok(r) => r,
        Err(e) => {
            fail(e.to_string());
            return if token.cancelled() {
                DevelopCompletion::Cancelled
            } else {
                DevelopCompletion::Finished
            };
        }
    };
    let buf = Arc::new(apply_orient(buf, meta.orient));

    // Completed work always lands in the cache; the event is suppressed
    // for cancelled jobs so the UI never sees stale publishes.
    if !warm_only {
        shared.cache.insert_rgba((index, tier), buf.clone());
        if !token.cancelled() {
            publish(shared, Event::ImageReady { index, tier });
        }
    }

    // Ring-2 + ring-3 insurance runs on a bounded background lane. ImageReady
    // can trigger a replan that cancels this token immediately; persistence is
    // deliberately independent of that token once completed pixels enqueue.
    if warm_only || !shared.cache.has_jpeg((index, tier)) {
        let retained_bytes = buf.byte_len();
        let enqueue = shared.persistence.enqueue(PersistenceRequest {
            id: (index, tier),
            pixels: buf,
            insert_ram: !warm_only,
            warm_completion: warm_only,
        });
        if enqueue == PersistenceEnqueue::Oversized {
            eprintln!(
                "disk cache persistence skipped: {retained_bytes} byte buffer exceeds the {} byte pending budget",
                shared.persistence.pending_budget_bytes
            );
        }
        return DevelopCompletion::Persistence {
            outcome: enqueue,
            retained_bytes,
        };
    }
    DevelopCompletion::Finished
}

fn run_rehydrate(shared: &Shared, index: usize, tier: Tier, token: &CancelToken) {
    // Ring 2 first, then ring 3 (disk). Disk bytes enter RAM only after JPEG
    // validation; a corrupt rebuildable object is evicted and falls through
    // to RAW development instead of poisoning every later request.
    if token.cancelled() {
        return;
    }
    let id = (index, tier);
    if let Some(bytes) = shared.cache.get_jpeg(id) {
        if let Ok(buf) = decode_jpeg(&bytes) {
            return install_rehydrated(shared, index, tier, buf, token);
        }
        shared.cache.remove_jpeg(id);
    }

    if token.cancelled() {
        return;
    }
    if let Some(disk) = &shared.disk {
        let key = DiskCache::key(&shared.entries[index], tier);
        if let Some(bytes) = disk.get(&key) {
            if token.cancelled() {
                return;
            }
            if let Ok(buf) = decode_jpeg(&bytes) {
                shared.cache.insert_jpeg(id, Arc::new(bytes));
                return install_rehydrated(shared, index, tier, buf, token);
            }
            if let Err(error) = disk.remove(&key) {
                eprintln!("failed to remove corrupt disk cache object: {error}");
            }
        }
    }

    develop_cache_miss(shared, index, tier, token);
}

fn install_rehydrated(
    shared: &Shared,
    index: usize,
    tier: Tier,
    buf: PixelBuf,
    token: &CancelToken,
) {
    shared.cache.insert_rgba((index, tier), Arc::new(buf));
    if !token.cancelled() {
        publish(shared, Event::ImageReady { index, tier });
    }
}

fn develop_cache_miss(shared: &Shared, index: usize, tier: Tier, token: &CancelToken) {
    let quality = match tier {
        Tier::Full => Quality::Full,
        _ => Quality::Browse,
    };
    run_develop(shared, index, tier, quality, token, false);
}

/// Encodes a tightly packed RGBA8 buffer as a JPEG.
///
/// `quality` is passed directly to `jpeg-encoder`. The output discards alpha.
///
/// # Errors
///
/// Returns a string error if either dimension exceeds `u16`, the RGBA storage
/// is inconsistent with the dimensions, or JPEG encoding fails.
pub fn encode_jpeg(buf: &PixelBuf, quality: u8) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let encoder = jpeg_encoder::Encoder::new(&mut out, quality);
    encoder
        .encode(
            &buf.rgba,
            u16::try_from(buf.width).map_err(|e| e.to_string())?,
            u16::try_from(buf.height).map_err(|e| e.to_string())?,
            jpeg_encoder::ColorType::Rgba,
        )
        .map_err(|e| e.to_string())?;
    Ok(out)
}

/// Decodes JPEG bytes into a tightly packed RGBA8 buffer.
///
/// # Errors
///
/// Returns a human-readable string for malformed or unsupported JPEG data, or
/// when the decoder does not report dimensions.
pub fn decode_jpeg(bytes: &[u8]) -> Result<PixelBuf, String> {
    use zune_jpeg::JpegDecoder;
    use zune_jpeg::zune_core::colorspace::ColorSpace;
    use zune_jpeg::zune_core::options::DecoderOptions;

    let options = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGBA);
    let mut decoder = JpegDecoder::new_with_options(std::io::Cursor::new(bytes), options);
    let pixels = decoder.decode().map_err(|e| e.to_string())?;
    let (w, h) = decoder
        .dimensions()
        .ok_or_else(|| "no dimensions".to_string())?;
    Ok(PixelBuf {
        width: w as u32,
        height: h as u32,
        rgba: pixels,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn notification_callback_panic_is_contained() {
        let calls = AtomicUsize::new(0);
        let notify = || {
            calls.fetch_add(1, Ordering::Relaxed);
            panic!("expected notification callback panic");
        };

        notify_safely(&notify);

        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn claimed_job_panic_clears_in_flight_and_the_next_job_runs() {
        let queue = JobQueue::new();
        queue.extend([
            ((3, Tier::Thumb), 0, 0, Action::Metadata),
            ((4, Tier::Thumb), 1, 0, Action::Thumb),
        ]);

        let (failed_id, failed_action, failed_token) = queue.pop().unwrap();
        assert_eq!(
            (failed_id, failed_action),
            ((3, Tier::Thumb), Action::Metadata)
        );
        let mut events = Vec::new();
        let deferred = execute_claimed_job(
            &queue,
            failed_id,
            failed_action,
            &failed_token,
            || panic!("deterministic decoder panic"),
            |event| events.push(event),
        );

        assert_eq!(deferred, None);
        assert!(
            !queue
                .state
                .lock()
                .unwrap()
                .in_flight
                .contains_key(&failed_id)
        );
        assert!(matches!(
            &events[..],
            [Event::MetadataFailed { index: 3, error }]
                if error == "worker panicked: deterministic decoder panic"
        ));

        let (next_id, next_action, next_token) = queue.pop().unwrap();
        assert_eq!((next_id, next_action), ((4, Tier::Thumb), Action::Thumb));
        let mut next_ran = false;
        let deferred = execute_claimed_job(
            &queue,
            next_id,
            next_action,
            &next_token,
            || {
                next_ran = true;
                JobCompletion::Complete
            },
            |_| panic!("a successful job must not emit a panic event"),
        );
        assert!(next_ran);
        assert_eq!(deferred, None);
        assert!(queue.state.lock().unwrap().in_flight.is_empty());
    }

    #[test]
    fn non_metadata_worker_panic_is_an_image_failure_for_the_claimed_tier() {
        let payload = String::from("rehydrate invariant failed");
        let event = worker_panic_event(
            (8, Tier::Full),
            Action::Rehydrate,
            &payload as &(dyn std::any::Any + Send),
        );

        assert!(matches!(
            event,
            Event::ImageFailed {
                index: 8,
                tier: Tier::Full,
                error,
            } if error == "worker panicked: rehydrate invariant failed"
        ));
    }

    fn patterned_buf(width: u32, height: u32) -> PixelBuf {
        let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
        for y in 0..height {
            for x in 0..width {
                rgba.extend_from_slice(&[
                    (x * 255 / width.max(1)) as u8,
                    (y * 255 / height.max(1)) as u8,
                    ((x + y) * 255 / (width + height).max(1)) as u8,
                    255,
                ]);
            }
        }
        PixelBuf {
            width,
            height,
            rgba,
        }
    }

    fn job(id: JobId, class: u8, dist: u32) -> (JobId, u8, u32, Action) {
        (id, class, dist, Action::Thumb)
    }

    fn warm(id: JobId) -> (JobId, Action) {
        (id, Action::WarmDevelop(Quality::Browse))
    }

    fn persistence_request(
        id: JobId,
        pixels: Arc<PixelBuf>,
        insert_ram: bool,
    ) -> PersistenceRequest {
        PersistenceRequest {
            id,
            pixels,
            insert_ram,
            warm_completion: !insert_ram,
        }
    }

    fn persistence_shared(
        entries: Vec<FolderEntry>,
        cache: Arc<RamCache>,
        disk: Option<DiskCache>,
        pending_budget_bytes: usize,
    ) -> Arc<Shared> {
        let (events, _receiver) = std::sync::mpsc::channel();
        Arc::new(Shared {
            entries: Arc::new(entries),
            cache,
            disk,
            events,
            notify: Arc::new(|| {}),
            heavy: JobQueue::new(),
            light: JobQueue::new(),
            persistence: PersistenceQueue::with_budget(pending_budget_bytes),
            navigation: Mutex::new(NavigationOrder::default()),
        })
    }

    fn entry(path: impl Into<std::path::PathBuf>, size: u64) -> FolderEntry {
        let path = path.into();
        FolderEntry {
            file_name: path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            path,
            size,
            mtime_ns: 456,
        }
    }

    #[test]
    fn successful_thumbnail_emits_one_ready_event_without_metadata_fallback() {
        let mut fallback_called = false;
        let mut installed = None;
        let mut events = Vec::new();
        complete_thumb_attempt(
            7,
            Ok(decode::ThumbResult {
                thumb: patterned_buf(3, 2),
                meta: FileMeta {
                    camera: "success camera".into(),
                    ..FileMeta::default()
                },
            }),
            || {
                fallback_called = true;
                Err(decode::DecodeError::NoThumb)
            },
            |thumb| installed = Some(thumb),
            |event| events.push(event),
        );

        assert!(!fallback_called);
        assert_eq!(installed.as_ref().map(|thumb| thumb.width), Some(3));
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            Event::ThumbReady { index: 7, meta } if meta.camera == "success camera"
        ));
    }

    #[test]
    fn failed_thumbnail_still_emits_failure_and_recovers_metadata() {
        let mut fallback_called = false;
        let mut install_called = false;
        let mut events = Vec::new();
        complete_thumb_attempt(
            11,
            Err(decode::DecodeError::NoThumb),
            || {
                fallback_called = true;
                Ok(FileMeta {
                    rating: Some(4),
                    camera: "metadata camera".into(),
                    ..FileMeta::default()
                })
            },
            |_| install_called = true,
            |event| events.push(event),
        );

        assert!(fallback_called);
        assert!(!install_called);
        assert_eq!(events.len(), 2);
        assert!(matches!(
            &events[0],
            Event::ImageFailed {
                index: 11,
                tier: Tier::Thumb,
                error,
            } if error == "no embedded preview or thumbnail"
        ));
        assert!(matches!(
            &events[1],
            Event::MetadataReady { index: 11, meta }
                if meta.rating == Some(4) && meta.camera == "metadata camera"
        ));
    }

    #[test]
    fn failed_thumbnail_reports_metadata_fallback_failure() {
        let mut events = Vec::new();
        complete_thumb_attempt(
            13,
            Err(decode::DecodeError::NoThumb),
            || Err(decode::DecodeError::NoThumb),
            |_| panic!("a failed thumbnail must not be installed"),
            |event| events.push(event),
        );

        assert_eq!(events.len(), 2);
        assert!(matches!(
            events[0],
            Event::ImageFailed {
                index: 13,
                tier: Tier::Thumb,
                ..
            }
        ));
        assert!(matches!(events[1], Event::MetadataFailed { index: 13, .. }));
    }

    #[test]
    fn fit_pins_skip_full_while_zoomed_pins_preserve_the_near_window() {
        let fit = navigation_pins(5, 2, false, &[]);
        assert_eq!(fit.len(), 6);
        assert!(fit.iter().all(|(_, tier)| *tier != Tier::Full));

        let zoomed = navigation_pins(5, 2, true, &[]);
        let full: Vec<_> = zoomed
            .iter()
            .filter(|(_, tier)| *tier == Tier::Full)
            .copied()
            .collect();
        assert_eq!(full, [(1, Tier::Full), (2, Tier::Full), (3, Tier::Full)]);
    }

    #[test]
    fn filtered_pins_follow_visible_neighbors() {
        let pins = navigation_pins(10, 4, false, &[1, 4, 8]);
        let browse: Vec<_> = pins
            .iter()
            .filter(|(_, tier)| *tier == Tier::Browse)
            .map(|(index, _)| *index)
            .collect();
        assert_eq!(browse, [1, 4, 8]);
        assert!(!pins.iter().any(|(index, _)| [3, 5].contains(index)));
    }

    #[test]
    fn persistent_warm_jobs_are_a_valid_folder_permutation() {
        for len in 0_usize..=32 {
            for start in 0..=len.saturating_add(1) {
                let jobs: Vec<_> = background_warm_jobs(len, start).collect();
                assert_eq!(jobs.len(), len);
                assert!(jobs.iter().all(|((index, tier), action)| {
                    *index < len
                        && *tier == Tier::Browse
                        && *action == Action::WarmDevelop(Quality::Browse)
                }));
                let mut indices: Vec<_> = jobs.into_iter().map(|(id, _)| id.0).collect();
                indices.sort_unstable();
                indices.dedup();
                assert_eq!(indices, (0..len).collect::<Vec<_>>());
            }
        }
    }

    #[test]
    fn filter_sequence_change_invalidates_navigation_generation() {
        let mut order = NavigationOrder::default();
        let nav = NavState {
            current: 4,
            direction: 1,
            zoomed: false,
        };
        assert!(order.update_navigation(nav));
        assert!(!order.update_navigation(nav));

        order.replace_indices(vec![1, 4, 8]);
        assert_eq!(order.indices, [1, 4, 8]);
        assert!(order.update_navigation(nav));
    }

    #[test]
    fn interactive_plan_precedes_persistent_background_initialized_once() {
        let q = JobQueue::new();
        let background = (7, Tier::Browse);
        let ignored = (8, Tier::Browse);
        let interactive = (1, Tier::Browse);
        assert!(q.initialize_background(|| [warm(background)]));
        assert!(!q.initialize_background(|| [warm(ignored)]));
        q.set_plan(vec![job(interactive, 0, 0)], false);

        let (first, action, first_token) = q.pop().unwrap();
        assert_eq!((first, action), (interactive, Action::Thumb));
        q.finish(first, &first_token);

        let (second, action, second_token) = q.pop().unwrap();
        assert_eq!(
            (second, action),
            (background, Action::WarmDevelop(Quality::Browse))
        );
        q.finish(second, &second_token);
        assert!(q.state.lock().unwrap().background.is_empty());
    }

    #[test]
    fn foreground_replacement_requeues_cancelled_background_generation() {
        let q = JobQueue::new();
        let displaced = (7, Tier::Browse);
        let other = (8, Tier::Browse);
        assert!(q.initialize_background(|| [warm(displaced), warm(other)]));

        let (id, action, background_token) = q.pop().unwrap();
        assert_eq!(
            (id, action),
            (displaced, Action::WarmDevelop(Quality::Browse))
        );

        // Even a same-navigation replan must replace a warm action when this
        // exact image becomes interactive.
        q.set_plan(vec![(id, 0, 0, Action::Develop(Quality::Browse))], false);
        assert!(background_token.cancelled());
        let (foreground_id, foreground_action, foreground_token) = q.pop().unwrap();
        assert_eq!(foreground_id, id);
        assert_eq!(foreground_action, Action::Develop(Quality::Browse));

        // The stale warm completion cannot erase the foreground generation,
        // and its canceled item returns behind the untouched background item.
        q.finish_with(id, &background_token, JobCompletion::RetryBackground);
        assert!(
            q.state
                .lock()
                .unwrap()
                .in_flight
                .get(&id)
                .is_some_and(|current| Arc::ptr_eq(&current.token, &foreground_token))
        );
        q.finish(foreground_id, &foreground_token);

        let (next, _, next_token) = q.pop().unwrap();
        assert_eq!(next, other);
        q.finish(next, &next_token);
        let (retried, retry_action, retry_token) = q.pop().unwrap();
        assert_eq!(
            (retried, retry_action),
            (displaced, Action::WarmDevelop(Quality::Browse))
        );
        q.finish(retried, &retry_token);
    }

    #[test]
    fn same_navigation_replan_keeps_unrelated_background_running() {
        let q = JobQueue::new();
        let background = (7, Tier::Browse);
        let interactive = (1, Tier::Browse);
        assert!(q.initialize_background(|| [warm(background)]));
        let (_, _, background_token) = q.pop().unwrap();

        q.set_plan(vec![job(interactive, 0, 0)], false);
        assert!(!background_token.cancelled());
        let (id, _, interactive_token) = q.pop().unwrap();
        assert_eq!(id, interactive);
        q.finish(id, &interactive_token);
        q.finish(background, &background_token);
    }

    #[test]
    fn shutdown_discards_cancelled_background_retry() {
        let q = JobQueue::new();
        let id = (3, Tier::Browse);
        assert!(q.initialize_background(|| [warm(id)]));
        let (_, _, token) = q.pop().unwrap();

        q.close();
        assert!(token.cancelled());
        q.finish(id, &token);
        let state = q.state.lock().unwrap();
        assert!(state.closed);
        assert!(state.background.is_empty());
        assert!(state.background_in_flight.is_empty());
        drop(state);
        assert!(q.pop().is_none());
    }

    #[test]
    fn busy_and_saturated_warm_work_parks_until_capacity_changes() {
        assert_eq!(
            warm_job_completion(DevelopCompletion::Persistence {
                outcome: PersistenceEnqueue::Busy,
                retained_bytes: 64,
            }),
            JobCompletion::DeferBackground { required_bytes: 64 }
        );
        assert_eq!(
            warm_job_completion(DevelopCompletion::Persistence {
                outcome: PersistenceEnqueue::Saturated,
                retained_bytes: 64,
            }),
            JobCompletion::DeferBackground { required_bytes: 64 }
        );

        let q = JobQueue::new();
        let first = (3, Tier::Browse);
        let second = (4, Tier::Browse);
        assert!(q.initialize_background(|| [warm(first), warm(second)]));
        for expected in [first, second] {
            let (id, _, token) = q.pop().unwrap();
            assert_eq!(id, expected);
            q.finish_with(
                id,
                &token,
                JobCompletion::DeferBackground { required_bytes: 64 },
            );
        }

        {
            let state = q.state.lock().unwrap();
            assert!(state.background.is_empty());
            assert_eq!(state.background_deferred.len(), 2);
        }
        assert!(!q.release_one_deferred(63));
        assert!(q.release_one_deferred(64));
        {
            let state = q.state.lock().unwrap();
            assert_eq!(state.background.len(), 1);
            assert_eq!(state.background_deferred.len(), 1);
        }

        let (id, _, token) = q.pop().unwrap();
        assert_eq!(id, first);
        q.finish_with(id, &token, JobCompletion::Complete);
        assert!(q.release_one_deferred(64));
        let (id, _, token) = q.pop().unwrap();
        assert_eq!(id, second);
        q.finish_with(id, &token, JobCompletion::Complete);

        let state = q.state.lock().unwrap();
        assert!(state.background.is_empty());
        assert!(state.background_deferred.is_empty());
        assert!(state.background_in_flight.is_empty());
    }

    #[test]
    fn cancellation_after_persistence_handoff_does_not_redevelop_raw() {
        let q = JobQueue::new();
        let id = (3, Tier::Browse);
        assert!(q.initialize_background(|| [warm(id)]));
        let (_, _, token) = q.pop().unwrap();
        token.cancel();

        let completion = warm_job_completion(DevelopCompletion::Persistence {
            outcome: PersistenceEnqueue::Queued,
            retained_bytes: 64,
        });
        assert_eq!(completion, JobCompletion::Complete);
        q.finish_with(id, &token, completion);

        let state = q.state.lock().unwrap();
        assert!(state.background.is_empty());
        assert!(state.background_deferred.is_empty());
        assert!(state.background_in_flight.is_empty());
    }

    #[test]
    fn saturated_warm_self_releases_when_capacity_freed_before_park() {
        let persistence = PersistenceQueue::with_budget(64);
        assert_eq!(
            persistence.try_enqueue(persistence_request(
                (0, Tier::Browse),
                Arc::new(patterned_buf(4, 4)),
                false,
            )),
            PersistenceEnqueue::Queued
        );
        let rejected = persistence_request((1, Tier::Browse), Arc::new(patterned_buf(4, 4)), false);
        assert_eq!(
            persistence.try_enqueue(rejected),
            PersistenceEnqueue::Saturated
        );

        // Persistence frees pending capacity before the warm worker records
        // its deferred disposition, so the earlier completion notification
        // cannot release it.
        let active = persistence.pop().unwrap();
        let q = JobQueue::new();
        let id = (1, Tier::Browse);
        assert!(q.initialize_background(|| [warm(id)]));
        let (_, _, token) = q.pop().unwrap();
        q.finish_with(
            id,
            &token,
            JobCompletion::DeferBackground { required_bytes: 64 },
        );

        let available_bytes = persistence.available_pending_bytes();
        assert_eq!(available_bytes, 64);
        assert!(q.release_one_deferred(available_bytes));
        let (retried, _, retried_token) = q.pop().unwrap();
        assert_eq!(retried, id);
        q.finish(retried, &retried_token);
        let _ = persistence.finish(active.id);
    }

    #[test]
    fn oversized_warm_buffer_is_not_deferred_forever() {
        let queue = PersistenceQueue::with_budget(63);
        let enqueue = queue.try_enqueue(persistence_request(
            (0, Tier::Browse),
            Arc::new(patterned_buf(4, 4)),
            false,
        ));
        assert_eq!(enqueue, PersistenceEnqueue::Oversized);
        assert_eq!(
            warm_job_completion(DevelopCompletion::Persistence {
                outcome: enqueue,
                retained_bytes: 64,
            }),
            JobCompletion::Complete
        );
        let state = queue.state.lock().unwrap();
        assert!(state.pending.is_empty());
        assert_eq!(state.pending_bytes, 0);
    }

    #[test]
    fn pops_in_priority_order() {
        let q = JobQueue::new();
        q.set_plan(
            vec![
                job((2, Tier::Browse), 4, 6),
                job((0, Tier::Browse), 0, 0),
                job((1, Tier::Browse), 2, 1),
            ],
            true,
        );
        assert_eq!(q.pop().unwrap().0, (0, Tier::Browse));
        assert_eq!(q.pop().unwrap().0, (1, Tier::Browse));
        assert_eq!(q.pop().unwrap().0, (2, Tier::Browse));
    }

    #[test]
    fn bulk_heapify_preserves_priority_and_fifo_semantics() {
        let q = JobQueue::new();
        let plan: Vec<_> = (0..512)
            .map(|index| {
                job(
                    (index, Tier::Browse),
                    ((index * 17) % 7) as u8,
                    ((index * 29) % 31) as u32,
                )
            })
            .collect();
        let mut expected: Vec<_> = plan
            .iter()
            .enumerate()
            .map(|(sequence, (id, class, distance, _))| (*class, *distance, sequence, *id))
            .collect();
        expected.sort_unstable();

        q.set_plan(plan, true);

        for (_, _, _, expected_id) in expected {
            assert_eq!(q.pop().unwrap().0, expected_id);
        }
        let state = q.state.lock().unwrap();
        assert!(state.heap.is_empty());
        assert!(state.queued.is_empty());
    }

    #[test]
    fn forward_bias_orders_wave() {
        // Same class: eff_dist decides; backward×3 means +1,+2,+3 beat -1.
        let q = JobQueue::new();
        q.set_plan(
            vec![
                job((9, Tier::Browse), 4, 3), // backward dist 1 → eff 3
                job((11, Tier::Browse), 4, 1),
                job((12, Tier::Browse), 4, 2),
            ],
            true,
        );
        assert_eq!(q.pop().unwrap().0.0, 11);
        assert_eq!(q.pop().unwrap().0.0, 12);
        assert_eq!(q.pop().unwrap().0.0, 9);
    }

    #[test]
    fn replan_supersedes_queued_jobs() {
        let q = JobQueue::new();
        q.set_plan(
            vec![job((0, Tier::Browse), 1, 0), job((1, Tier::Browse), 2, 0)],
            true,
        );
        // New plan drops job 0 entirely.
        q.set_plan(vec![job((1, Tier::Browse), 1, 0)], true);
        assert_eq!(q.pop().unwrap().0, (1, Tier::Browse));
        assert!(
            q.state.lock().unwrap().heap.is_empty() || {
                // Any remaining entries must be inert (stale epoch).
                let state = q.state.lock().unwrap();
                state.queued.is_empty()
            }
        );
    }

    #[test]
    fn urgent_thumbnail_precedes_background_without_rewriting_the_heap() {
        let q = JobQueue::new();
        let requested = (9, Tier::Thumb);
        let other = (1, Tier::Thumb);
        q.extend([job(requested, 5, 100), job(other, 5, 1)]);
        let background_heap_len = q.state.lock().unwrap().heap.len();

        assert!(q.set_urgent([(requested, Action::Thumb)]));
        assert_eq!(q.state.lock().unwrap().heap.len(), background_heap_len);
        let (first, _, first_token) = q.pop().unwrap();
        assert_eq!(first, requested);
        q.finish(first, &first_token);

        let (second, _, second_token) = q.pop().unwrap();
        assert_eq!(second, other);
        q.finish(second, &second_token);

        let state = q.state.lock().unwrap();
        assert!(state.queued.is_empty());
        assert!(state.heap.iter().all(|job| job.id == requested));
    }

    #[test]
    fn urgent_thumbnail_replaces_queued_metadata_for_the_same_image() {
        let q = JobQueue::new();
        let requested = (9, Tier::Thumb);
        q.extend([(requested, 5, 100, Action::Metadata)]);

        assert!(q.set_urgent([(requested, Action::Thumb)]));
        let (id, action, token) = q.pop().unwrap();
        assert_eq!(id, requested);
        assert_eq!(action, Action::Thumb);
        q.finish(id, &token);

        let state = q.state.lock().unwrap();
        assert!(state.queued.is_empty());
        assert!(state.heap.iter().all(|job| job.id == requested));
    }

    #[test]
    fn urgent_thumbnail_cancels_in_flight_metadata_generation() {
        let q = JobQueue::new();
        let requested = (9, Tier::Thumb);
        q.extend([(requested, 5, 100, Action::Metadata)]);
        let (_, action, metadata_token) = q.pop().unwrap();
        assert_eq!(action, Action::Metadata);

        assert!(q.set_urgent([(requested, Action::Thumb)]));
        let (id, action, thumbnail_token) = q.pop().unwrap();
        assert_eq!(id, requested);
        assert_eq!(action, Action::Thumb);
        assert!(metadata_token.cancelled());

        // The displaced generation can finish after the thumbnail starts,
        // but it must not erase the thumbnail's live queue state.
        q.finish(requested, &metadata_token);
        assert!(
            q.state
                .lock()
                .unwrap()
                .in_flight
                .get(&requested)
                .is_some_and(|current| Arc::ptr_eq(&current.token, &thumbnail_token))
        );
        q.finish(requested, &thumbnail_token);
    }

    #[test]
    fn replacing_urgent_viewport_drops_old_urgency_without_starvation() {
        let q = JobQueue::new();
        let old_viewport: Vec<_> = (0..50).map(|index| (index, Tier::Thumb)).collect();
        let newest = (999, Tier::Thumb);
        q.extend(
            old_viewport
                .iter()
                .enumerate()
                .map(|(distance, id)| job(*id, 5, distance as u32))
                .chain([job(newest, 5, 999)]),
        );

        assert!(q.set_urgent(old_viewport.iter().copied().map(|id| (id, Action::Thumb))));
        assert!(q.set_urgent([(newest, Action::Thumb)]));
        assert_eq!(q.state.lock().unwrap().urgent.len(), 1);

        let (first, _, token) = q.pop().unwrap();
        assert_eq!(first, newest);
        q.finish(first, &token);
    }

    #[test]
    fn urgent_viewport_reuses_a_live_thumbnail_job() {
        let q = JobQueue::new();
        let id = (7, Tier::Thumb);
        q.extend([job(id, 5, 10)]);
        let (_, _, live_token) = q.pop().unwrap();

        assert!(q.set_urgent([(id, Action::Thumb)]));
        let state = q.state.lock().unwrap();
        assert!(!live_token.cancelled());
        assert!(state.urgent.is_empty());
        assert!(state.heap.is_empty());
    }

    #[test]
    fn urgent_viewport_is_rejected_after_queue_shutdown() {
        let q = JobQueue::new();
        q.close();
        assert!(!q.set_urgent([((0, Tier::Thumb), Action::Thumb)]));
    }

    #[test]
    fn replan_cancels_unwanted_in_flight_and_keeps_wanted() {
        let q = JobQueue::new();
        q.set_plan(
            vec![job((0, Tier::Browse), 1, 0), job((1, Tier::Browse), 2, 0)],
            true,
        );
        let (id0, _, token0) = q.pop().unwrap();
        assert_eq!(id0, (0, Tier::Browse));
        // Job 0 is now in flight. Replan wants only job 1 → 0 cancelled.
        q.set_plan(vec![job((1, Tier::Browse), 1, 0)], true);
        assert!(token0.cancelled());

        // Re-wanting job 0 while its cancelled instance is still in flight:
        // a fresh instance must be queued (the dying one won't publish).
        q.set_plan(vec![job((0, Tier::Browse), 1, 0)], true);
        assert!(
            q.state
                .lock()
                .unwrap()
                .queued
                .contains_key(&(0, Tier::Browse))
        );

        // But a LIVE in-flight job is never duplicated.
        let q2 = JobQueue::new();
        q2.set_plan(vec![job((7, Tier::Browse), 1, 0)], true);
        let _live = q2.pop().unwrap();
        q2.set_plan(vec![job((7, Tier::Browse), 1, 0)], true);
        assert!(
            !q2.state
                .lock()
                .unwrap()
                .queued
                .contains_key(&(7, Tier::Browse))
        );
    }

    #[test]
    fn replan_replaces_live_job_when_action_changes() {
        let q = JobQueue::new();
        let id = (7, Tier::Browse);
        q.set_plan(
            vec![(id, 6, 20, Action::WarmDevelop(Quality::Browse))],
            true,
        );
        let (_, action, warm_token) = q.pop().unwrap();
        assert_eq!(action, Action::WarmDevelop(Quality::Browse));

        q.set_plan(vec![(id, 0, 0, Action::Develop(Quality::Browse))], true);
        assert!(warm_token.cancelled());

        let (_, action, display_token) = q.pop().unwrap();
        assert_eq!(action, Action::Develop(Quality::Browse));
        assert!(!display_token.cancelled());

        // Completion from the displaced warm generation must not erase the
        // foreground replacement.
        q.finish(id, &warm_token);
        assert!(
            q.state
                .lock()
                .unwrap()
                .in_flight
                .get(&id)
                .is_some_and(|current| Arc::ptr_eq(&current.token, &display_token))
        );
    }

    #[test]
    fn close_wakes_waiters_and_rejects_queued_work() {
        let q = Arc::new(JobQueue::new());
        let waiter_queue = q.clone();
        let waiter = std::thread::spawn(move || waiter_queue.pop());

        q.close();
        assert!(waiter.join().unwrap().is_none());

        q.set_plan(vec![job((0, Tier::Browse), 0, 0)], true);
        assert!(q.pop().is_none());
    }

    #[test]
    fn stale_completion_does_not_finish_replacement_generation() {
        let q = JobQueue::new();
        let id = (0, Tier::Browse);

        q.set_plan(vec![job(id, 1, 0)], true);
        let (_, _, stale) = q.pop().unwrap();
        q.set_plan(Vec::new(), true);
        assert!(stale.cancelled());

        q.set_plan(vec![job(id, 1, 0)], true);
        let (_, _, replacement) = q.pop().unwrap();
        assert!(!Arc::ptr_eq(&stale, &replacement));

        q.finish(id, &stale);
        assert!(
            q.state
                .lock()
                .unwrap()
                .in_flight
                .get(&id)
                .is_some_and(|current| Arc::ptr_eq(&current.token, &replacement))
        );

        q.finish(id, &replacement);
        assert!(!q.state.lock().unwrap().in_flight.contains_key(&id));
    }

    #[test]
    fn equal_priority_jobs_are_fifo() {
        let q = JobQueue::new();
        q.set_plan(
            vec![
                job((0, Tier::Browse), 1, 1),
                job((1, Tier::Browse), 1, 1),
                job((2, Tier::Browse), 1, 1),
            ],
            true,
        );
        assert_eq!(q.pop().unwrap().0.0, 0);
        assert_eq!(q.pop().unwrap().0.0, 1);
        assert_eq!(q.pop().unwrap().0.0, 2);
    }

    #[test]
    fn cancel_token_is_monotonic_and_idempotent() {
        let token = CancelToken::default();
        assert!(!token.cancelled());
        token.cancel();
        token.cancel();
        assert!(token.cancelled());
    }

    #[test]
    fn cached_warm_job_does_not_touch_the_raw_file() {
        let dir = tempfile::tempdir().unwrap();
        let entry = FolderEntry {
            path: dir.path().join("missing.arw"),
            file_name: "missing.arw".into(),
            size: 123,
            mtime_ns: 456,
        };
        let disk = DiskCache::open_at(dir.path().join("cache"));
        let key = DiskCache::key(&entry, Tier::Browse);
        disk.put(&key, b"already warm").unwrap();
        let (events, receiver) = std::sync::mpsc::channel();
        let shared = Shared {
            entries: Arc::new(vec![entry]),
            cache: Arc::new(RamCache::new(0, 0, 0)),
            disk: Some(disk.clone()),
            events,
            notify: Arc::new(|| {}),
            heavy: JobQueue::new(),
            light: JobQueue::new(),
            persistence: PersistenceQueue::new(),
            navigation: Mutex::new(NavigationOrder::default()),
        };

        run_warm_develop(
            &shared,
            0,
            Tier::Browse,
            Quality::Browse,
            &CancelToken::default(),
        );

        assert!(matches!(
            receiver.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        assert_eq!(disk.get(&key).unwrap(), b"already warm");
    }

    #[test]
    fn active_persistence_prevents_duplicate_warm_raw_decode() {
        let dir = tempfile::tempdir().unwrap();
        let entry = FolderEntry {
            path: dir.path().join("missing.arw"),
            file_name: "missing.arw".into(),
            size: 123,
            mtime_ns: 456,
        };
        let cache = Arc::new(RamCache::new(0, 0, 1024 * 1024));
        let shared = persistence_shared(
            vec![entry],
            cache,
            Some(DiskCache::open_at(dir.path().join("cache"))),
            1024 * 1024,
        );
        let id = (0, Tier::Browse);
        assert_eq!(
            shared.persistence.try_enqueue(persistence_request(
                id,
                Arc::new(patterned_buf(4, 4)),
                true,
            )),
            PersistenceEnqueue::Queued
        );
        let _active = shared.persistence.pop().unwrap();

        // The source path is deliberately missing. Reaching RAW decode would
        // fail; the active persistence request must instead absorb the warm
        // completion flag under the same queue lock.
        assert_eq!(
            run_warm_develop(
                &shared,
                0,
                Tier::Browse,
                Quality::Browse,
                &CancelToken::default(),
            ),
            JobCompletion::Complete
        );
        assert_eq!(
            shared.persistence.finish(id),
            PersistenceCompletion {
                insert_ram: true,
                warm_completion: true,
            }
        );
    }

    #[test]
    fn persistence_membership_marks_active_and_pending_warm_atomically() {
        let queue = PersistenceQueue::with_budget(128);
        let pixels = Arc::new(patterned_buf(4, 4));
        let active_id = (0, Tier::Browse);
        let pending_id = (1, Tier::Browse);
        assert_eq!(
            queue.try_enqueue(persistence_request(active_id, pixels.clone(), true)),
            PersistenceEnqueue::Queued
        );
        let _active = queue.pop().unwrap();
        assert_eq!(
            queue.try_enqueue(persistence_request(pending_id, pixels, true)),
            PersistenceEnqueue::Queued
        );

        assert!(queue.mark_warm_completion_if_present(active_id));
        assert!(queue.mark_warm_completion_if_present(pending_id));
        assert!(!queue.mark_warm_completion_if_present((2, Tier::Browse)));
        assert_eq!(
            queue.finish(active_id),
            PersistenceCompletion {
                insert_ram: true,
                warm_completion: true,
            }
        );
        assert_eq!(queue.pop().unwrap().id, pending_id);
        assert_eq!(
            queue.finish(pending_id),
            PersistenceCompletion {
                insert_ram: true,
                warm_completion: true,
            }
        );
    }

    #[test]
    fn persistence_write_retry_recovers_and_permanent_failure_terminates() {
        let mut attempts = 0;
        let mut backoffs = Vec::new();
        retry_persistence_write(
            PERSIST_WRITE_ATTEMPTS,
            || {
                attempts += 1;
                if attempts < PERSIST_WRITE_ATTEMPTS {
                    Err(std::io::Error::other("transient"))
                } else {
                    Ok(())
                }
            },
            |retry| backoffs.push(retry),
        )
        .unwrap();
        assert_eq!(attempts, PERSIST_WRITE_ATTEMPTS);
        assert_eq!(backoffs, [1, 2]);

        let mut attempts = 0;
        let mut backoffs = Vec::new();
        let error = retry_persistence_write(
            PERSIST_WRITE_ATTEMPTS,
            || {
                attempts += 1;
                Err(std::io::Error::other("permanent"))
            },
            |retry| backoffs.push(retry),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "permanent");
        assert_eq!(attempts, PERSIST_WRITE_ATTEMPTS);
        assert_eq!(backoffs, [1, 2]);
    }

    #[test]
    fn busy_enqueue_fallback_preserves_pixels_without_future_completion() {
        let queue = PersistenceQueue::with_budget(64);
        let request = persistence_request((0, Tier::Browse), Arc::new(patterned_buf(4, 4)), false);

        let guard = queue.state.lock().unwrap();
        assert_eq!(queue.try_enqueue(request.clone()), PersistenceEnqueue::Busy);
        drop(guard);

        // Nothing was active or pending to produce a later capacity wakeup.
        // The retained request can still take the short lock once and enqueue
        // without another RAW develop.
        assert_eq!(
            queue.enqueue_after_busy(request),
            PersistenceEnqueue::Queued
        );
        let accepted = queue.pop().unwrap();
        assert_eq!(accepted.id, (0, Tier::Browse));
        assert_eq!(
            queue.finish(accepted.id),
            PersistenceCompletion {
                insert_ram: false,
                warm_completion: true,
            }
        );
    }

    #[test]
    fn persistence_queue_is_bounded_nonblocking_and_coalesces_by_id() {
        let queue = PersistenceQueue::with_budget(64);
        let pixels = Arc::new(patterned_buf(4, 4));
        let id = (0, Tier::Browse);

        assert_eq!(
            queue.try_enqueue(persistence_request(id, pixels.clone(), false)),
            PersistenceEnqueue::Queued
        );
        let active = queue.pop().unwrap();
        assert_eq!(active.id, id);

        // A display request for the active warm item reuses its encoding and
        // upgrades the eventual cache action to include the RAM JPEG ring.
        assert_eq!(
            queue.try_enqueue(persistence_request(id, pixels.clone(), true)),
            PersistenceEnqueue::Coalesced
        );

        let pending_id = (1, Tier::Browse);
        assert_eq!(
            queue.try_enqueue(persistence_request(pending_id, pixels.clone(), false)),
            PersistenceEnqueue::Queued
        );
        assert_eq!(
            queue.try_enqueue(persistence_request(pending_id, pixels.clone(), true)),
            PersistenceEnqueue::Coalesced
        );
        assert_eq!(
            queue.try_enqueue(persistence_request(
                (2, Tier::Browse),
                Arc::new(patterned_buf(1, 1)),
                true,
            )),
            PersistenceEnqueue::Saturated
        );

        {
            let state = queue.state.lock().unwrap();
            assert_eq!(state.pending.len(), 1);
            assert_eq!(state.pending_bytes, 64);
        }
        assert_eq!(
            queue.finish(id),
            PersistenceCompletion {
                insert_ram: true,
                warm_completion: true,
            }
        );
        assert_eq!(queue.pop().unwrap().id, pending_id);
        assert_eq!(
            queue.finish(pending_id),
            PersistenceCompletion {
                insert_ram: true,
                warm_completion: true,
            }
        );

        // Contention cannot delay a heavy worker: enqueue fails immediately.
        let guard = queue.state.lock().unwrap();
        assert_eq!(
            queue.try_enqueue(persistence_request(id, pixels, true)),
            PersistenceEnqueue::Busy
        );
        drop(guard);

        queue.close();
        assert!(queue.pop().is_none());
    }

    #[test]
    fn production_persistence_budget_fits_a_61mp_rgba_frame() {
        let frame_61mp_bytes = std::hint::black_box(9_504 * 6_336 * 4);
        let budget = std::hint::black_box(PERSIST_PENDING_BUDGET_BYTES);
        assert!(budget >= frame_61mp_bytes);
        assert!(budget < frame_61mp_bytes * 2);
    }

    #[test]
    fn engine_shutdown_drains_persistence_and_obeys_warm_ram_policy() {
        let dir = tempfile::tempdir().unwrap();
        let disk = DiskCache::open_at(dir.path().join("cache"));
        let entries = vec![
            entry(dir.path().join("display.arw"), 100),
            entry(dir.path().join("warm.arw"), 200),
        ];
        let keys = [
            DiskCache::key(&entries[0], Tier::Browse),
            DiskCache::key(&entries[1], Tier::Browse),
        ];
        let cache = Arc::new(RamCache::new(0, 0, 1024 * 1024));
        let shared = persistence_shared(entries, cache.clone(), Some(disk.clone()), 1024 * 1024);

        assert_eq!(
            shared.persistence.try_enqueue(persistence_request(
                (0, Tier::Browse),
                Arc::new(patterned_buf(96, 64)),
                true,
            )),
            PersistenceEnqueue::Queued
        );
        assert_eq!(
            shared.persistence.try_enqueue(persistence_request(
                (1, Tier::Browse),
                Arc::new(patterned_buf(96, 64)),
                false,
            )),
            PersistenceEnqueue::Queued
        );

        let worker_shared = shared.clone();
        let worker = std::thread::spawn(move || persistence_worker(&worker_shared));
        // Engine shutdown rejects new work, drains both accepted requests, and
        // joins the persistence worker before returning.
        let engine = Engine {
            shared: shared.clone(),
            workers: Vec::new(),
            persistence_worker: Some(worker),
        };
        drop(engine);
        assert_eq!(
            shared.persistence.try_enqueue(persistence_request(
                (0, Tier::Full),
                Arc::new(patterned_buf(1, 1)),
                true,
            )),
            PersistenceEnqueue::Closed
        );

        for key in &keys {
            let persisted = disk.get(key).expect("accepted work must reach disk");
            let decoded = decode_jpeg(&persisted).unwrap();
            assert_eq!((decoded.width, decoded.height), (96, 64));
        }
        assert!(cache.has_jpeg((0, Tier::Browse)));
        assert!(!cache.has_jpeg((1, Tier::Browse)));
    }

    #[test]
    fn persistence_survives_replan_cancellation_after_pixels_complete() {
        let dir = tempfile::tempdir().unwrap();
        let disk = DiskCache::open_at(dir.path().join("cache"));
        let entries = vec![entry(dir.path().join("stale.arw"), 100)];
        let key = DiskCache::key(&entries[0], Tier::Browse);
        let cache = Arc::new(RamCache::new(0, 0, 1024 * 1024));
        let shared = persistence_shared(entries, cache.clone(), Some(disk.clone()), 1024 * 1024);

        shared
            .heavy
            .set_plan(vec![job((0, Tier::Browse), 1, 0)], true);
        let (_, _, token) = shared.heavy.pop().unwrap();
        assert_eq!(
            shared.persistence.try_enqueue(persistence_request(
                (0, Tier::Browse),
                Arc::new(patterned_buf(64, 48)),
                true,
            )),
            PersistenceEnqueue::Queued
        );

        shared.heavy.set_plan(Vec::new(), true);
        assert!(token.cancelled());
        let worker_shared = shared.clone();
        let worker = std::thread::spawn(move || persistence_worker(&worker_shared));
        shared.persistence.close();
        worker.join().unwrap();

        assert!(disk.has(&key));
        assert!(cache.has_jpeg((0, Tier::Browse)));
    }

    #[test]
    fn corrupt_cached_jpeg_is_evicted_before_raw_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let raw_entry = entry(dir.path().join("missing.arw"), 100);
        let disk = DiskCache::open_at(dir.path().join("cache"));
        let key = DiskCache::key(&raw_entry, Tier::Browse);
        disk.put(&key, b"not a jpeg").unwrap();

        let cache = Arc::new(RamCache::new(0, 0, 1024));
        cache.insert_jpeg((0, Tier::Browse), Arc::new(b"not a jpeg".to_vec()));
        let (events, receiver) = std::sync::mpsc::channel();
        let shared = Shared {
            entries: Arc::new(vec![raw_entry]),
            cache: cache.clone(),
            disk: Some(disk.clone()),
            events,
            notify: Arc::new(|| {}),
            heavy: JobQueue::new(),
            light: JobQueue::new(),
            persistence: PersistenceQueue::new(),
            navigation: Mutex::new(NavigationOrder::default()),
        };

        run_rehydrate(&shared, 0, Tier::Browse, &CancelToken::default());

        assert!(!cache.has_jpeg((0, Tier::Browse)));
        assert!(!disk.has(&key));
        assert!(matches!(
            receiver.try_recv(),
            Ok(Event::ImageFailed {
                index: 0,
                tier: Tier::Browse,
                ..
            })
        ));
    }

    #[test]
    fn jpeg_roundtrip_preserves_dimensions_alpha_and_visual_content() {
        let source = patterned_buf(96, 64);
        let encoded = encode_jpeg(&source, 90).unwrap();
        assert!(!encoded.is_empty());
        let decoded = decode_jpeg(&encoded).unwrap();
        assert_eq!(
            (decoded.width, decoded.height),
            (source.width, source.height)
        );
        assert_eq!(decoded.rgba.len(), source.rgba.len());
        assert!(decoded.rgba.chunks_exact(4).all(|pixel| pixel[3] == 255));

        let total_error: u64 = source
            .rgba
            .chunks_exact(4)
            .zip(decoded.rgba.chunks_exact(4))
            .map(|(expected, actual)| {
                (0..3)
                    .map(|channel| expected[channel].abs_diff(actual[channel]) as u64)
                    .sum::<u64>()
            })
            .sum();
        let channel_count = source.width as u64 * source.height as u64 * 3;
        assert!(
            total_error / channel_count < 5,
            "mean channel error was {}",
            total_error / channel_count
        );
    }

    #[test]
    fn jpeg_codec_rejects_invalid_inputs() {
        assert!(decode_jpeg(b"not a jpeg").is_err());
        let malformed = PixelBuf {
            width: 2,
            height: 2,
            rgba: vec![0; 15],
        };
        assert!(encode_jpeg(&malformed, 90).is_err());
        let too_wide = PixelBuf {
            width: u16::MAX as u32 + 1,
            height: 1,
            rgba: Vec::new(),
        };
        assert!(encode_jpeg(&too_wide, 90).is_err());
    }
}
