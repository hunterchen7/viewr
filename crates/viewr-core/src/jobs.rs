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

use std::collections::{BinaryHeap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};

use crate::cache_ram::RamCache;
use crate::decode;
use crate::develop::{Quality, develop};
use crate::folder::{FolderEntry, outward_order};
use crate::meta::FileMeta;
use crate::resize::apply_orient;
use crate::types::{PixelBuf, Tier};

pub type JobId = (usize, Tier);

const HEAVY_WORKERS: usize = 3;
const LIGHT_WORKERS: usize = 2;
/// Browse-tier prefetch window in effective distance.
const BROWSE_WINDOW: u32 = 24;
/// Full-tier pre-warm window in effective distance.
const FULL_WINDOW: u32 = 2;
const JPEG_QUALITY_BROWSE: u8 = 87;
const JPEG_QUALITY_FULL: u8 = 90;

#[derive(Debug)]
pub enum Event {
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

#[derive(Debug, Clone, Copy)]
enum Action {
    Thumb,
    Develop(Quality),
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

#[derive(Default)]
struct QueueState {
    heap: BinaryHeap<QueuedJob>,
    /// id → epoch of its live heap entry. Entries with a different epoch
    /// are inert and skipped on pop.
    queued: HashMap<JobId, u64>,
    in_flight: HashMap<JobId, Arc<CancelToken>>,
    epoch: u64,
    seq: u64,
}

struct JobQueue {
    state: Mutex<QueueState>,
    cond: Condvar,
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
        state.epoch += 1;
        let epoch = state.epoch;
        state.heap.clear();
        state.queued.clear();
        let wanted: std::collections::HashSet<JobId> = plan.iter().map(|(id, ..)| *id).collect();
        for (id, token) in &state.in_flight {
            if !wanted.contains(id) {
                token.cancel();
            }
        }
        let mut heap = std::mem::take(&mut state.heap);
        for (id, class, eff_dist, action) in plan {
            // Skip only if a LIVE instance is already running; a cancelled
            // in-flight instance won't publish, so queue a fresh one.
            if state.in_flight.get(&id).is_some_and(|t| !t.cancelled()) {
                continue;
            }
            state.seq += 1;
            let prio = Prio {
                class,
                eff_dist,
                seq: state.seq,
            };
            state.queued.insert(id, epoch);
            heap.push(QueuedJob {
                prio,
                epoch,
                id,
                action,
            });
        }
        state.heap = heap;
        drop(state);
        self.cond.notify_all();
    }

    /// Append jobs without disturbing the existing plan (used for the
    /// one-shot thumb wave).
    fn extend(&self, jobs: impl IntoIterator<Item = (JobId, u8, u32, Action)>) {
        let mut state = self.state.lock().unwrap();
        let epoch = state.epoch;
        for (id, class, eff_dist, action) in jobs {
            state.seq += 1;
            let prio = Prio {
                class,
                eff_dist,
                seq: state.seq,
            };
            state.queued.insert(id, epoch);
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

    /// Block until a valid job is available (or shutdown).
    fn pop(&self, shutdown: &AtomicBool) -> Option<(JobId, Action, Arc<CancelToken>)> {
        let mut state = self.state.lock().unwrap();
        loop {
            if shutdown.load(Ordering::Relaxed) {
                return None;
            }
            while let Some(job) = state.heap.pop() {
                if state.queued.get(&job.id) != Some(&job.epoch) {
                    continue; // superseded by a newer plan
                }
                state.queued.remove(&job.id);
                let token = Arc::new(CancelToken::default());
                state.in_flight.insert(job.id, token.clone());
                return Some((job.id, job.action, token));
            }
            state = self.cond.wait(state).unwrap();
        }
    }

    fn finish(&self, id: JobId) {
        self.state.lock().unwrap().in_flight.remove(&id);
    }
}

struct Shared {
    entries: Vec<FolderEntry>,
    cache: Arc<RamCache>,
    events: Sender<Event>,
    notify: Arc<dyn Fn() + Send + Sync>,
    heavy: JobQueue,
    light: JobQueue,
    shutdown: AtomicBool,
}

pub struct Engine {
    shared: Arc<Shared>,
}

impl Engine {
    /// Spawns the worker pool and queues the outward thumb wave.
    /// `notify` is called after every published result (the app passes
    /// `ctx.request_repaint`).
    pub fn new(
        entries: Vec<FolderEntry>,
        start: usize,
        cache: Arc<RamCache>,
        notify: Arc<dyn Fn() + Send + Sync>,
    ) -> (Self, Receiver<Event>) {
        let (events, rx) = std::sync::mpsc::channel();
        let shared = Arc::new(Shared {
            entries,
            cache,
            events,
            notify,
            heavy: JobQueue::new(),
            light: JobQueue::new(),
            shutdown: AtomicBool::new(false),
        });

        for _ in 0..HEAVY_WORKERS {
            let shared = shared.clone();
            std::thread::spawn(move || worker(&shared, false));
        }
        for _ in 0..LIGHT_WORKERS {
            let shared = shared.clone();
            std::thread::spawn(move || worker(&shared, true));
        }

        // One-shot thumb+metadata wave, outward from the start position.
        let len = shared.entries.len();
        shared.light.extend(
            outward_order(len, start)
                .into_iter()
                .enumerate()
                .map(|(dist, index)| ((index, Tier::Thumb), 5, dist as u32, Action::Thumb)),
        );

        (Self { shared }, rx)
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

        // Pin current ±1 across tiers.
        let mut pins: Vec<JobId> = Vec::new();
        for i in current.saturating_sub(1)..=(current + 1).min(len - 1) {
            pins.extend([(i, Tier::Thumb), (i, Tier::Browse), (i, Tier::Full)]);
        }
        cache.set_pins(pins);

        let mut plan: Vec<(JobId, u8, u32, Action)> = Vec::new();
        let mut want = |index: usize, tier: Tier, class: u8, eff: u32| {
            let id = (index, tier);
            if cache.has_rgba(id) {
                return;
            }
            let action = if cache.has_jpeg(id) {
                Action::Rehydrate
            } else {
                Action::Develop(match tier {
                    Tier::Full => Quality::Full,
                    _ => Quality::Browse,
                })
            };
            plan.push((id, class, eff, action));
        };

        // Current image first.
        want(current, Tier::Browse, 0, 0);
        want(current, Tier::Full, if nav.zoomed { 0 } else { 1 }, 0);

        // Outward wave with 3:1 forward bias.
        for index in 0..len {
            if index == current {
                continue;
            }
            let ahead = (index > current) == (nav.direction >= 0);
            let dist = index.abs_diff(current) as u32;
            let eff = if ahead { dist } else { dist.saturating_mul(3) };
            if eff <= FULL_WINDOW {
                want(index, Tier::Browse, 2, eff);
                want(index, Tier::Full, 3, eff);
            } else if eff <= BROWSE_WINDOW {
                want(index, Tier::Browse, 4, eff);
            }
        }

        self.shared.heavy.set_plan(plan);
    }

    pub fn cache(&self) -> &Arc<RamCache> {
        &self.shared.cache
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.shared.shutdown.store(true, Ordering::Relaxed);
        self.shared.heavy.cond.notify_all();
        self.shared.light.cond.notify_all();
    }
}

fn worker(shared: &Shared, light: bool) {
    let queue = if light { &shared.light } else { &shared.heavy };
    while let Some((id, action, token)) = queue.pop(&shared.shutdown) {
        run_job(shared, id, action, &token);
        queue.finish(id);
    }
}

fn publish(shared: &Shared, event: Event) {
    let _ = shared.events.send(event);
    (shared.notify)();
}

fn run_job(shared: &Shared, id: JobId, action: Action, token: &CancelToken) {
    let (index, tier) = id;
    match action {
        Action::Thumb => run_thumb(shared, index),
        Action::Rehydrate => run_rehydrate(shared, index, tier, token),
        Action::Develop(quality) => run_develop(shared, index, tier, quality, token),
    }
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

fn run_develop(shared: &Shared, index: usize, tier: Tier, quality: Quality, token: &CancelToken) {
    let path = &shared.entries[index].path;
    let fail = |e: String| {
        publish(
            shared,
            Event::ImageFailed {
                index,
                tier,
                error: e,
            },
        );
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
    shared.cache.insert_rgba((index, tier), buf.clone());
    if !token.cancelled() {
        publish(shared, Event::ImageReady { index, tier });
    }

    // Ring-2 insurance: encode the developed result so future evictions
    // demote instead of discarding. Skipped when already cancelled.
    if !token.cancelled() && !shared.cache.has_jpeg((index, tier)) {
        let quality_jpeg = match tier {
            Tier::Full => JPEG_QUALITY_FULL,
            _ => JPEG_QUALITY_BROWSE,
        };
        if let Ok(bytes) = encode_jpeg(&buf, quality_jpeg) {
            shared.cache.insert_jpeg((index, tier), Arc::new(bytes));
        }
    }
}

fn run_rehydrate(shared: &Shared, index: usize, tier: Tier, token: &CancelToken) {
    let Some(bytes) = shared.cache.get_jpeg((index, tier)) else {
        // JPEG evicted between planning and execution — fall back.
        let quality = match tier {
            Tier::Full => Quality::Full,
            _ => Quality::Browse,
        };
        return run_develop(shared, index, tier, quality, token);
    };
    if token.cancelled() {
        return;
    }
    match decode_jpeg(&bytes) {
        Ok(buf) => {
            shared.cache.insert_rgba((index, tier), Arc::new(buf));
            if !token.cancelled() {
                publish(shared, Event::ImageReady { index, tier });
            }
        }
        Err(e) => publish(
            shared,
            Event::ImageFailed {
                index,
                tier,
                error: e,
            },
        ),
    }
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

    fn job(id: JobId, class: u8, dist: u32) -> (JobId, u8, u32, Action) {
        (id, class, dist, Action::Thumb)
    }

    #[test]
    fn pops_in_priority_order() {
        let q = JobQueue::new();
        let shutdown = AtomicBool::new(false);
        q.set_plan(vec![
            job((2, Tier::Browse), 4, 6),
            job((0, Tier::Browse), 0, 0),
            job((1, Tier::Browse), 2, 1),
        ]);
        assert_eq!(q.pop(&shutdown).unwrap().0, (0, Tier::Browse));
        assert_eq!(q.pop(&shutdown).unwrap().0, (1, Tier::Browse));
        assert_eq!(q.pop(&shutdown).unwrap().0, (2, Tier::Browse));
    }

    #[test]
    fn forward_bias_orders_wave() {
        // Same class: eff_dist decides; backward×3 means +1,+2,+3 beat -1.
        let q = JobQueue::new();
        let shutdown = AtomicBool::new(false);
        q.set_plan(vec![
            job((9, Tier::Browse), 4, 3), // backward dist 1 → eff 3
            job((11, Tier::Browse), 4, 1),
            job((12, Tier::Browse), 4, 2),
        ]);
        assert_eq!(q.pop(&shutdown).unwrap().0.0, 11);
        assert_eq!(q.pop(&shutdown).unwrap().0.0, 12);
        assert_eq!(q.pop(&shutdown).unwrap().0.0, 9);
    }

    #[test]
    fn replan_supersedes_queued_jobs() {
        let q = JobQueue::new();
        let shutdown = AtomicBool::new(false);
        q.set_plan(vec![
            job((0, Tier::Browse), 1, 0),
            job((1, Tier::Browse), 2, 0),
        ]);
        // New plan drops job 0 entirely.
        q.set_plan(vec![job((1, Tier::Browse), 1, 0)]);
        assert_eq!(q.pop(&shutdown).unwrap().0, (1, Tier::Browse));
        assert!(
            q.state.lock().unwrap().heap.is_empty() || {
                // Any remaining entries must be inert (stale epoch).
                let state = q.state.lock().unwrap();
                state.queued.is_empty()
            }
        );
    }

    #[test]
    fn replan_cancels_unwanted_in_flight_and_keeps_wanted() {
        let q = JobQueue::new();
        let shutdown = AtomicBool::new(false);
        q.set_plan(vec![
            job((0, Tier::Browse), 1, 0),
            job((1, Tier::Browse), 2, 0),
        ]);
        let (id0, _, token0) = q.pop(&shutdown).unwrap();
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
        let _live = q2.pop(&shutdown).unwrap();
        q2.set_plan(vec![job((7, Tier::Browse), 1, 0)]);
        assert!(
            !q2.state
                .lock()
                .unwrap()
                .queued
                .contains_key(&(7, Tier::Browse))
        );
    }
}
