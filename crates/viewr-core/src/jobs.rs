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
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, OnceLock, TryLockError};
use std::time::Duration;

use crate::cache_disk::{DEFAULT_CACHE_JPEG_QUALITY, DiskCache};
use crate::cache_ram::RamCache;
#[cfg(test)]
use crate::cache_ram::RamCacheBudgets;
use crate::decode;
use crate::develop::{Quality, develop};
use crate::folder::{FolderEntry, outward_order};
use crate::meta::FileMeta;
#[cfg(feature = "benchmarks")]
use crate::planning::build_plan_targets;
use crate::planning::{
    BrowsePrefetchBudget, FullPrefetchBudget, NavigationPrefetchBudgets, PlanKind,
    build_plan_targets_with_normalized_prefetch,
};
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
/// Default JPEG quality used by persistent Browse and Full renders.
pub const CACHE_JPEG_QUALITY: u8 = DEFAULT_CACHE_JPEG_QUALITY;
/// Lowest cache JPEG quality exposed by the application preference.
pub const MIN_CACHE_JPEG_QUALITY: u8 = 80;
/// Highest cache JPEG quality exposed by the application preference.
pub const MAX_CACHE_JPEG_QUALITY: u8 = 100;
const MAX_JPEG_WORKERS: usize = 10;
const PERSIST_WRITE_ATTEMPTS: usize = 3;
const PERSIST_RETRY_BASE_DELAY: Duration = Duration::from_millis(2);

/// Returns the logical CPU capacity currently available to Viewr.
///
/// This respects operating-system affinity and container limits when the
/// platform reports them. At least one worker is always returned.
pub fn available_worker_threads() -> NonZeroUsize {
    std::thread::available_parallelism()
        .unwrap_or_else(|_| NonZeroUsize::new(1).expect("one is non-zero"))
}

/// Resolves zero-as-automatic or a fixed processing-thread limit against the
/// logical CPU capacity currently available to Viewr.
///
/// A fixed limit is a ceiling rather than a request to oversubscribe the host.
pub fn resolve_worker_threads(limit: usize) -> NonZeroUsize {
    resolve_worker_threads_for_available(limit, available_worker_threads().get())
}

fn resolve_worker_threads_for_available(limit: usize, available: usize) -> NonZeroUsize {
    let available = available.max(1);
    let resolved = if limit == 0 {
        available
    } else {
        limit.min(available)
    };
    NonZeroUsize::new(resolved).expect("resolved processing thread count is non-zero")
}

fn build_processing_pool(
    worker_threads: NonZeroUsize,
) -> Result<rayon::ThreadPool, rayon::ThreadPoolBuildError> {
    rayon::ThreadPoolBuilder::new()
        .num_threads(worker_threads.get())
        .thread_name(|index| format!("viewr-processing-{index}"))
        .build()
}

/// Returns the default production persistence quality for a benchmark tier.
#[cfg(feature = "benchmarks")]
#[doc(hidden)]
pub fn benchmark_jpeg_quality(_tier: Tier) -> u8 {
    CACHE_JPEG_QUALITY
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
    ///
    /// Full resolution fills its adaptive RAM working set in every mode. This
    /// flag raises the current Full render's priority.
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
    /// Populate only the adaptive Full RGBA working set. Existing RAM/disk
    /// JPEGs may be rehydrated, but a RAW miss is not encoded or persisted.
    PrefetchFull,
    /// Persist a resident Full buffer after it becomes required display work.
    PersistResident,
    /// Develop only to fill ring 2 + disk (P3 folder warm): no RGBA
    /// insert, no event — keeps far images from thrashing ring 1.
    WarmDevelop(Quality),
    Rehydrate,
}

impl Action {
    fn compatible_with(self, requested: Self) -> bool {
        self == requested
            || matches!(
                (self, requested),
                (
                    Self::PrefetchFull,
                    Self::Develop(Quality::Full) | Self::Rehydrate
                )
            )
    }

    fn is_speculative_full(self) -> bool {
        self == Self::PrefetchFull
    }
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
    /// A live optional Full attempt produced no resident pixels. Suppress the
    /// same speculative target until navigation changes or foreground demand
    /// explicitly promotes it.
    SuppressSpeculative,
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
    /// Optional Full targets that failed during the current navigation wave.
    /// This prevents unrelated readiness events from repeatedly reopening the
    /// same unreadable source.
    speculative_failures: HashSet<JobId>,
    epoch: u64,
    seq: u64,
    closed: bool,
}

struct JobQueue {
    state: Mutex<QueueState>,
    cond: Condvar,
    parallel_background_lanes: bool,
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
        Self::new_with_parallel_background_lanes(true)
    }

    fn new_with_parallel_background_lanes(parallel_background_lanes: bool) -> Self {
        Self {
            state: Mutex::new(QueueState::default()),
            cond: Condvar::new(),
            parallel_background_lanes,
        }
    }

    /// Replace the interactive job set. In-flight interactive jobs no longer
    /// wanted are cancelled; in-flight jobs still wanted keep running (not
    /// duplicated). A real navigation change can also cancel active
    /// background generations so newly interactive work reaches a worker at
    /// the next cancellation point.
    fn set_plan(&self, mut plan: Vec<(JobId, u8, u32, Action)>, navigation_changed: bool) {
        let mut state = self.state.lock().unwrap();
        if state.closed {
            return;
        }
        if navigation_changed {
            state.speculative_failures.clear();
        } else {
            // Required display demand must retry immediately even if the same
            // image failed as optional work earlier in this navigation wave.
            for (id, _, _, action) in &plan {
                if !action.is_speculative_full() {
                    state.speculative_failures.remove(id);
                }
            }
            plan.retain(|(id, _, _, action)| {
                !action.is_speculative_full() || !state.speculative_failures.contains(id)
            });
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
        if navigation_changed && !plan.is_empty() {
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
            if (!is_background
                && !wanted_action.is_some_and(|action| running.action.compatible_with(action)))
                || (is_background
                    && wanted_action.is_some_and(|action| !running.action.compatible_with(action)))
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
            if state.in_flight.get(&id).is_some_and(|running| {
                running.action.compatible_with(action) && !running.token.cancelled()
            }) {
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

    fn claim(
        state: &mut QueueState,
        id: JobId,
        action: Action,
    ) -> (JobId, Action, Arc<CancelToken>) {
        let token = Arc::new(CancelToken::default());
        state.in_flight.insert(
            id,
            InFlight {
                action,
                token: token.clone(),
            },
        );
        (id, action, token)
    }

    /// Claims the highest-priority runnable job without waiting.
    fn try_claim_locked(
        state: &mut QueueState,
        parallel_background_lanes: bool,
    ) -> Option<(JobId, Action, Arc<CancelToken>)> {
        let urgent_len = state.urgent.len();
        for _ in 0..urgent_len {
            let (id, action) = state
                .urgent
                .pop_front()
                .expect("urgent length was captured under the queue lock");
            if let Some(running) = state.in_flight.get(&id) {
                if running.action == action && !running.token.cancelled() {
                    continue;
                }
                // Cooperative cancellation cannot interrupt a decoder. Keep
                // the replacement pending until the old generation exits so
                // one file is never decoded concurrently by two workers.
                running.token.cancel();
                state.urgent.push_back((id, action));
                continue;
            }
            // Invalidate a background copy only when this urgent copy is
            // actually claimed. Until then, replacing the urgent viewport
            // leaves the original background priority untouched.
            state.queued.remove(&id);
            return Some(Self::claim(state, id, action));
        }

        let speculative_full_active = state.in_flight.iter().any(|(id, running)| {
            running.action.is_speculative_full()
                || (id.1 == Tier::Full && running.token.cancelled())
        });
        let mut blocked = Vec::new();
        let mut selected = None;
        while let Some(job) = state.heap.pop() {
            if state.queued.get(&job.id).map(|queued| queued.epoch) != Some(job.epoch) {
                continue; // superseded by a newer plan
            }
            if state.in_flight.contains_key(&job.id)
                || (job.action.is_speculative_full() && speculative_full_active)
                || (job.action.is_speculative_full()
                    && !parallel_background_lanes
                    && !state.background_in_flight.is_empty())
            {
                blocked.push(job);
                continue;
            }
            state.queued.remove(&job.id);
            selected = Some(Self::claim(state, job.id, job.action));
            break;
        }
        state.heap.extend(blocked);
        if selected.is_some() {
            return selected;
        }

        // Persistent folder warming is strictly below both replaceable lanes
        // and owns at most one worker. A speculative Full job may use one more,
        // leaving at least one heavy dispatcher ready for foreground work.
        if !state.background_in_flight.is_empty()
            || (!parallel_background_lanes && speculative_full_active)
        {
            return None;
        }
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
            let claimed = Self::claim(state, id, action);
            state.background_in_flight.insert(
                id,
                InFlight {
                    action,
                    token: claimed.2.clone(),
                },
            );
            return Some(claimed);
        }
        None
    }

    /// Block until a valid job is available (or shutdown).
    fn pop(&self) -> Option<(JobId, Action, Arc<CancelToken>)> {
        let mut state = self.state.lock().unwrap();
        loop {
            if state.closed {
                return None;
            }
            if let Some(job) = Self::try_claim_locked(&mut state, self.parallel_background_lanes) {
                return Some(job);
            }
            state = self.cond.wait(state).unwrap();
        }
    }

    #[cfg(test)]
    fn try_pop(&self) -> Option<(JobId, Action, Arc<CancelToken>)> {
        let mut state = self.state.lock().unwrap();
        (!state.closed)
            .then(|| Self::try_claim_locked(&mut state, self.parallel_background_lanes))
            .flatten()
    }

    #[cfg(test)]
    fn finish(&self, id: JobId, token: &Arc<CancelToken>) {
        self.finish_with(id, token, JobCompletion::Complete);
    }

    fn enqueue_current_event(
        &self,
        id: JobId,
        token: &Arc<CancelToken>,
        events: &Sender<Event>,
        event: Event,
    ) -> bool {
        let state = self.state.lock().unwrap();
        if !state
            .in_flight
            .get(&id)
            .is_some_and(|current| Arc::ptr_eq(&current.token, token) && !token.cancelled())
        {
            return false;
        }

        // Sending on the unbounded worker channel cannot call application
        // code. Keep the ownership check and enqueue under one lock so a
        // replan linearizes either before both operations or after both.
        let _ = events.send(event);
        true
    }

    #[cfg(test)]
    fn finish_with(&self, id: JobId, token: &Arc<CancelToken>, completion: JobCompletion) {
        self.finish_with_publication(id, token, completion, None);
    }

    fn finish_with_publication(
        &self,
        id: JobId,
        token: &Arc<CancelToken>,
        completion: JobCompletion,
        publication: Option<(&Sender<Event>, Event)>,
    ) -> bool {
        let mut state = self.state.lock().unwrap();
        let publishable = state
            .in_flight
            .get(&id)
            .is_some_and(|current| Arc::ptr_eq(&current.token, token) && !token.cancelled());
        if publishable && completion == JobCompletion::SuppressSpeculative {
            debug_assert!(
                state
                    .in_flight
                    .get(&id)
                    .is_some_and(|current| current.action.is_speculative_full())
            );
            state.speculative_failures.insert(id);
        }
        let published = if publishable {
            publication.is_some_and(|(events, event)| {
                // As in `enqueue_current_event`, enqueue before releasing
                // generation ownership. User notification happens only after
                // this method releases the queue-state lock.
                let _ = events.send(event);
                true
            })
        } else {
            false
        };
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
                    JobCompletion::SuppressSpeculative => {}
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
        published
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
    /// Folder index to filtered position, built once when a filter changes.
    positions: HashMap<usize, usize>,
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

    fn current_position(&self, current: usize) -> usize {
        if self.indices.is_empty() {
            current
        } else {
            self.positions.get(&current).copied().unwrap_or_default()
        }
    }

    fn replace_indices(&mut self, len: usize, indices: Vec<usize>) {
        self.indices.clear();
        self.indices.reserve(indices.len());
        self.positions.clear();
        self.positions.reserve(indices.len());
        for index in indices {
            if index < len && !self.positions.contains_key(&index) {
                let position = self.indices.len();
                self.positions.insert(index, position);
                self.indices.push(index);
            }
        }
        self.last_nav = None;
    }
}

struct Shared {
    entries: Arc<Vec<FolderEntry>>,
    cache: Arc<RamCache>,
    disk: Option<DiskCache>,
    events: Sender<Event>,
    notify: Arc<dyn Fn() + Send + Sync>,
    /// A fixed user limit owns the pool that contains CPU-heavy work, its
    /// source/cache reads, every nested Rayon operation, and cache JPEG
    /// encoding. Automatic mode has no owned pool: it preserves the established
    /// dispatcher/global-Rayon topology and separately tuned JPEG pool. Queue,
    /// UI, metadata, database/update, and disk-cache persistence/maintenance
    /// service threads remain separate.
    processing_pool: Option<rayon::ThreadPool>,
    heavy: JobQueue,
    light: JobQueue,
    persistence: PersistenceQueue,
    /// Full RGBA entries whose matching disk object was observed without
    /// installing JPEG bytes in RAM.
    persistence_known_present: Mutex<HashSet<JobId>>,
    jpeg_quality: u8,
    /// Display order and its last navigation generation.
    navigation: Mutex<NavigationOrder>,
}

/// Construction options for an image-processing [`Engine`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EngineOptions {
    jpeg_quality: u8,
    worker_threads: Option<NonZeroUsize>,
}

impl Default for EngineOptions {
    fn default() -> Self {
        Self {
            jpeg_quality: CACHE_JPEG_QUALITY,
            worker_threads: None,
        }
    }
}

impl EngineOptions {
    /// Selects the persistent-cache JPEG quality.
    ///
    /// Values outside the supported application range are clamped.
    #[must_use]
    pub fn with_jpeg_quality(mut self, jpeg_quality: u8) -> Self {
        self.jpeg_quality = jpeg_quality.clamp(MIN_CACHE_JPEG_QUALITY, MAX_CACHE_JPEG_QUALITY);
        self
    }

    /// Selects the number of CPU-heavy image-processing workers.
    ///
    /// This also brings cache JPEG encoding under the same strict limit. It
    /// does not count the UI/render thread or lightweight queue, metadata,
    /// database/update, and disk-cache persistence/maintenance service threads.
    /// Source and cache reads performed by image jobs run within the selected
    /// processing workers.
    #[must_use]
    pub fn with_worker_threads(mut self, worker_threads: NonZeroUsize) -> Self {
        self.worker_threads = Some(resolve_worker_threads(worker_threads.get()));
        self
    }
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

type EngineTask = Box<dyn FnOnce() + Send + 'static>;

fn spawn_engine_threads(
    engine: &mut Engine,
    mut spawn: impl FnMut(String, EngineTask) -> std::io::Result<std::thread::JoinHandle<()>>,
) -> std::io::Result<()> {
    let shared = engine.shared.clone();
    engine.persistence_worker = Some(spawn(
        "viewr-persistence".into(),
        Box::new(move || persistence_worker(&shared)),
    )?);

    for worker_index in 0..HEAVY_WORKERS {
        let shared = engine.shared.clone();
        engine.workers.push(spawn(
            format!("viewr-heavy-{worker_index}"),
            Box::new(move || worker(&shared, false)),
        )?);
    }
    for worker_index in 0..LIGHT_WORKERS {
        let shared = engine.shared.clone();
        engine.workers.push(spawn(
            format!("viewr-light-{worker_index}"),
            Box::new(move || worker(&shared, true)),
        )?);
    }
    Ok(())
}

fn navigation_pins(
    len: usize,
    current: usize,
    current_position: usize,
    sequence: &[usize],
) -> Vec<JobId> {
    if len == 0 {
        return Vec::new();
    }
    let current = current.min(len - 1);
    let indices: Vec<usize> = if sequence.is_empty() {
        (current.saturating_sub(1)..=(current + 1).min(len - 1)).collect()
    } else {
        let position = current_position.min(sequence.len() - 1);
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
    let mut pins = Vec::with_capacity(indices.len() * 3);
    for index in indices {
        pins.extend([
            (index, Tier::Thumb),
            (index, Tier::Browse),
            (index, Tier::Full),
        ]);
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
        Self::new_with_options(
            entries,
            start,
            cache,
            disk,
            EngineOptions::default(),
            notify,
        )
    }

    /// Spawns the worker pool with a selected persistent-cache JPEG quality.
    ///
    /// Values outside the supported application range are clamped. The
    /// selected quality becomes part of non-default disk-cache keys, so
    /// changing it cannot reuse bytes encoded at another quality.
    pub fn new_with_jpeg_quality(
        entries: Arc<Vec<FolderEntry>>,
        start: usize,
        cache: Arc<RamCache>,
        disk: Option<DiskCache>,
        jpeg_quality: u8,
        notify: Arc<dyn Fn() + Send + Sync>,
    ) -> (Self, Receiver<Event>) {
        Self::new_with_options(
            entries,
            start,
            cache,
            disk,
            EngineOptions::default().with_jpeg_quality(jpeg_quality),
            notify,
        )
    }

    /// Spawns the worker pool with explicit processing options.
    ///
    /// A count selected with [`EngineOptions::with_worker_threads`] caps
    /// concurrent CPU-heavy RAW decode, development, resize, cache decode, and
    /// cache encode work. Default automatic mode keeps cache encoding on its
    /// separately tuned pool. Lightweight service threads remain separate so a
    /// one-thread processing budget does not deadlock queue coordination or
    /// persistence I/O.
    ///
    /// # Panics
    ///
    /// Panics if the processing pool or an operating-system service thread
    /// cannot be spawned.
    pub fn new_with_options(
        entries: Arc<Vec<FolderEntry>>,
        start: usize,
        cache: Arc<RamCache>,
        disk: Option<DiskCache>,
        options: EngineOptions,
        notify: Arc<dyn Fn() + Send + Sync>,
    ) -> (Self, Receiver<Event>) {
        let (events, rx) = std::sync::mpsc::channel();
        let processing_pool = options
            .worker_threads
            .map(build_processing_pool)
            .transpose()
            .expect("failed to spawn image-processing worker pool");
        let parallel_background_lanes = options
            .worker_threads
            .is_none_or(|threads| threads.get() >= HEAVY_WORKERS);
        // Construct the owner before spawning. If any later spawn panics via
        // `expect`, `Engine::drop` closes both queues and joins every handle
        // already installed by `spawn_engine_threads`.
        let mut engine = Self {
            shared: Arc::new(Shared {
                entries,
                cache,
                disk,
                events,
                notify,
                processing_pool,
                heavy: JobQueue::new_with_parallel_background_lanes(parallel_background_lanes),
                light: JobQueue::new(),
                persistence: PersistenceQueue::new(),
                persistence_known_present: Mutex::new(HashSet::new()),
                jpeg_quality: options.jpeg_quality,
                navigation: Mutex::new(NavigationOrder::default()),
            }),
            workers: Vec::with_capacity(HEAVY_WORKERS + LIGHT_WORKERS),
            persistence_worker: None,
        };
        spawn_engine_threads(&mut engine, |name, task| {
            std::thread::Builder::new().name(name).spawn(task)
        })
        .expect("failed to spawn engine worker");
        let shared = &engine.shared;

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

        (engine, rx)
    }

    /// Recomputes and synchronizes the heavy plan for a navigation state.
    ///
    /// Call this after navigation or zoom changes and after image completion
    /// events. Obsolete jobs are cooperatively cancelled. Current and immediate
    /// visible neighbors are always requested and pinned at Full resolution;
    /// farther Full renders grow to the dedicated ring's byte budget. Zoom
    /// mode raises the current Full job's priority. The first call with a disk
    /// cache also installs the one-shot folder-wide Browse warm lane.
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
        // Keep navigation generation, cache admission, and queue replacement
        // in one serialized transaction. Engine is Sync, so concurrent API
        // callers must not interleave policy A with queue plan B. Taking the
        // snapshots inside this transaction also keeps a waiting navigate call
        // from extending the copy-on-write observation window.
        let mut navigation = self.shared.navigation.lock().unwrap();
        let navigation_changed = navigation.update_navigation(nav);
        let current_position = navigation.current_position(current);
        let (full_snapshot, browse_snapshot) = cache.prefetch_snapshots();
        let prefetch_budgets = NavigationPrefetchBudgets::new(
            FullPrefetchBudget::from_observations(
                full_snapshot.budget_bytes,
                full_snapshot.fallback_bytes,
                full_snapshot.per_index_bytes,
            ),
            BrowsePrefetchBudget::from_observations(
                browse_snapshot.budget_bytes,
                browse_snapshot.fallback_bytes,
                browse_snapshot.per_index_bytes,
            ),
        );

        let disk = &self.shared.disk;
        let pins = navigation_pins(len, current, current_position, &navigation.indices);
        let targets = build_plan_targets_with_normalized_prefetch(
            len,
            current,
            nav.direction,
            nav.zoomed,
            (&navigation.indices, current_position),
            &prefetch_budgets,
            false,
        );
        // A worker can now record another exact size without cloning the
        // planner's copy-on-write observation map.
        drop(prefetch_budgets);
        // Filtered navigation pins visible neighbors rather than unrelated raw
        // indices. Installing the desired Full keys under that same cache lock
        // also evicts stale speculative buffers and rejects late completions.
        cache.set_navigation_policy(
            pins,
            targets
                .iter()
                .filter(|target| target.tier == Tier::Full)
                .map(|target| (target.index, Tier::Full)),
        );
        let mut plan: Vec<(JobId, u8, u32, Action)> = Vec::with_capacity(targets.len());
        for target in targets {
            let id = (target.index, target.tier);
            match target.kind {
                PlanKind::Display => {
                    if cache.has_rgba(id) {
                        if target.tier == Tier::Full
                            && !cache.has_jpeg(id)
                            && !self
                                .shared
                                .persistence_known_present
                                .lock()
                                .unwrap()
                                .contains(&id)
                        {
                            plan.push((id, 3, target.effective_distance, Action::PersistResident));
                        }
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
                PlanKind::Prefetch => {
                    if !cache.has_rgba(id) {
                        plan.push((
                            id,
                            target.class,
                            target.effective_distance,
                            Action::PrefetchFull,
                        ));
                    }
                }
                PlanKind::Warm => {
                    // Folder warming lives in the persistent background lane,
                    // so navigation never contributes this O(N) target set.
                }
            }
        }

        self.shared.heavy.set_plan(plan, navigation_changed);
        if disk.is_some() {
            self.shared
                .heavy
                .initialize_background(|| background_warm_jobs(len, current));
        }
        drop(navigation);
    }

    /// Sets the display order followed by navigation and pinning.
    ///
    /// An empty vector restores identity order. This call removes out-of-range
    /// entries and later duplicates once; later navigation replans reuse the
    /// normalized order. Call [`navigate`](Self::navigate) afterwards to apply
    /// the new order.
    pub fn set_sequence(&self, sequence: Vec<usize>) {
        let mut navigation = self.shared.navigation.lock().unwrap();
        navigation.replace_indices(self.shared.entries.len(), sequence);
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

/// Production metadata queue fixture exposed only to the Criterion harness.
#[cfg(feature = "benchmarks")]
#[doc(hidden)]
pub struct BenchmarkMetadataQueue {
    queue: JobQueue,
}

#[cfg(feature = "benchmarks")]
impl BenchmarkMetadataQueue {
    /// Builds the production metadata queue without starting decoder threads.
    pub fn new(len: usize) -> Self {
        let queue = JobQueue::new();
        queue.extend((0..len).map(|index| {
            (
                (index, Tier::Thumb),
                5,
                index.min(u32::MAX as usize) as u32,
                Action::Metadata,
            )
        }));
        Self { queue }
    }

    /// Returns the number of jobs retained by the production queue.
    pub fn resident_jobs(&self) -> usize {
        self.queue.state.lock().unwrap().heap.len()
    }
}

/// Production priority-queue synchronization isolated from decoder threads.
#[cfg(feature = "benchmarks")]
#[doc(hidden)]
pub struct BenchmarkNavigationQueue {
    queue: JobQueue,
    len: usize,
    budgets: NavigationPrefetchBudgets,
    sequence: Vec<usize>,
    positions: HashMap<usize, usize>,
}

#[cfg(feature = "benchmarks")]
impl BenchmarkNavigationQueue {
    /// Creates an empty production queue for a synthetic folder.
    pub fn new(len: usize) -> Self {
        Self {
            queue: JobQueue::new(),
            len,
            budgets: NavigationPrefetchBudgets::new(
                FullPrefetchBudget::new(1024 * 1024 * 1024, 128 * 1024 * 1024, HashMap::new()),
                BrowsePrefetchBudget::new(1024 * 1024 * 1024, 32 * 1024 * 1024, HashMap::new()),
            ),
            sequence: Vec::new(),
            positions: HashMap::new(),
        }
    }

    /// Normalizes and installs a filtered production navigation sequence once.
    pub fn set_sequence(&mut self, sequence: Vec<usize>) {
        self.sequence.clear();
        self.sequence.reserve(sequence.len());
        self.positions.clear();
        self.positions.reserve(sequence.len());
        for index in sequence {
            if index < self.len && !self.positions.contains_key(&index) {
                self.positions.insert(index, self.sequence.len());
                self.sequence.push(index);
            }
        }
    }

    /// Replans one production adaptive fit-mode navigation and returns the
    /// queued target count.
    pub fn navigate(&self, current: usize) -> usize {
        let current_position = if self.sequence.is_empty() {
            current
        } else {
            self.positions.get(&current).copied().unwrap_or_default()
        };
        let plan = build_plan_targets_with_normalized_prefetch(
            self.len,
            current,
            1,
            false,
            (&self.sequence, current_position),
            &self.budgets,
            false,
        )
        .into_iter()
        .filter_map(|target| {
            let action = match target.kind {
                PlanKind::Display => Action::Develop(match target.tier {
                    Tier::Full => Quality::Full,
                    Tier::Thumb | Tier::Browse => Quality::Browse,
                }),
                PlanKind::Prefetch => Action::PrefetchFull,
                PlanKind::Warm => return None,
            };
            Some((
                (target.index, target.tier),
                target.class,
                target.effective_distance,
                action,
            ))
        })
        .collect();
        self.queue.set_plan(plan, true);
        self.queue.state.lock().unwrap().heap.len()
    }

    /// Replans the prior fixed ±1 Full policy as a benchmark reference.
    pub fn navigate_fixed_reference(&self, current: usize) -> usize {
        let plan = build_plan_targets(self.len, current, 1, false, &[], false)
            .into_iter()
            .filter(|target| target.kind == PlanKind::Display)
            .map(|target| {
                (
                    (target.index, target.tier),
                    target.class,
                    target.effective_distance,
                    Action::Develop(match target.tier {
                        Tier::Full => Quality::Full,
                        Tier::Thumb | Tier::Browse => Quality::Browse,
                    }),
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
            || {
                match (action, &shared.processing_pool) {
                    // Container metadata scanning is a lightweight I/O service
                    // lane. Keeping it outside a one-thread processing pool
                    // prevents a folder-wide scan from delaying the current
                    // image's first decode. Automatic mode also stays on the
                    // established direct dispatcher path.
                    (Action::Metadata, _) | (_, None) => run_job(shared, queue, id, action, &token),
                    (_, Some(pool)) => pool.install(|| run_job(shared, queue, id, action, &token)),
                }
            },
            &shared.events,
            || notify_safely(shared.notify.as_ref()),
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
/// ownership validation, failure enqueue, and cleanup share one critical
/// section; application notification happens only after that lock is released.
fn execute_claimed_job(
    queue: &JobQueue,
    id: JobId,
    action: Action,
    token: &Arc<CancelToken>,
    run: impl FnOnce() -> JobCompletion,
    events: &Sender<Event>,
    notify: impl FnOnce(),
) -> Option<usize> {
    let (completion, panic_payload) =
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)) {
            Ok(completion) => (completion, None),
            Err(payload) => (
                if action.is_speculative_full() {
                    JobCompletion::SuppressSpeculative
                } else {
                    JobCompletion::Complete
                },
                Some(payload),
            ),
        };
    let deferred_bytes = match completion {
        JobCompletion::DeferBackground { required_bytes } => Some(required_bytes),
        JobCompletion::Complete
        | JobCompletion::RetryBackground
        | JobCompletion::SuppressSpeculative => None,
    };
    let panic_event = panic_payload
        .as_deref()
        .and_then(|payload| worker_panic_event(id, action, payload));
    if queue.finish_with_publication(
        id,
        token,
        completion,
        panic_event.map(|event| (events, event)),
    ) {
        notify();
    }
    deferred_bytes
}

fn worker_panic_event(
    (index, tier): JobId,
    action: Action,
    payload: &(dyn std::any::Any + Send),
) -> Option<Event> {
    let detail = if let Some(message) = payload.downcast_ref::<&str>() {
        *message
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.as_str()
    } else {
        "non-string panic payload"
    };
    let error = format!("worker panicked: {detail}");
    match action {
        Action::Metadata => Some(Event::MetadataFailed { index, error }),
        Action::Thumb | Action::Develop(_) | Action::PrefetchFull | Action::Rehydrate => {
            Some(Event::ImageFailed { index, tier, error })
        }
        Action::PersistResident => None,
        // Folder-wide warming is intentionally invisible to the event/UI
        // channel. The panic hook still records the contained panic.
        Action::WarmDevelop(_) => None,
    }
}

#[cfg(test)]
fn publish(shared: &Shared, event: Event) {
    let _ = shared.events.send(event);
    notify_safely(shared.notify.as_ref());
}

fn publish_claimed(
    shared: &Shared,
    queue: &JobQueue,
    id: JobId,
    token: &Arc<CancelToken>,
    event: Event,
) {
    if queue.enqueue_current_event(id, token, &shared.events, event) {
        notify_safely(shared.notify.as_ref());
    }
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
    let quality = shared.jpeg_quality;
    while let Some(request) = shared.persistence.pop() {
        // Fixed limits encode on one permitted processing worker, leaving the
        // rest available for newly interactive work. Automatic mode retains
        // the separately tuned JPEG pool used by previous releases.
        std::thread::yield_now();
        let encoded = encode_cache_jpeg(shared.processing_pool.as_ref(), &request.pixels, quality);
        let mut persistence_error = None;
        let encode_error = encoded.as_ref().err().cloned();
        if let Ok(bytes) = &encoded
            && let Some(disk) = &shared.disk
        {
            let key = DiskCache::key_with_jpeg_quality(
                &shared.entries[request.id.0],
                request.id.1,
                quality,
            );
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

fn run_job(
    shared: &Shared,
    queue: &JobQueue,
    id: JobId,
    action: Action,
    token: &Arc<CancelToken>,
) -> JobCompletion {
    let (index, tier) = id;
    let emit = |event| publish_claimed(shared, queue, id, token, event);
    if matches!(
        action,
        Action::Develop(_) | Action::PrefetchFull | Action::Rehydrate
    ) && shared.cache.has_rgba(id)
    {
        // A cancelled generation can finish between the replacement plan's
        // cache probe and this claim (A→B→A). Reuse its admitted pixels rather
        // than decoding the same file again, and publish under the live token.
        emit(Event::ImageReady { index, tier });
        if tier == Tier::Full && !action.is_speculative_full() {
            run_persist_resident(shared, index, tier, token);
        }
        return JobCompletion::Complete;
    }
    match action {
        Action::Metadata => run_metadata(shared, index, token, &emit),
        Action::Thumb => run_thumb(shared, index, &emit),
        Action::Rehydrate => {
            run_rehydrate(shared, index, tier, token, DevelopMode::Display, &emit);
        }
        Action::PrefetchFull => {
            debug_assert_eq!(tier, Tier::Full);
            run_rehydrate(shared, index, tier, token, DevelopMode::Prefetch, &emit);
            if !token.cancelled() && !shared.cache.has_rgba(id) {
                return JobCompletion::SuppressSpeculative;
            }
        }
        Action::PersistResident => run_persist_resident(shared, index, tier, token),
        Action::Develop(quality) => {
            let _ = run_develop(
                shared,
                index,
                tier,
                quality,
                token,
                DevelopMode::Display,
                &emit,
            );
        }
        Action::WarmDevelop(quality) => {
            return run_warm_develop(shared, index, tier, quality, token, &emit);
        }
    }
    JobCompletion::Complete
}

fn run_metadata(shared: &Shared, index: usize, token: &CancelToken, emit: &dyn Fn(Event)) {
    if token.cancelled() {
        return;
    }
    match decode::metadata(&shared.entries[index].path) {
        Ok(meta) if !token.cancelled() => emit(Event::MetadataReady {
            index,
            meta: Box::new(meta),
        }),
        Ok(_) => {}
        Err(error) if !token.cancelled() => emit(Event::MetadataFailed {
            index,
            error: error.to_string(),
        }),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DevelopMode {
    /// Install decoded pixels, publish readiness, and persist a cache miss.
    Display,
    /// Install only into the adaptive Full RGBA ring.
    Prefetch,
    /// Populate encoded RAM/disk cache without installing decoded pixels.
    Warm,
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
    emit: &dyn Fn(Event),
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
    if disk.has(&DiskCache::key_with_jpeg_quality(
        &shared.entries[index],
        tier,
        shared.jpeg_quality,
    )) {
        return JobCompletion::Complete;
    }
    if shared
        .persistence
        .mark_warm_completion_if_present((index, tier))
    {
        return JobCompletion::Complete;
    }
    warm_job_completion(run_develop(
        shared,
        index,
        tier,
        quality,
        token,
        DevelopMode::Warm,
        emit,
    ))
}

fn run_thumb(shared: &Shared, index: usize, emit: &dyn Fn(Event)) {
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
        emit,
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
    mode: DevelopMode,
    emit: &dyn Fn(Event),
) -> DevelopCompletion {
    let path = &shared.entries[index].path;
    let fail = |e: String| {
        if mode != DevelopMode::Warm && !token.cancelled() {
            emit(Event::ImageFailed {
                index,
                tier,
                error: e,
            });
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
    if token.cancelled() {
        return DevelopCompletion::Cancelled;
    }
    let buf = Arc::new(apply_orient(buf, meta.orient));

    // Cancellation is checked after every non-interruptible stage. Full-ring
    // admission and navigation-policy replacement share the cache mutex, so a
    // late speculative completion cannot repopulate a stale working set.
    if token.cancelled() {
        return DevelopCompletion::Cancelled;
    }
    if mode != DevelopMode::Warm {
        if !shared
            .cache
            .insert_rgba_if_desired((index, tier), buf.clone())
        {
            return DevelopCompletion::Finished;
        }
        emit(Event::ImageReady { index, tier });
    }
    if mode == DevelopMode::Prefetch {
        return DevelopCompletion::Finished;
    }

    // Ring-2 + ring-3 insurance runs on a bounded background lane. ImageReady
    // can trigger a replan that cancels this token immediately; persistence is
    // deliberately independent of that token once completed pixels enqueue.
    if mode == DevelopMode::Warm || !shared.cache.has_jpeg((index, tier)) {
        let retained_bytes = buf.byte_len();
        let enqueue = shared.persistence.enqueue(PersistenceRequest {
            id: (index, tier),
            pixels: buf,
            insert_ram: mode == DevelopMode::Display,
            warm_completion: mode == DevelopMode::Warm,
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

fn run_rehydrate(
    shared: &Shared,
    index: usize,
    tier: Tier,
    token: &CancelToken,
    mode: DevelopMode,
    emit: &dyn Fn(Event),
) {
    // Ring 2 first, then ring 3 (disk). Disk bytes enter RAM only after JPEG
    // validation; a corrupt rebuildable object is evicted and falls through
    // to RAW development instead of poisoning every later request.
    if token.cancelled() {
        return;
    }
    let id = (index, tier);
    if let Some(bytes) = shared.cache.get_jpeg(id) {
        if let Ok(buf) = decode_jpeg(&bytes) {
            install_rehydrated(shared, index, tier, buf, token, emit);
            return;
        }
        shared.cache.remove_jpeg(id);
    }

    if token.cancelled() {
        return;
    }
    if let Some(disk) = &shared.disk {
        let key =
            DiskCache::key_with_jpeg_quality(&shared.entries[index], tier, shared.jpeg_quality);
        if let Some(bytes) = disk.get(&key) {
            if token.cancelled() {
                return;
            }
            if let Ok(buf) = decode_jpeg(&bytes) {
                if install_rehydrated(shared, index, tier, buf, token, emit)
                    && mode == DevelopMode::Display
                {
                    shared.cache.insert_jpeg(id, Arc::new(bytes));
                }
                return;
            }
            if let Err(error) = disk.remove(&key) {
                eprintln!("failed to remove corrupt disk cache object: {error}");
            }
            shared.persistence_known_present.lock().unwrap().remove(&id);
        } else {
            // A previous resident promotion may have observed this key. If
            // disk cleanup removed it later, let the next successful RAW
            // fallback enqueue persistence again.
            shared.persistence_known_present.lock().unwrap().remove(&id);
        }
    }

    develop_cache_miss(shared, index, tier, token, mode, emit);
}

fn run_persist_resident(shared: &Shared, index: usize, tier: Tier, token: &CancelToken) {
    let id = (index, tier);
    if token.cancelled()
        || shared.cache.has_jpeg(id)
        || shared
            .persistence_known_present
            .lock()
            .unwrap()
            .contains(&id)
    {
        return;
    }
    if let Some(disk) = &shared.disk
        && disk.has(&DiskCache::key_with_jpeg_quality(
            &shared.entries[index],
            tier,
            shared.jpeg_quality,
        ))
    {
        shared.persistence_known_present.lock().unwrap().insert(id);
        return;
    }
    let Some(pixels) = shared.cache.get_rgba(id) else {
        return;
    };
    if token.cancelled() {
        return;
    }
    let retained_bytes = pixels.byte_len();
    if shared.persistence.enqueue(PersistenceRequest {
        id,
        pixels,
        insert_ram: true,
        warm_completion: false,
    }) == PersistenceEnqueue::Oversized
    {
        eprintln!(
            "disk cache persistence skipped: {retained_bytes} byte buffer exceeds the {} byte pending budget",
            shared.persistence.pending_budget_bytes
        );
    }
}

fn install_rehydrated(
    shared: &Shared,
    index: usize,
    tier: Tier,
    buf: PixelBuf,
    token: &CancelToken,
    emit: &dyn Fn(Event),
) -> bool {
    if token.cancelled()
        || !shared
            .cache
            .insert_rgba_if_desired((index, tier), Arc::new(buf))
    {
        return false;
    }
    emit(Event::ImageReady { index, tier });
    true
}

fn develop_cache_miss(
    shared: &Shared,
    index: usize,
    tier: Tier,
    token: &CancelToken,
    mode: DevelopMode,
    emit: &dyn Fn(Event),
) {
    let quality = match tier {
        Tier::Full => Quality::Full,
        _ => Quality::Browse,
    };
    run_develop(shared, index, tier, quality, token, mode, emit);
}

/// Encodes a tightly packed RGBA8 buffer as a JPEG.
///
/// `quality` is passed directly to jpeg-rusturbo. Chroma is always encoded at
/// full 4:4:4 resolution and the output discards alpha. Encoding uses a
/// dedicated bounded Rayon pool so cache persistence cannot occupy the
/// foreground RAW-development pool.
///
/// # Errors
///
/// Returns a string error if quality is outside 1–100, either dimension is zero
/// or exceeds the JPEG format limit, the RGBA storage is inconsistent with the
/// dimensions, or JPEG encoding fails.
pub fn encode_jpeg(buf: &PixelBuf, quality: u8) -> Result<Vec<u8>, String> {
    encode_jpeg_with_pool(jpeg_pool()?, buf, quality)
}

fn encode_jpeg_with_pool(
    pool: &rayon::ThreadPool,
    buf: &PixelBuf,
    quality: u8,
) -> Result<Vec<u8>, String> {
    validate_jpeg_input(buf, quality)?;
    pool.install(|| encode_jpeg_on_current_pool(buf, quality, 0))
}

fn encode_cache_jpeg(
    processing_pool: Option<&rayon::ThreadPool>,
    buf: &PixelBuf,
    quality: u8,
) -> Result<Vec<u8>, String> {
    validate_jpeg_input(buf, quality)?;
    with_cache_encoding_context(processing_pool, |encoder_threads| {
        encode_jpeg_on_current_pool(buf, quality, encoder_threads)
    })?
}

fn with_cache_encoding_context<R: Send>(
    processing_pool: Option<&rayon::ThreadPool>,
    encode: impl FnOnce(u32) -> R + Send,
) -> Result<R, String> {
    if let Some(processing_pool) = processing_pool {
        Ok(processing_pool.install(|| encode(1)))
    } else {
        Ok(jpeg_pool()?.install(|| encode(0)))
    }
}

fn jpeg_pool() -> Result<&'static rayon::ThreadPool, String> {
    static POOL: OnceLock<rayon::ThreadPool> = OnceLock::new();
    if let Some(pool) = POOL.get() {
        return Ok(pool);
    }
    let available = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1);
    let workers = available.clamp(1, MAX_JPEG_WORKERS);
    let candidate = rayon::ThreadPoolBuilder::new()
        .num_threads(workers)
        .thread_name(|index| format!("viewr-jpeg-{index}"))
        .build()
        .map_err(|error| error.to_string())?;
    let _ = POOL.set(candidate);
    Ok(POOL
        .get()
        .expect("the JPEG pool was installed by this or a racing caller"))
}

fn encode_jpeg_on_current_pool(
    buf: &PixelBuf,
    quality: u8,
    encoder_threads: u32,
) -> Result<Vec<u8>, String> {
    let mut output = Vec::new();
    let mut encoder = jpeg_rusturbo::JpegEncoder::new_with_quality(&mut output, quality);
    encoder.set_subsampling(jpeg_rusturbo::ChromaSubsampling::Yuv444);
    encoder.set_threads(encoder_threads);
    // One restart interval per 4:4:4 MCU row (8 pixel rows) resets the DC
    // predictors at every row boundary, which lets decode_jpeg split the scan
    // across workers. The output stays a standard baseline JPEG; decoders
    // without that split read it unchanged. Width is validated to 65535 or
    // less, so the per-row MCU count always fits the u16 DRI field.
    encoder.set_restart_interval(buf.width.div_ceil(8) as u16);
    encoder
        .encode_rgba(&buf.rgba, buf.width, buf.height)
        .map_err(|error| error.to_string())?;
    drop(encoder);
    Ok(output)
}

fn validate_jpeg_input(buf: &PixelBuf, quality: u8) -> Result<(usize, usize, usize), String> {
    if !(1..=100).contains(&quality) {
        return Err(format!("JPEG quality {quality} is outside 1..=100"));
    }
    let width = usize::from(
        u16::try_from(buf.width).map_err(|_| format!("JPEG width {} exceeds 65535", buf.width))?,
    );
    let height = usize::from(
        u16::try_from(buf.height)
            .map_err(|_| format!("JPEG height {} exceeds 65535", buf.height))?,
    );
    if width == 0 || height == 0 {
        return Err("JPEG dimensions must be non-zero".into());
    }
    let pitch = width
        .checked_mul(4)
        .ok_or_else(|| "JPEG row byte count overflowed".to_string())?;
    let expected_len = pitch
        .checked_mul(height)
        .ok_or_else(|| "JPEG buffer byte count overflowed".to_string())?;
    if buf.rgba.len() != expected_len {
        return Err(format!(
            "RGBA storage has {} bytes; expected {expected_len} for {}x{}",
            buf.rgba.len(),
            buf.width,
            buf.height
        ));
    }
    Ok((width, height, pitch))
}

/// Decodes JPEG bytes into a tightly packed RGBA8 buffer.
///
/// Viewr's own cache objects carry row-aligned restart markers, so large
/// streams normally decode on multiple workers; every other stream — and any
/// stream the splitter refuses — uses the whole-buffer serial decoder.
///
/// # Errors
///
/// Returns a human-readable string for malformed or unsupported JPEG data, or
/// when the decoder does not report dimensions.
pub fn decode_jpeg(bytes: &[u8]) -> Result<PixelBuf, String> {
    if let Some(buf) = crate::jpeg_restart::try_decode(bytes) {
        return Ok(buf);
    }
    decode_jpeg_serial(bytes)
}

fn decode_jpeg_serial(bytes: &[u8]) -> Result<PixelBuf, String> {
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

#[cfg(feature = "benchmarks")]
#[doc(hidden)]
/// Runs the whole-buffer serial JPEG decode as a benchmark reference.
pub fn benchmark_decode_jpeg_serial(bytes: &[u8]) -> Result<PixelBuf, String> {
    decode_jpeg_serial(bytes)
}

#[cfg(feature = "benchmarks")]
#[doc(hidden)]
/// Encodes without restart markers as a size and speed benchmark reference.
pub fn benchmark_encode_jpeg_plain(buf: &PixelBuf, quality: u8) -> Result<Vec<u8>, String> {
    validate_jpeg_input(buf, quality)?;
    jpeg_pool()?.install(|| {
        let mut output = Vec::new();
        let mut encoder = jpeg_rusturbo::JpegEncoder::new_with_quality(&mut output, quality);
        encoder.set_subsampling(jpeg_rusturbo::ChromaSubsampling::Yuv444);
        encoder.set_threads(0);
        encoder
            .encode_rgba(&buf.rgba, buf.width, buf.height)
            .map_err(|error| error.to_string())?;
        drop(encoder);
        Ok(output)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rayon::prelude::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    fn test_processing_pool() -> rayon::ThreadPool {
        build_processing_pool(NonZeroUsize::new(1).unwrap()).unwrap()
    }

    fn test_cache(thumb_bytes: u64, browse_bytes: u64, jpeg_bytes: u64) -> RamCache {
        RamCache::new(RamCacheBudgets::new(
            thumb_bytes,
            browse_bytes,
            browse_bytes,
            jpeg_bytes,
        ))
    }

    #[test]
    fn worker_thread_limits_resolve_against_available_parallelism() {
        assert_eq!(resolve_worker_threads_for_available(0, 8).get(), 8);
        assert_eq!(resolve_worker_threads_for_available(1, 8).get(), 1);
        assert_eq!(resolve_worker_threads_for_available(4, 8).get(), 4);
        assert_eq!(resolve_worker_threads_for_available(64, 8).get(), 8);
        assert_eq!(resolve_worker_threads_for_available(0, 0).get(), 1);
        assert_eq!(resolve_worker_threads_for_available(64, 0).get(), 1);
    }

    #[test]
    fn automatic_preserves_dispatcher_topology_and_fixed_builds_a_processing_pool() {
        let automatic = EngineOptions::default();
        let fixed_host_limit = automatic.with_worker_threads(available_worker_threads());
        assert_eq!(automatic.worker_threads, None);
        assert_eq!(
            fixed_host_limit.worker_threads,
            Some(available_worker_threads())
        );
        let (automatic_engine, _events) = Engine::new_with_options(
            Arc::new(Vec::new()),
            0,
            Arc::new(test_cache(0, 0, 0)),
            None,
            automatic,
            Arc::new(|| {}),
        );
        assert!(
            automatic_engine.shared.processing_pool.is_none(),
            "automatic mode must preserve the established dispatcher/global-Rayon topology"
        );

        let (engine, _events) = Engine::new_with_options(
            Arc::new(Vec::new()),
            0,
            Arc::new(test_cache(0, 0, 0)),
            None,
            EngineOptions::default().with_worker_threads(NonZeroUsize::new(1).unwrap()),
            Arc::new(|| {}),
        );

        let processing_pool = engine
            .shared
            .processing_pool
            .as_ref()
            .expect("a fixed limit owns a processing pool");
        assert_eq!(processing_pool.current_num_threads(), 1);
        assert_eq!(
            processing_pool
                .install(|| std::thread::current().name().map(str::to_owned))
                .as_deref(),
            Some("viewr-processing-0")
        );
    }

    #[test]
    fn cache_encoding_strategy_distinguishes_automatic_from_a_fixed_limit() {
        let processing = build_processing_pool(NonZeroUsize::new(1).unwrap()).unwrap();
        let strict = with_cache_encoding_context(Some(&processing), |threads| {
            (threads, std::thread::current().name().map(str::to_owned))
        })
        .unwrap();
        assert_eq!(strict.0, 1);
        assert_eq!(strict.1.as_deref(), Some("viewr-processing-0"));

        let automatic = with_cache_encoding_context(None, |threads| {
            (threads, std::thread::current().name().map(str::to_owned))
        })
        .unwrap();
        assert_eq!(automatic.0, 0);
        assert!(
            automatic
                .1
                .as_deref()
                .is_some_and(|name| name.starts_with("viewr-jpeg-"))
        );
    }

    #[test]
    fn processing_pool_caps_concurrent_dispatchers_and_nested_rayon_work() {
        let pool = Arc::new(build_processing_pool(NonZeroUsize::new(2).unwrap()).unwrap());
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let release = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let (started, started_rx) = std::sync::mpsc::channel();
        let callers: Vec<_> = (0..6)
            .map(|_| {
                let pool = pool.clone();
                let active = active.clone();
                let peak = peak.clone();
                let release = release.clone();
                let started = started.clone();
                std::thread::spawn(move || {
                    pool.install(|| {
                        rayon::join(
                            || {
                                let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                                peak.fetch_max(now, Ordering::SeqCst);
                                started.send(()).unwrap();
                                let (lock, wake) = &*release;
                                let mut released = lock.lock().unwrap();
                                while !*released {
                                    released = wake.wait(released).unwrap();
                                }
                                active.fetch_sub(1, Ordering::SeqCst);
                            },
                            || {},
                        );
                    });
                })
            })
            .collect();
        drop(started);

        started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("first processing worker did not start");
        started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("second processing worker did not start");
        assert!(
            started_rx.try_recv().is_err(),
            "more work started than the two-thread processing limit"
        );

        let (lock, wake) = &*release;
        *lock.lock().unwrap() = true;
        wake.notify_all();
        for caller in callers {
            caller.join().unwrap();
        }
        assert_eq!(peak.load(Ordering::SeqCst), 2);
        assert_eq!(active.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn nested_rayon_work_stays_on_the_configured_processing_pool() {
        let pool = build_processing_pool(NonZeroUsize::new(2).unwrap()).unwrap();
        let parallel_iterator_names: HashSet<String> = pool.install(|| {
            (0..4096)
                .into_par_iter()
                .map(|_| {
                    std::thread::current()
                        .name()
                        .expect("processing worker has a name")
                        .to_owned()
                })
                .collect()
        });

        assert!(!parallel_iterator_names.is_empty());
        assert!(parallel_iterator_names.len() <= 2);
        assert!(
            parallel_iterator_names
                .iter()
                .all(|name| name.starts_with("viewr-processing-"))
        );

        // rawler's JPEG-XL DNG path uses JxlThreadPool::rayon_global().
        // Its "global" methods are ambient Rayon operations, so when the
        // decoder runs inside `processing_pool.install` they must inherit this
        // pool rather than escape to Rayon's process-global registry.
        let jxl_names = Arc::new(Mutex::new(HashSet::new()));
        pool.install(|| {
            let jxl_pool = jxl_threadpool::JxlThreadPool::rayon_global();

            let names = jxl_names.clone();
            jxl_pool.for_each_vec((0..4096).collect(), move |_| {
                names.lock().unwrap().insert(
                    std::thread::current()
                        .name()
                        .expect("processing worker has a name")
                        .to_owned(),
                );
            });

            let names = jxl_names.clone();
            jxl_pool.scope(|scope| {
                for _ in 0..64 {
                    let names = names.clone();
                    scope.spawn(move |_| {
                        names.lock().unwrap().insert(
                            std::thread::current()
                                .name()
                                .expect("processing worker has a name")
                                .to_owned(),
                        );
                    });
                }
            });

            let (completed, completed_rx) = std::sync::mpsc::channel();
            for _ in 0..64 {
                let names = jxl_names.clone();
                let completed = completed.clone();
                jxl_pool.spawn(move || {
                    let name = std::thread::current()
                        .name()
                        .unwrap_or("<unnamed>")
                        .to_owned();
                    if let Ok(mut names) = names.lock() {
                        names.insert(name);
                    }
                    let _ = completed.send(());
                });
            }
            drop(completed);
            for _ in 0..64 {
                completed_rx
                    .recv_timeout(Duration::from_secs(2))
                    .expect("ambient JPEG-XL task did not complete");
            }
        });

        let jxl_names = jxl_names.lock().unwrap();
        assert!(!jxl_names.is_empty());
        assert!(jxl_names.len() <= 2);
        assert!(
            jxl_names
                .iter()
                .all(|name| name.starts_with("viewr-processing-"))
        );
    }

    #[test]
    fn jpeg_results_are_identical_across_processing_thread_limits() {
        let pixels = textured_buf(1023, 769);
        let one = build_processing_pool(NonZeroUsize::new(1).unwrap()).unwrap();
        let two = build_processing_pool(NonZeroUsize::new(2).unwrap()).unwrap();

        let encoded_one = encode_jpeg_with_pool(&one, &pixels, 97).unwrap();
        let encoded_two = encode_jpeg_with_pool(&two, &pixels, 97).unwrap();
        let encoded_strict = encode_cache_jpeg(Some(&two), &pixels, 97).unwrap();
        assert_eq!(encoded_one, encoded_two);
        assert_eq!(encoded_one, encoded_strict);

        let serial = decode_jpeg_serial(&encoded_one).unwrap();
        assert!(
            one.install(|| crate::jpeg_restart::try_decode(&encoded_one))
                .is_none(),
            "a one-thread limit must use the serial cache decode fallback"
        );
        let parallel_two = two
            .install(|| crate::jpeg_restart::try_decode(&encoded_two))
            .unwrap_or_else(|| {
                panic!(
                    "production-size cache JPEG uses parallel restart decoding ({} bytes)",
                    encoded_two.len()
                )
            });
        let decoded_one = one.install(|| decode_jpeg(&encoded_one)).unwrap();
        let decoded_two = two.install(|| decode_jpeg(&encoded_two)).unwrap();
        for decoded in [&parallel_two, &decoded_one, &decoded_two] {
            assert_eq!(
                (decoded.width, decoded.height),
                (serial.width, serial.height)
            );
            assert_eq!(decoded.rgba, serial.rgba);
        }
    }

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
    fn partial_engine_construction_joins_started_threads_during_unwind() {
        let spawn_count = 1 + HEAVY_WORKERS + LIGHT_WORKERS;
        for fixed_processing_limit in [false, true] {
            for failure_at in 1..=spawn_count {
                let active_threads = Arc::new(AtomicUsize::new(0));
                let active_for_construction = active_threads.clone();
                let (weak_shared, weak_receiver) = std::sync::mpsc::channel();
                let construction =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                        let shared = persistence_shared_with_options(
                            Vec::new(),
                            Arc::new(test_cache(0, 0, 0)),
                            None,
                            0,
                            CACHE_JPEG_QUALITY,
                            fixed_processing_limit,
                        );
                        weak_shared.send(Arc::downgrade(&shared)).unwrap();
                        let mut engine = Engine {
                            shared,
                            workers: Vec::with_capacity(HEAVY_WORKERS + LIGHT_WORKERS),
                            persistence_worker: None,
                        };
                        let mut attempts = 0;

                        spawn_engine_threads(&mut engine, |name, task| {
                            attempts += 1;
                            if attempts == failure_at {
                                return Err(std::io::Error::other("injected worker spawn failure"));
                            }
                            let active_threads = active_for_construction.clone();
                            let (started, started_wait) = std::sync::mpsc::channel();
                            let handle =
                                std::thread::Builder::new().name(name).spawn(move || {
                                    active_threads.fetch_add(1, Ordering::SeqCst);
                                    started.send(()).unwrap();
                                    task();
                                    active_threads.fetch_sub(1, Ordering::SeqCst);
                                })?;
                            started_wait.recv().unwrap();
                            Ok(handle)
                        })
                        .expect("injected worker spawn failure must unwind construction");
                    }));

                assert!(
                    construction.is_err(),
                    "fixed={fixed_processing_limit} spawn {failure_at} must fail"
                );
                assert_eq!(
                    active_threads.load(Ordering::SeqCst),
                    0,
                    "fixed={fixed_processing_limit} spawn {failure_at} left a started worker running"
                );
                assert!(
                    weak_receiver.recv().unwrap().upgrade().is_none(),
                    "fixed={fixed_processing_limit} spawn {failure_at} left a worker retaining the engine"
                );
            }
        }
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
        let (events, receiver) = std::sync::mpsc::channel();
        let notifications = AtomicUsize::new(0);
        let deferred = execute_claimed_job(
            &queue,
            failed_id,
            failed_action,
            &failed_token,
            || panic!("deterministic decoder panic"),
            &events,
            || {
                notifications.fetch_add(1, Ordering::Relaxed);
            },
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
        assert_eq!(notifications.load(Ordering::Relaxed), 1);
        assert!(matches!(
            receiver.try_recv(),
            Ok(Event::MetadataFailed { index: 3, error })
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
            &events,
            || panic!("a successful job must not emit a panic event"),
        );
        assert!(next_ran);
        assert_eq!(deferred, None);
        assert!(matches!(
            receiver.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        assert!(queue.state.lock().unwrap().in_flight.is_empty());
    }

    #[test]
    fn non_metadata_worker_panic_is_an_image_failure_for_the_claimed_tier() {
        let payload = String::from("rehydrate invariant failed");
        let event = worker_panic_event(
            (8, Tier::Full),
            Action::Rehydrate,
            &payload as &(dyn std::any::Any + Send),
        )
        .unwrap();

        assert!(matches!(
            event,
            Event::ImageFailed {
                index: 8,
                tier: Tier::Full,
                error,
            } if error == "worker panicked: rehydrate invariant failed"
        ));
    }

    #[test]
    fn cancelled_worker_panic_does_not_publish_a_stale_failure() {
        let queue = JobQueue::new();
        queue.extend([((3, Tier::Thumb), 0, 0, Action::Metadata)]);
        let (id, action, token) = queue.pop().unwrap();
        token.cancel();
        let (events, receiver) = std::sync::mpsc::channel();

        execute_claimed_job(
            &queue,
            id,
            action,
            &token,
            || panic!("stale decoder panic"),
            &events,
            || panic!("a cancelled generation must not notify"),
        );

        assert!(matches!(
            receiver.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        assert!(queue.state.lock().unwrap().in_flight.is_empty());
    }

    #[test]
    fn panic_event_enqueue_precedes_reentrant_replan_notification() {
        let queue = JobQueue::new();
        let id = (3, Tier::Thumb);
        queue.extend([(id, 0, 0, Action::Metadata)]);
        let (claimed_id, action, token) = queue.pop().unwrap();
        let (events, receiver) = std::sync::mpsc::channel();
        let mut notified = false;

        execute_claimed_job(
            &queue,
            claimed_id,
            action,
            &token,
            || panic!("decoder panic before replan"),
            &events,
            || {
                notified = true;
                assert!(
                    queue.state.try_lock().is_ok(),
                    "notification ran while queue ownership was locked"
                );
                assert!(matches!(
                    receiver.try_recv(),
                    Ok(Event::MetadataFailed { index: 3, error })
                        if error == "worker panicked: decoder panic before replan"
                ));
                queue.set_plan(vec![(id, 0, 0, Action::Metadata)], false);
            },
        );

        assert!(notified);
        let (replacement_id, replacement_action, _) = queue.pop().unwrap();
        assert_eq!((replacement_id, replacement_action), (id, Action::Metadata));
        assert!(matches!(
            receiver.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
    }

    #[test]
    fn ordinary_event_enqueue_precedes_reentrant_replan_notification() {
        let queue = Arc::new(JobQueue::new());
        let id = (3, Tier::Browse);
        queue.extend([(id, 0, 0, Action::Metadata)]);
        let (claimed_id, _, token) = queue.pop().unwrap();
        let (events, receiver) = std::sync::mpsc::channel();
        let notified = Arc::new(AtomicBool::new(false));
        let notify_unlocked = Arc::new(AtomicBool::new(false));
        let notify_queue = queue.clone();
        let notified_callback = notified.clone();
        let notify_unlocked_callback = notify_unlocked.clone();
        let shared = Shared {
            entries: Arc::new(Vec::new()),
            cache: Arc::new(test_cache(0, 0, 0)),
            disk: None,
            events,
            notify: Arc::new(move || {
                notified_callback.store(true, Ordering::SeqCst);
                notify_unlocked_callback
                    .store(notify_queue.state.try_lock().is_ok(), Ordering::SeqCst);
                notify_queue.set_plan(vec![(id, 0, 0, Action::Develop(Quality::Browse))], false);
            }),
            processing_pool: None,
            heavy: JobQueue::new(),
            light: JobQueue::new(),
            persistence: PersistenceQueue::new(),
            persistence_known_present: Mutex::new(HashSet::new()),
            jpeg_quality: CACHE_JPEG_QUALITY,
            navigation: Mutex::new(NavigationOrder::default()),
        };

        publish_claimed(
            &shared,
            queue.as_ref(),
            claimed_id,
            &token,
            Event::ImageReady {
                index: id.0,
                tier: id.1,
            },
        );

        assert!(notified.load(Ordering::SeqCst));
        assert!(notify_unlocked.load(Ordering::SeqCst));
        assert!(matches!(
            receiver.try_recv(),
            Ok(Event::ImageReady {
                index: 3,
                tier: Tier::Browse
            })
        ));
        assert!(token.cancelled());
        assert!(queue.try_pop().is_none());
        queue.finish(claimed_id, &token);
        let (replacement_id, replacement_action, replacement_token) = queue.pop().unwrap();
        assert_eq!(
            (replacement_id, replacement_action),
            (id, Action::Develop(Quality::Browse))
        );
        queue.finish(replacement_id, &replacement_token);
    }

    #[test]
    fn ordinary_event_publication_rejects_a_replanned_generation() {
        let queue = JobQueue::new();
        let id = (3, Tier::Browse);
        queue.extend([(id, 0, 0, Action::Metadata)]);
        let (claimed_id, _, stale_token) = queue.pop().unwrap();
        queue.set_plan(vec![(id, 0, 0, Action::Develop(Quality::Browse))], false);
        let (events, receiver) = std::sync::mpsc::channel();

        assert!(stale_token.cancelled());
        assert!(!queue.enqueue_current_event(
            claimed_id,
            &stale_token,
            &events,
            Event::ImageReady {
                index: id.0,
                tier: id.1,
            },
        ));
        assert!(matches!(
            receiver.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        assert!(queue.try_pop().is_none());
        queue.finish(claimed_id, &stale_token);

        let (replacement_id, replacement_action, replacement_token) = queue.pop().unwrap();
        assert_eq!(
            (replacement_id, replacement_action),
            (id, Action::Develop(Quality::Browse))
        );
        assert!(queue.enqueue_current_event(
            replacement_id,
            &replacement_token,
            &events,
            Event::ImageReady {
                index: id.0,
                tier: id.1,
            },
        ));
        assert!(matches!(
            receiver.try_recv(),
            Ok(Event::ImageReady {
                index: 3,
                tier: Tier::Browse
            })
        ));
        queue.finish(replacement_id, &replacement_token);
    }

    #[test]
    fn background_warm_panic_has_no_user_visible_event() {
        let payload = "warm decoder panic";
        assert!(
            worker_panic_event(
                (8, Tier::Browse),
                Action::WarmDevelop(Quality::Browse),
                &payload as &(dyn std::any::Any + Send),
            )
            .is_none()
        );
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

    fn textured_buf(width: u32, height: u32) -> PixelBuf {
        let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
        let mut state = 0x9E37_79B9u32;
        for y in 0..height {
            for x in 0..width {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let noise = (state >> 24) as u8;
                let edge = if (x / 37 + y / 23) % 2 == 0 { 200 } else { 40 };
                rgba.extend_from_slice(&[
                    ((x * 255) / width.max(1)) as u8,
                    ((y * 255) / height.max(1)) as u8 ^ (noise >> 3),
                    edge ^ (noise >> 2),
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

    fn jpeg_has_444_sampling(bytes: &[u8]) -> bool {
        let Some([0xff, 0xd8]) = bytes.get(..2) else {
            return false;
        };
        let mut offset = 2;
        while offset + 4 <= bytes.len() {
            if bytes[offset] != 0xff {
                return false;
            }
            while offset < bytes.len() && bytes[offset] == 0xff {
                offset += 1;
            }
            let Some(&marker) = bytes.get(offset) else {
                return false;
            };
            offset += 1;
            if marker == 0xd9 || marker == 0xda {
                return false;
            }
            if marker == 0x01 || (0xd0..=0xd7).contains(&marker) {
                continue;
            }
            let Some(length_bytes) = bytes.get(offset..offset + 2) else {
                return false;
            };
            let length = usize::from(u16::from_be_bytes([length_bytes[0], length_bytes[1]]));
            if length < 2 || offset + length > bytes.len() {
                return false;
            }
            if matches!(
                marker,
                0xc0 | 0xc1
                    | 0xc2
                    | 0xc3
                    | 0xc5
                    | 0xc6
                    | 0xc7
                    | 0xc9
                    | 0xca
                    | 0xcb
                    | 0xcd
                    | 0xce
                    | 0xcf
            ) {
                let segment = &bytes[offset + 2..offset + length];
                if segment.len() < 9 {
                    return false;
                }
                let components = usize::from(segment[5]);
                return components == 3
                    && segment.len() >= 6 + components * 3
                    && (0..components).all(|component| segment[7 + component * 3] == 0x11);
            }
            offset += length;
        }
        false
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
        persistence_shared_with_quality(
            entries,
            cache,
            disk,
            pending_budget_bytes,
            CACHE_JPEG_QUALITY,
        )
    }

    fn persistence_shared_with_quality(
        entries: Vec<FolderEntry>,
        cache: Arc<RamCache>,
        disk: Option<DiskCache>,
        pending_budget_bytes: usize,
        jpeg_quality: u8,
    ) -> Arc<Shared> {
        persistence_shared_with_options(
            entries,
            cache,
            disk,
            pending_budget_bytes,
            jpeg_quality,
            false,
        )
    }

    fn persistence_shared_with_options(
        entries: Vec<FolderEntry>,
        cache: Arc<RamCache>,
        disk: Option<DiskCache>,
        pending_budget_bytes: usize,
        jpeg_quality: u8,
        fixed_processing_limit: bool,
    ) -> Arc<Shared> {
        let (events, _receiver) = std::sync::mpsc::channel();
        Arc::new(Shared {
            entries: Arc::new(entries),
            cache,
            disk,
            events,
            notify: Arc::new(|| {}),
            processing_pool: fixed_processing_limit.then(test_processing_pool),
            heavy: JobQueue::new(),
            light: JobQueue::new(),
            persistence: PersistenceQueue::with_budget(pending_budget_bytes),
            persistence_known_present: Mutex::new(HashSet::new()),
            jpeg_quality,
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
    fn cancelled_rehydrate_cannot_publish_pixels_to_the_full_ring() {
        let cache = Arc::new(RamCache::new(RamCacheBudgets::new(0, 0, 64, 0)));
        cache.set_navigation_policy([], [(0, Tier::Full)]);
        let shared = persistence_shared(Vec::new(), cache.clone(), None, 0);
        let token = CancelToken::default();
        token.cancel();
        let events = Mutex::new(Vec::new());

        install_rehydrated(
            &shared,
            0,
            Tier::Full,
            patterned_buf(2, 2),
            &token,
            &|event| events.lock().unwrap().push(event),
        );

        assert!(!cache.has_rgba((0, Tier::Full)));
        assert!(events.lock().unwrap().is_empty());
    }

    #[test]
    fn rehydrate_rejected_by_a_new_working_set_cannot_emit_ready() {
        let cache = Arc::new(RamCache::new(RamCacheBudgets::new(0, 0, 64, 0)));
        cache.set_navigation_policy([], [(1, Tier::Full)]);
        let shared = persistence_shared(Vec::new(), cache.clone(), None, 0);
        let token = CancelToken::default();
        let events = Mutex::new(Vec::new());

        install_rehydrated(
            &shared,
            0,
            Tier::Full,
            patterned_buf(2, 2),
            &token,
            &|event| events.lock().unwrap().push(event),
        );

        assert!(!cache.has_rgba((0, Tier::Full)));
        assert!(events.lock().unwrap().is_empty());
    }

    #[test]
    fn resident_full_promotion_enqueues_persistence_without_redeveloping() {
        let cache = Arc::new(RamCache::new(RamCacheBudgets::new(0, 0, 64, 64)));
        cache.set_navigation_policy([], [(0, Tier::Full)]);
        cache.insert_rgba((0, Tier::Full), Arc::new(patterned_buf(2, 2)));
        let shared = persistence_shared(vec![entry("promoted.arw", 100)], cache, None, 64);

        run_persist_resident(&shared, 0, Tier::Full, &CancelToken::default());

        let state = shared.persistence.state.lock().unwrap();
        assert_eq!(state.pending.len(), 1);
        assert!(state.pending.contains_key(&(0, Tier::Full)));
    }

    #[test]
    fn known_disk_persistence_is_reused_until_rehydrate_finds_it_missing() {
        let dir = tempfile::tempdir().unwrap();
        let raw_entry = entry(dir.path().join("promoted.arw"), 100);
        let disk = DiskCache::open_at(dir.path().join("cache"));
        let key = DiskCache::key(&raw_entry, Tier::Full);
        disk.put(&key, b"existing object").unwrap();
        let cache = Arc::new(RamCache::new(RamCacheBudgets::new(0, 0, 64, 64)));
        cache.set_navigation_policy([], [(0, Tier::Full)]);
        cache.insert_rgba((0, Tier::Full), Arc::new(patterned_buf(2, 2)));
        let shared = persistence_shared(vec![raw_entry], cache, Some(disk.clone()), 64);

        run_persist_resident(&shared, 0, Tier::Full, &CancelToken::default());
        assert!(
            shared
                .persistence_known_present
                .lock()
                .unwrap()
                .contains(&(0, Tier::Full))
        );
        disk.remove(&key).unwrap();
        run_persist_resident(&shared, 0, Tier::Full, &CancelToken::default());

        assert!(shared.persistence.state.lock().unwrap().pending.is_empty());

        // Losing the observed object must become recoverable once the Full
        // pixels leave RAM and a later rehydrate observes the disk miss.
        shared.cache.set_navigation_policy([], []);
        shared.cache.set_navigation_policy([], [(0, Tier::Full)]);
        run_rehydrate(
            &shared,
            0,
            Tier::Full,
            &CancelToken::default(),
            DevelopMode::Display,
            &|_| {},
        );
        assert!(
            !shared
                .persistence_known_present
                .lock()
                .unwrap()
                .contains(&(0, Tier::Full))
        );

        shared
            .cache
            .insert_rgba((0, Tier::Full), Arc::new(patterned_buf(2, 2)));
        run_persist_resident(&shared, 0, Tier::Full, &CancelToken::default());
        assert!(
            shared
                .persistence
                .state
                .lock()
                .unwrap()
                .pending
                .contains_key(&(0, Tier::Full))
        );
    }

    #[test]
    fn replacement_generation_reuses_pixels_from_a_late_cancelled_completion() {
        let cache = Arc::new(RamCache::new(RamCacheBudgets::new(0, 0, 64, 64)));
        cache.set_navigation_policy([], [(0, Tier::Full)]);
        cache.insert_rgba((0, Tier::Full), Arc::new(patterned_buf(2, 2)));
        let shared = persistence_shared(
            vec![entry("intentionally-missing.arw", 100)],
            cache,
            None,
            64,
        );
        let id = (0, Tier::Full);
        shared
            .heavy
            .set_plan(vec![(id, 0, 0, Action::Develop(Quality::Full))], true);
        let (_, action, token) = shared.heavy.pop().unwrap();

        assert_eq!(
            run_job(&shared, &shared.heavy, id, action, &token),
            JobCompletion::Complete
        );
        assert_eq!(shared.persistence.state.lock().unwrap().pending.len(), 1);
        shared.heavy.finish(id, &token);
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
    fn navigation_pins_preserve_full_for_current_and_near_window() {
        let pins = navigation_pins(5, 2, 2, &[]);
        assert_eq!(pins.len(), 9);
        let full: Vec<_> = pins
            .iter()
            .filter(|(_, tier)| *tier == Tier::Full)
            .copied()
            .collect();
        assert_eq!(full, [(1, Tier::Full), (2, Tier::Full), (3, Tier::Full)]);
    }

    #[test]
    fn filtered_pins_follow_visible_neighbors() {
        let pins = navigation_pins(10, 4, 1, &[1, 4, 8]);
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

        order.replace_indices(10, vec![1, 4, 8]);
        assert_eq!(order.indices, [1, 4, 8]);
        assert!(order.update_navigation(nav));
    }

    #[test]
    fn navigation_order_normalizes_filtered_indices_once() {
        let mut order = NavigationOrder::default();
        order.replace_indices(10, vec![usize::MAX, 4, 5, 5, 6, usize::MAX - 1]);

        assert_eq!(order.indices, [4, 5, 6]);
        assert_eq!(order.current_position(5), 1);
        let pins = navigation_pins(10, 5, order.current_position(5), &order.indices);
        let full: Vec<_> = pins
            .into_iter()
            .filter(|(_, tier)| *tier == Tier::Full)
            .map(|(index, _)| index)
            .collect();
        assert_eq!(full, [4, 5, 6]);
    }

    #[test]
    fn byte_bounded_browse_wave_reaches_a_resident_fixed_point() {
        let cache = RamCache::new(RamCacheBudgets::new(0, 400, 700, 0));
        let mut final_targets = Vec::new();
        for _ in 0..3 {
            let (full_snapshot, browse_snapshot) = cache.prefetch_snapshots();
            let budgets = NavigationPrefetchBudgets::new(
                FullPrefetchBudget::from_observations(
                    full_snapshot.budget_bytes,
                    full_snapshot.fallback_bytes,
                    full_snapshot.per_index_bytes,
                ),
                BrowsePrefetchBudget::from_observations(
                    browse_snapshot.budget_bytes,
                    browse_snapshot.fallback_bytes,
                    browse_snapshot.per_index_bytes,
                ),
            );
            final_targets = build_plan_targets_with_normalized_prefetch(
                100,
                50,
                1,
                false,
                (&[], 50),
                &budgets,
                false,
            );
            cache.set_navigation_policy(
                [],
                final_targets
                    .iter()
                    .filter(|target| target.tier == Tier::Full)
                    .map(|target| (target.index, Tier::Full)),
            );
            for target in final_targets
                .iter()
                .filter(|target| target.tier == Tier::Browse)
            {
                if !cache.has_rgba((target.index, Tier::Browse)) {
                    cache.insert_rgba(
                        (target.index, Tier::Browse),
                        Arc::new(PixelBuf {
                            width: 1,
                            height: 1,
                            rgba: vec![0; 100],
                        }),
                    );
                }
            }
        }

        let browse: Vec<_> = final_targets
            .iter()
            .filter(|target| target.tier == Tier::Browse)
            .map(|target| (target.index, Tier::Browse))
            .collect();
        assert_eq!(browse.len(), 4);
        assert!(browse.iter().all(|key| cache.has_rgba(*key)));
        assert_eq!(cache.stats().browse_rgba_bytes, 400);
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
        assert!(q.try_pop().is_none());

        // The cooperative background generation exits before the foreground
        // replacement can claim the same source file. Its canceled one-shot
        // obligation returns behind the untouched background item.
        q.finish_with(id, &background_token, JobCompletion::RetryBackground);
        let (foreground_id, foreground_action, foreground_token) = q.pop().unwrap();
        assert_eq!(foreground_id, id);
        assert_eq!(foreground_action, Action::Develop(Quality::Browse));
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
        assert!(q.try_pop().is_none());
        assert!(metadata_token.cancelled());
        q.finish(requested, &metadata_token);

        let (id, action, thumbnail_token) = q.pop().unwrap();
        assert_eq!(id, requested);
        assert_eq!(action, Action::Thumb);
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
    fn cancelled_generation_must_exit_before_the_same_id_is_reclaimed() {
        let q = JobQueue::new();
        let id = (7, Tier::Full);
        q.set_plan(vec![(id, 0, 0, Action::Develop(Quality::Full))], true);
        let (_, _, stale) = q.pop().unwrap();

        q.set_plan(Vec::new(), true);
        q.set_plan(vec![(id, 0, 0, Action::Develop(Quality::Full))], true);
        assert!(stale.cancelled());
        assert!(q.try_pop().is_none());

        q.finish(id, &stale);
        let (replacement_id, _, replacement) = q.try_pop().unwrap();
        assert_eq!(replacement_id, id);
        assert!(!Arc::ptr_eq(&stale, &replacement));
        q.finish(id, &replacement);
    }

    #[test]
    fn speculative_full_uses_one_worker_and_yields_to_lower_priority_foreground() {
        let q = JobQueue::new();
        q.set_plan(
            vec![
                ((0, Tier::Full), 5, 1, Action::PrefetchFull),
                ((1, Tier::Full), 5, 2, Action::PrefetchFull),
                ((2, Tier::Browse), 6, 0, Action::Develop(Quality::Browse)),
            ],
            true,
        );

        let (first, first_action, first_token) = q.pop().unwrap();
        assert_eq!(
            (first, first_action),
            ((0, Tier::Full), Action::PrefetchFull)
        );
        let (second, second_action, second_token) = q.try_pop().unwrap();
        assert_eq!(
            (second, second_action),
            ((2, Tier::Browse), Action::Develop(Quality::Browse))
        );
        assert!(q.try_pop().is_none());

        q.finish(first, &first_token);
        let (third, third_action, third_token) = q.try_pop().unwrap();
        assert_eq!(
            (third, third_action),
            ((1, Tier::Full), Action::PrefetchFull)
        );
        q.finish(second, &second_token);
        q.finish(third, &third_token);
    }

    #[test]
    fn failed_speculative_full_is_suppressed_until_foreground_demand() {
        let q = JobQueue::new();
        let failed = (7, Tier::Full);
        q.set_plan(vec![(failed, 5, 1, Action::PrefetchFull)], true);
        let (_, _, failed_token) = q.pop().unwrap();
        q.finish_with(failed, &failed_token, JobCompletion::SuppressSpeculative);

        let foreground = (8, Tier::Browse);
        q.set_plan(
            vec![
                (failed, 5, 1, Action::PrefetchFull),
                (foreground, 0, 0, Action::Develop(Quality::Browse)),
            ],
            false,
        );
        let (claimed, _, foreground_token) = q.try_pop().unwrap();
        assert_eq!(claimed, foreground);
        assert!(q.try_pop().is_none());
        q.finish(foreground, &foreground_token);

        q.set_plan(vec![(failed, 0, 0, Action::Develop(Quality::Full))], false);
        let (retried, action, retry_token) = q.try_pop().unwrap();
        assert_eq!((retried, action), (failed, Action::Develop(Quality::Full)));
        q.finish(retried, &retry_token);
    }

    #[test]
    fn navigation_change_retries_a_failed_speculative_full_target() {
        let q = JobQueue::new();
        let failed = (7, Tier::Full);
        q.set_plan(vec![(failed, 5, 1, Action::PrefetchFull)], true);
        let (_, _, failed_token) = q.pop().unwrap();
        q.finish_with(failed, &failed_token, JobCompletion::SuppressSpeculative);

        q.set_plan(vec![(failed, 5, 1, Action::PrefetchFull)], true);
        let (retried, action, retry_token) = q.try_pop().unwrap();
        assert_eq!((retried, action), (failed, Action::PrefetchFull));
        q.finish(retried, &retry_token);
    }

    #[test]
    fn folder_warming_uses_at_most_one_worker() {
        let q = JobQueue::new();
        assert!(q.initialize_background(|| {
            [
                ((0, Tier::Browse), Action::WarmDevelop(Quality::Browse)),
                ((1, Tier::Browse), Action::WarmDevelop(Quality::Browse)),
            ]
        }));

        let (first, _, first_token) = q.pop().unwrap();
        assert_eq!(first, (0, Tier::Browse));
        assert!(q.try_pop().is_none());

        q.finish(first, &first_token);
        let (second, _, second_token) = q.try_pop().unwrap();
        assert_eq!(second, (1, Tier::Browse));
        q.finish(second, &second_token);
    }

    #[test]
    fn live_speculative_full_can_be_promoted_without_restart() {
        let q = JobQueue::new();
        let id = (7, Tier::Full);
        q.set_plan(vec![(id, 5, 1, Action::PrefetchFull)], true);
        let (_, _, token) = q.pop().unwrap();

        q.set_plan(vec![(id, 0, 0, Action::Develop(Quality::Full))], true);
        assert!(!token.cancelled());
        assert!(q.try_pop().is_none());
        q.finish(id, &token);
    }

    #[test]
    fn demoted_full_work_exits_before_optional_full_replacement() {
        let q = JobQueue::new();
        let demoted = (7, Tier::Full);
        let other = (8, Tier::Full);
        q.set_plan(vec![(demoted, 0, 0, Action::Develop(Quality::Full))], true);
        let (_, _, display_token) = q.pop().unwrap();

        q.set_plan(
            vec![
                (demoted, 5, 1, Action::PrefetchFull),
                (other, 5, 2, Action::PrefetchFull),
            ],
            true,
        );
        assert!(display_token.cancelled());
        assert!(q.try_pop().is_none());

        q.finish(demoted, &display_token);
        let (claimed, action, replacement_token) = q.try_pop().unwrap();
        assert!(matches!(claimed, id if id == demoted || id == other));
        assert_eq!(action, Action::PrefetchFull);
        assert!(q.try_pop().is_none());
        q.finish(claimed, &replacement_token);
    }

    #[test]
    fn small_processing_pool_serializes_optional_full_and_folder_warm_lanes() {
        let q = JobQueue::new_with_parallel_background_lanes(false);
        q.initialize_background(|| [((9, Tier::Browse), Action::WarmDevelop(Quality::Browse))]);
        q.set_plan(vec![((7, Tier::Full), 5, 1, Action::PrefetchFull)], false);

        let (full, _, full_token) = q.pop().unwrap();
        assert_eq!(full, (7, Tier::Full));
        assert!(q.try_pop().is_none());
        q.finish(full, &full_token);

        let (warm_id, warm_action, warm_token) = q.try_pop().unwrap();
        assert_eq!(warm_id, (9, Tier::Browse));
        assert_eq!(warm_action, Action::WarmDevelop(Quality::Browse));
        q.finish(warm_id, &warm_token);
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
        assert!(q.try_pop().is_none());
        q.finish(id, &warm_token);

        let (_, action, display_token) = q.pop().unwrap();
        assert_eq!(action, Action::Develop(Quality::Browse));
        assert!(!display_token.cancelled());
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
            cache: Arc::new(test_cache(0, 0, 0)),
            disk: Some(disk.clone()),
            events,
            notify: Arc::new(|| {}),
            processing_pool: None,
            heavy: JobQueue::new(),
            light: JobQueue::new(),
            persistence: PersistenceQueue::new(),
            persistence_known_present: Mutex::new(HashSet::new()),
            jpeg_quality: CACHE_JPEG_QUALITY,
            navigation: Mutex::new(NavigationOrder::default()),
        };

        run_warm_develop(
            &shared,
            0,
            Tier::Browse,
            Quality::Browse,
            &CancelToken::default(),
            &|event| publish(&shared, event),
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
        let cache = Arc::new(test_cache(0, 0, 1024 * 1024));
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
                &|event| publish(&shared, event),
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
        let cache = Arc::new(test_cache(0, 0, 1024 * 1024));
        let shared = persistence_shared_with_options(
            entries,
            cache.clone(),
            Some(disk.clone()),
            1024 * 1024,
            CACHE_JPEG_QUALITY,
            true,
        );

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
        let cache = Arc::new(test_cache(0, 0, 1024 * 1024));
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
    fn non_default_quality_persists_and_rehydrates_only_its_cache_profile() {
        let dir = tempfile::tempdir().unwrap();
        let disk = DiskCache::open_at(dir.path().join("cache"));
        let entries = vec![entry(dir.path().join("quality-90.arw"), 100)];
        let default_key = DiskCache::key(&entries[0], Tier::Browse);
        let selected_key = DiskCache::key_with_jpeg_quality(&entries[0], Tier::Browse, 90);
        let cache = Arc::new(test_cache(0, 0, 1024 * 1024));
        let shared = persistence_shared_with_quality(
            entries,
            cache.clone(),
            Some(disk.clone()),
            1024 * 1024,
            90,
        );

        assert_eq!(
            shared.persistence.try_enqueue(persistence_request(
                (0, Tier::Browse),
                Arc::new(patterned_buf(64, 48)),
                false,
            )),
            PersistenceEnqueue::Queued
        );
        let worker_shared = shared.clone();
        let worker = std::thread::spawn(move || persistence_worker(&worker_shared));
        shared.persistence.close();
        worker.join().unwrap();

        assert!(!disk.has(&default_key));
        assert!(disk.has(&selected_key));
        assert!(!cache.has_jpeg((0, Tier::Browse)));
        cache.set_navigation_policy([(0, Tier::Browse)], []);

        run_rehydrate(
            &shared,
            0,
            Tier::Browse,
            &CancelToken::default(),
            DevelopMode::Display,
            &|event| publish(&shared, event),
        );
        assert!(cache.has_jpeg((0, Tier::Browse)));
    }

    #[test]
    fn corrupt_cached_jpeg_is_evicted_before_raw_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let raw_entry = entry(dir.path().join("missing.arw"), 100);
        let disk = DiskCache::open_at(dir.path().join("cache"));
        let key = DiskCache::key(&raw_entry, Tier::Browse);
        disk.put(&key, b"not a jpeg").unwrap();

        let cache = Arc::new(test_cache(0, 0, 1024));
        cache.insert_jpeg((0, Tier::Browse), Arc::new(b"not a jpeg".to_vec()));
        let (events, receiver) = std::sync::mpsc::channel();
        let shared = Shared {
            entries: Arc::new(vec![raw_entry]),
            cache: cache.clone(),
            disk: Some(disk.clone()),
            events,
            notify: Arc::new(|| {}),
            processing_pool: None,
            heavy: JobQueue::new(),
            light: JobQueue::new(),
            persistence: PersistenceQueue::new(),
            persistence_known_present: Mutex::new(HashSet::new()),
            jpeg_quality: CACHE_JPEG_QUALITY,
            navigation: Mutex::new(NavigationOrder::default()),
        };

        run_rehydrate(
            &shared,
            0,
            Tier::Browse,
            &CancelToken::default(),
            DevelopMode::Display,
            &|event| publish(&shared, event),
        );

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
        assert!(jpeg_has_444_sampling(&encoded));
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
    fn production_jpeg_quality_preserves_dark_gradients_better_than_legacy_full() {
        let width = 640_u32;
        let height = 384_u32;
        let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
        for y in 0..height {
            for x in 0..width {
                let base = 6 + ((x + y) * 58 / (width + height));
                let hash = x.wrapping_mul(0x9e37_79b9) ^ y.wrapping_mul(0x85eb_ca6b);
                let noise = ((hash ^ (hash >> 13)) % 5) as i16 - 2;
                let value = (base as i16 + noise).clamp(0, 255) as u8;
                rgba.extend_from_slice(&[value, value, value, 255]);
            }
        }
        let source = PixelBuf {
            width,
            height,
            rgba,
        };
        let squared_error = |quality| {
            let encoded = encode_jpeg(&source, quality).expect("gradient must encode");
            let decoded = decode_jpeg(&encoded).expect("gradient must decode");
            source
                .rgba
                .chunks_exact(4)
                .zip(decoded.rgba.chunks_exact(4))
                .map(|(expected, actual)| {
                    (0..3)
                        .map(|channel| {
                            u64::from(expected[channel].abs_diff(actual[channel])).pow(2)
                        })
                        .sum::<u64>()
                })
                .sum::<u64>()
        };

        assert_eq!(CACHE_JPEG_QUALITY, 97);
        let legacy_error = squared_error(90);
        let production_error = squared_error(CACHE_JPEG_QUALITY);
        assert!(
            production_error * 4 < legacy_error * 3,
            "q97 error {production_error} was not at least 25% below q90 error {legacy_error}"
        );
    }

    #[test]
    fn jpeg_codec_rejects_invalid_inputs() {
        assert!(decode_jpeg(b"not a jpeg").is_err());
        let valid = patterned_buf(8, 8);
        assert!(encode_jpeg(&valid, 0).is_err());
        assert!(encode_jpeg(&valid, 101).is_err());
        let zero_width = PixelBuf {
            width: 0,
            height: 1,
            rgba: Vec::new(),
        };
        assert!(encode_jpeg(&zero_width, 90).is_err());
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

    #[test]
    fn jpeg_input_validation_is_independent_of_the_encoding_pool() {
        let valid = patterned_buf(8, 6);
        assert_eq!(validate_jpeg_input(&valid, 97), Ok((8, 6, 32)));

        let malformed = PixelBuf {
            width: 8,
            height: 6,
            rgba: vec![0; 8 * 6 * 4 - 1],
        };
        assert!(validate_jpeg_input(&malformed, 97).is_err());
        assert!(validate_jpeg_input(&valid, 0).is_err());
        assert!(validate_jpeg_input(&valid, 101).is_err());
    }

    #[test]
    fn jpeg_pool_is_bounded_and_separate_from_the_global_pool() {
        let pool = jpeg_pool().unwrap();
        assert!(pool.current_num_threads() <= MAX_JPEG_WORKERS);
        assert!(pool.current_num_threads() >= 1);
        let thread_name = pool
            .install(|| std::thread::current().name().map(str::to_owned))
            .expect("the dedicated JPEG worker has a name");
        assert!(thread_name.starts_with("viewr-jpeg-"));
    }

    #[test]
    fn jpeg_encoder_does_not_leak_state_between_images() {
        let first = patterned_buf(96, 64);
        let second = patterned_buf(127, 93);

        let encoded_first = encode_jpeg(&first, 97).unwrap();
        let encoded_second = encode_jpeg(&second, 97).unwrap();

        assert_eq!(encoded_first, encode_jpeg(&first, 97).unwrap());
        assert_eq!(encoded_second, encode_jpeg(&second, 97).unwrap());
        assert_eq!(
            decode_jpeg(&encoded_second)
                .map(|decoded| (decoded.width, decoded.height))
                .unwrap(),
            (second.width, second.height)
        );
    }

    #[test]
    fn jpeg_encoder_recovers_after_a_bad_request_and_quality_change() {
        let valid = patterned_buf(96, 64);
        let malformed = PixelBuf {
            width: 96,
            height: 64,
            rgba: vec![0; 17],
        };
        let first = encode_jpeg(&valid, 97).unwrap();
        assert!(encode_jpeg(&malformed, 97).is_err());
        let recovered = encode_jpeg(&valid, 97).unwrap();

        assert_eq!(first, recovered);

        let changed_quality = encode_jpeg(&valid, 90).unwrap();
        assert_eq!(changed_quality, encode_jpeg(&valid, 90).unwrap());
        assert_ne!(changed_quality, first);
    }
}
