//! The engine: worker pool, priority scheduling with real cancellation,
//! outward prefetch planning, and cache filling.
//!
//! Design (from the plan):
//! - Declarative planning: on every navigation the desired job set is
//!   recomputed and synced; queued jobs outside it vanish, in-flight jobs
//!   outside it get their cancel token flipped, jobs still wanted keep
//!   running. Epochs make stale heap entries inert — nothing toggles.
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

use crate::cache_disk::DiskCache;
use crate::cache_ram::RamCache;
use crate::decode;
use crate::develop::{Quality, develop};
use crate::folder::{FolderEntry, outward_order};
use crate::meta::FileMeta;
use crate::planning::{PlanKind, build_plan_targets};
use crate::resize::apply_orient;
use crate::types::{PixelBuf, Tier};

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

#[derive(Debug)]
pub enum Event {
    MetadataReady {
        index: usize,
        meta: Box<FileMeta>,
    },
    ThumbReady {
        index: usize,
        meta: Box<FileMeta>,
    },
    ImageReady {
        index: usize,
        tier: Tier,
    },
    ImageFailed {
        index: usize,
        tier: Tier,
        error: String,
    },
    MetadataFailed {
        index: usize,
        error: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NavState {
    pub current: usize,
    /// +1 browsing forward, -1 backward.
    pub direction: i8,
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
pub struct CancelToken(AtomicBool);

impl CancelToken {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }
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
    epoch: u64,
    seq: u64,
    closed: bool,
}

struct JobQueue {
    state: Mutex<QueueState>,
    cond: Condvar,
}

struct PersistenceRequest {
    id: JobId,
    pixels: Arc<PixelBuf>,
    /// Warm-only work populates disk but deliberately skips the RAM JPEG ring.
    insert_ram: bool,
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
    Saturated,
    Busy,
    Closed,
}

struct ActivePersistence {
    id: JobId,
    insert_ram: bool,
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
/// Producers use `try_lock`, so a completed develop never waits behind JPEG
/// work. Requests for an active or pending ID coalesce; otherwise excess work
/// is dropped as a best-effort cache miss.
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

    fn try_enqueue(&self, request: PersistenceRequest) -> PersistenceEnqueue {
        let mut state = match self.state.try_lock() {
            Ok(state) => state,
            Err(TryLockError::WouldBlock) => return PersistenceEnqueue::Busy,
            Err(TryLockError::Poisoned(_)) => return PersistenceEnqueue::Closed,
        };
        if state.closed {
            return PersistenceEnqueue::Closed;
        }

        if let Some(active) = state.active.as_mut()
            && active.id == request.id
        {
            active.insert_ram |= request.insert_ram;
            return PersistenceEnqueue::Coalesced;
        }
        if let Some(pending) = state.pending.get_mut(&request.id) {
            pending.insert_ram |= request.insert_ram;
            return PersistenceEnqueue::Coalesced;
        }

        let retained_bytes = request.retained_bytes();
        if retained_bytes > self.pending_budget_bytes
            || state.pending_bytes > self.pending_budget_bytes - retained_bytes
        {
            return PersistenceEnqueue::Saturated;
        }

        state.pending_bytes += retained_bytes;
        state.order.push_back(request.id);
        state.pending.insert(request.id, request);
        drop(state);
        self.ready.notify_one();
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
                });
                return Some(request);
            }
            if state.closed {
                return None;
            }
            state = self.ready.wait(state).unwrap();
        }
    }

    /// Finish the active request and return the RAM-insert requirement merged
    /// from every duplicate that arrived while it was being persisted.
    fn finish(&self, id: JobId) -> bool {
        let mut state = self.state.lock().unwrap();
        let active = state
            .active
            .take()
            .expect("a persistence request must be active when it finishes");
        debug_assert_eq!(active.id, id);
        active.insert_ram
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

    /// Replace the desired job set. In-flight jobs no longer wanted are
    /// cancelled; in-flight jobs still wanted keep running (not duplicated).
    fn set_plan(&self, plan: Vec<(JobId, u8, u32, Action)>) {
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
        for (id, running) in &state.in_flight {
            if state
                .queued
                .get(id)
                .is_none_or(|wanted| wanted.action != running.action)
            {
                running.token.cancel();
            }
        }

        // Reuse the heap's backing allocation, then heapify once in O(P).
        // Repeated push would make each navigation O(P log P), which is
        // noticeable when the folder-wide warm wave contains many images.
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

    /// Append jobs without disturbing the existing plan (used for the
    /// one-shot thumb wave).
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
            state = self.cond.wait(state).unwrap();
        }
    }

    fn finish(&self, id: JobId, token: &Arc<CancelToken>) {
        let mut state = self.state.lock().unwrap();
        if state
            .in_flight
            .get(&id)
            .is_some_and(|current| Arc::ptr_eq(&current.token, token))
        {
            state.in_flight.remove(&id);
        }
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
        for running in state.in_flight.values() {
            running.token.cancel();
        }
        drop(state);
        self.cond.notify_all();
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
    /// Display order the prefetch wave follows (filtered view). Empty ⇒
    /// identity. Distances are positions in this sequence, so with a
    /// rating filter active the wave targets the next *visible* images.
    sequence: Mutex<Vec<usize>>,
}

pub struct Engine {
    shared: Arc<Shared>,
    workers: Vec<std::thread::JoinHandle<()>>,
    persistence_worker: Option<std::thread::JoinHandle<()>>,
    gc_worker: Option<std::thread::JoinHandle<()>>,
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

impl Engine {
    /// Spawns the worker pool and queues the outward metadata wave.
    /// `notify` is called after every published result (the app passes
    /// `ctx.request_repaint`).
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
            sequence: Mutex::new(Vec::new()),
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

        // Background disk-cache GC sweep on open.
        let gc_worker = shared.disk.clone().map(|disk| {
            std::thread::Builder::new()
                .name("viewr-cache-gc".into())
                .spawn(move || {
                    disk.gc_to_budget();
                })
                .expect("failed to spawn disk cache GC worker")
        });

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
                gc_worker,
            },
            rx,
        )
    }

    /// Recompute and sync the heavy plan for a navigation state. Cheap;
    /// call on every nav/zoom change and on job completion events.
    pub fn navigate(&self, nav: NavState) {
        let len = self.shared.entries.len();
        if len == 0 {
            return;
        }
        let current = nav.current.min(len - 1);
        let cache = &self.shared.cache;

        let disk = &self.shared.disk;
        let (pins, targets) = {
            let s = self.shared.sequence.lock().unwrap();
            (
                navigation_pins(len, current, nav.zoomed, &s),
                build_plan_targets(len, current, nav.direction, nav.zoomed, &s, disk.is_some()),
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
                    if disk.is_some() {
                        // RAM and disk presence are checked when this
                        // background job reaches a worker. Besides removing
                        // synchronous filesystem probes, this avoids taking
                        // the RAM-cache mutex once per file during navigation.
                        plan.push((
                            id,
                            target.class,
                            target.effective_distance,
                            Action::WarmDevelop(Quality::Browse),
                        ));
                    }
                }
            }
        }

        self.shared.heavy.set_plan(plan);
    }

    /// Set the display order (filtered view) the wave follows.
    /// Call `navigate` afterwards to apply.
    pub fn set_sequence(&self, sequence: Vec<usize>) {
        *self.shared.sequence.lock().unwrap() = sequence;
    }

    /// Replace the thumbnail viewport demand lane. It is intentionally
    /// separate from the folder-wide metadata wave so rapid scrolling drops
    /// stale urgency. Claiming a thumbnail displaces queued metadata for that
    /// file because the thumbnail decode returns the same metadata.
    pub fn set_thumbnail_demand(&self, indices: &[usize]) -> bool {
        self.shared.light.set_urgent(
            indices
                .iter()
                .copied()
                .filter(|index| *index < self.shared.entries.len())
                .map(|index| ((index, Tier::Thumb), Action::Thumb)),
        )
    }

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
        if let Some(worker) = self.gc_worker.take() {
            let _ = worker.join();
        }
    }
}

fn worker(shared: &Shared, light: bool) {
    let queue = if light { &shared.light } else { &shared.heavy };
    while let Some((id, action, token)) = queue.pop() {
        run_job(shared, id, action, &token);
        queue.finish(id, &token);
    }
}

fn publish(shared: &Shared, event: Event) {
    let _ = shared.events.send(event);
    (shared.notify)();
}

fn persistence_worker(shared: &Shared) {
    while let Some(request) = shared.persistence.pop() {
        // This lane is intentionally single-threaded and yields before CPU
        // work so interactive develop workers retain scheduling priority.
        std::thread::yield_now();
        let quality = match request.id.1 {
            Tier::Full => JPEG_QUALITY_FULL,
            _ => JPEG_QUALITY_BROWSE,
        };
        let encoded = encode_jpeg(&request.pixels, quality);
        if let Ok(bytes) = &encoded
            && let Some(disk) = &shared.disk
        {
            let key = DiskCache::key(&shared.entries[request.id.0], request.id.1);
            if let Err(e) = disk.put(&key, bytes) {
                eprintln!("disk cache write failed: {e}");
            }
        }

        let insert_ram = shared.persistence.finish(request.id);
        if insert_ram && let Ok(bytes) = encoded {
            shared.cache.insert_jpeg(request.id, Arc::new(bytes));
        }
    }
}

fn run_job(shared: &Shared, id: JobId, action: Action, token: &CancelToken) {
    let (index, tier) = id;
    match action {
        Action::Metadata => run_metadata(shared, index, token),
        Action::Thumb => run_thumb(shared, index),
        Action::Rehydrate => run_rehydrate(shared, index, tier, token),
        Action::Develop(quality) => run_develop(shared, index, tier, quality, token, false),
        Action::WarmDevelop(quality) => run_warm_develop(shared, index, tier, quality, token),
    }
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

/// Probe background cache state away from the navigation/UI thread. A warm
/// hit is a no-op; a miss follows the same develop-and-persist path as before.
fn run_warm_develop(
    shared: &Shared,
    index: usize,
    tier: Tier,
    quality: Quality,
    token: &CancelToken,
) {
    if token.cancelled() || shared.cache.has_jpeg((index, tier)) {
        return;
    }
    let Some(disk) = &shared.disk else {
        return;
    };
    if disk.has(&DiskCache::key(&shared.entries[index], tier)) {
        return;
    }
    run_develop(shared, index, tier, quality, token, true);
}

fn run_thumb(shared: &Shared, index: usize) {
    match decode::thumb_and_meta(&shared.entries[index].path, 360) {
        Ok(result) => {
            shared
                .cache
                .insert_rgba((index, Tier::Thumb), Arc::new(result.thumb));
            publish(
                shared,
                Event::ThumbReady {
                    index,
                    meta: Box::new(result.meta),
                },
            );
        }
        Err(e) => publish(
            shared,
            Event::ImageFailed {
                index,
                tier: Tier::Thumb,
                error: e.to_string(),
            },
        ),
    }
}

fn run_develop(
    shared: &Shared,
    index: usize,
    tier: Tier,
    quality: Quality,
    token: &CancelToken,
    warm_only: bool,
) {
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
        return;
    }
    let decoded = match decode::load(path) {
        Ok(d) => d,
        Err(e) => return fail(e.to_string()),
    };
    let meta = FileMeta::from_metadata(&decoded.metadata);
    if token.cancelled() {
        return;
    }
    let (buf, _) = match develop(decoded.raw, quality) {
        Ok(r) => r,
        Err(e) => return fail(e.to_string()),
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
        let _ = shared.persistence.try_enqueue(PersistenceRequest {
            id: (index, tier),
            pixels: buf,
            insert_ram: !warm_only,
        });
    }
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

    fn persistence_request(
        id: JobId,
        pixels: Arc<PixelBuf>,
        insert_ram: bool,
    ) -> PersistenceRequest {
        PersistenceRequest {
            id,
            pixels,
            insert_ram,
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
            sequence: Mutex::new(Vec::new()),
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
    fn pops_in_priority_order() {
        let q = JobQueue::new();
        q.set_plan(vec![
            job((2, Tier::Browse), 4, 6),
            job((0, Tier::Browse), 0, 0),
            job((1, Tier::Browse), 2, 1),
        ]);
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

        q.set_plan(plan);

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
        q.set_plan(vec![
            job((9, Tier::Browse), 4, 3), // backward dist 1 → eff 3
            job((11, Tier::Browse), 4, 1),
            job((12, Tier::Browse), 4, 2),
        ]);
        assert_eq!(q.pop().unwrap().0.0, 11);
        assert_eq!(q.pop().unwrap().0.0, 12);
        assert_eq!(q.pop().unwrap().0.0, 9);
    }

    #[test]
    fn replan_supersedes_queued_jobs() {
        let q = JobQueue::new();
        q.set_plan(vec![
            job((0, Tier::Browse), 1, 0),
            job((1, Tier::Browse), 2, 0),
        ]);
        // New plan drops job 0 entirely.
        q.set_plan(vec![job((1, Tier::Browse), 1, 0)]);
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
        q.set_plan(vec![
            job((0, Tier::Browse), 1, 0),
            job((1, Tier::Browse), 2, 0),
        ]);
        let (id0, _, token0) = q.pop().unwrap();
        assert_eq!(id0, (0, Tier::Browse));
        // Job 0 is now in flight. Replan wants only job 1 → 0 cancelled.
        q.set_plan(vec![job((1, Tier::Browse), 1, 0)]);
        assert!(token0.cancelled());

        // Re-wanting job 0 while its cancelled instance is still in flight:
        // a fresh instance must be queued (the dying one won't publish).
        q.set_plan(vec![job((0, Tier::Browse), 1, 0)]);
        assert!(
            q.state
                .lock()
                .unwrap()
                .queued
                .contains_key(&(0, Tier::Browse))
        );

        // But a LIVE in-flight job is never duplicated.
        let q2 = JobQueue::new();
        q2.set_plan(vec![job((7, Tier::Browse), 1, 0)]);
        let _live = q2.pop().unwrap();
        q2.set_plan(vec![job((7, Tier::Browse), 1, 0)]);
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
        q.set_plan(vec![(id, 6, 20, Action::WarmDevelop(Quality::Browse))]);
        let (_, action, warm_token) = q.pop().unwrap();
        assert_eq!(action, Action::WarmDevelop(Quality::Browse));

        q.set_plan(vec![(id, 0, 0, Action::Develop(Quality::Browse))]);
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

        q.set_plan(vec![job((0, Tier::Browse), 0, 0)]);
        assert!(q.pop().is_none());
    }

    #[test]
    fn stale_completion_does_not_finish_replacement_generation() {
        let q = JobQueue::new();
        let id = (0, Tier::Browse);

        q.set_plan(vec![job(id, 1, 0)]);
        let (_, _, stale) = q.pop().unwrap();
        q.set_plan(Vec::new());
        assert!(stale.cancelled());

        q.set_plan(vec![job(id, 1, 0)]);
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
        q.set_plan(vec![
            job((0, Tier::Browse), 1, 1),
            job((1, Tier::Browse), 1, 1),
            job((2, Tier::Browse), 1, 1),
        ]);
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
            sequence: Mutex::new(Vec::new()),
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
        assert!(queue.finish(id));
        assert_eq!(queue.pop().unwrap().id, pending_id);
        assert!(queue.finish(pending_id));

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
            gc_worker: None,
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

        shared.heavy.set_plan(vec![job((0, Tier::Browse), 1, 0)]);
        let (_, _, token) = shared.heavy.pop().unwrap();
        assert_eq!(
            shared.persistence.try_enqueue(persistence_request(
                (0, Tier::Browse),
                Arc::new(patterned_buf(64, 48)),
                true,
            )),
            PersistenceEnqueue::Queued
        );

        shared.heavy.set_plan(Vec::new());
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
            sequence: Mutex::new(Vec::new()),
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
