//! The app shell: draws from the RamCache, dispatches navigation to the
//! engine, and owns every texture (workers never touch UI types).
//!
//! Modes: Loupe (filmstrip + big image) and Grid. A min-rating filter
//! produces the visible sequence; the engine's prefetch wave follows it.
//! Refiltering applies lazily on navigation so rating an image below the
//! threshold doesn't yank it out from under the cursor.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use eframe::egui::{self, vec2};
use viewr_core::cache_disk::DiskCache;
use viewr_core::cache_ram::RamCache;
use viewr_core::db::{Db, default_db_path};
use viewr_core::folder::{FolderEntry, normalize_physical_path, scan};
use viewr_core::jobs::{Engine, EngineOptions, Event, NavState};
use viewr_core::library::{
    Library, RatingLoad, load_ratings_with_owners, rating_owner_keys, try_load_ratings_with_owners,
};
use viewr_core::meta::FileMeta;
use viewr_core::types::{PixelBuf, Tier};

use crate::config::{Action, Config, ScrollMode, TierIndicator};
use crate::filmstrip;
use crate::loupe::{self, LoupeResponse, Zoom};
use crate::progressive_texture::{self, TileCoord};
use crate::rating_groups::{build_owner_members, install_rating_for_members};
use crate::settings::SettingsState;
use crate::texture_lru::ByteLru;
use crate::update::UpdateManager;

const THUMB_BUDGET: u64 = 384 * 1024 * 1024;
/// Logical RGBA bytes retained by thumbnail texture handles. Actual backend
/// allocation can be slightly higher, but remains proportional to this cap.
const THUMB_TEXTURE_BUDGET_BYTES: usize = 256 * 1024 * 1024;
const THUMB_UPLOADS_PER_FRAME: usize = 8;
const VISIBLE_FULL_TILE_UPLOADS_PER_FRAME: usize = 4;
const BACKGROUND_FULL_TILE_UPLOADS_PER_FRAME: usize = 1;
const THUMB_REQUEST_POLL_AFTER: Duration = Duration::from_millis(16);
const THUMB_REQUEST_STALE_AFTER: Duration = Duration::from_millis(500);
const THUMB_FAILURE_RETRY_AFTER: Duration = Duration::from_secs(2);
const RATING_DB_REFRESH_POLL: Duration = Duration::from_millis(50);
const RATING_DB_REFRESH_MAX_POLL: Duration = Duration::from_secs(5);

type RatingRefreshResult = std::result::Result<RatingLoad, String>;

fn ram_cache_budgets(total: u64) -> (u64, u64, u64) {
    let thumbs = THUMB_BUDGET.min(total / 2);
    let developed = total - thumbs;
    let rgba = developed / 3 * 2 + (developed % 3) * 2 / 3;
    let jpeg = developed - rgba;
    (thumbs, rgba, jpeg)
}

pub fn run(dir: &Path, select: Option<&Path>) -> Result<()> {
    let options = native_options();
    let dir = dir.to_owned();
    let select = select.map(Path::to_owned);
    eframe::run_native(
        "viewr",
        options,
        Box::new(move |cc| {
            crate::color::pin_srgb_colorspace(cc);
            let mut app = App::empty(cc);
            app.open_folder(&dir, select.as_deref())
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;
            Ok(Box::new(app))
        }),
    )
    .map_err(|e| anyhow!("eframe: {e}"))
}

fn native_options() -> eframe::NativeOptions {
    eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_app_id("viewr")
            .with_inner_size([1500.0, 950.0]),
        persist_window: true,
        ..Default::default()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Loupe,
    Grid,
}

#[derive(Default)]
enum Status {
    #[default]
    Empty,
    Performance(String),
    Error(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CacheState {
    Full,
    Browse,
    Compressed,
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
struct Filter {
    /// 0 = off; N = show only rating ≥ N.
    min_rating: u8,
    unrated_only: bool,
}

impl Filter {
    fn passes(&self, rating: u8) -> bool {
        if self.unrated_only {
            return rating == 0;
        }
        rating >= self.min_rating
    }
    fn active(&self) -> bool {
        self.min_rating > 0 || self.unrated_only
    }
}

/// Rebuild `visible` in source order. Returns whether that order is the
/// identity sequence understood by the engine.
fn rebuild_visible(
    visible: &mut Vec<usize>,
    len: usize,
    ratings: &HashMap<usize, u8>,
    filter: Filter,
) -> bool {
    visible.clear();
    if filter.active() {
        visible.extend((0..len).filter(|i| filter.passes(ratings.get(i).copied().unwrap_or(0))));
    }
    if visible.is_empty() {
        visible.extend(0..len);
    }
    visible.len() == len
}

/// Position of `current` in a sorted visible sequence, or the next visible
/// item when the current one has just been filtered out.
fn visible_position(visible: &[usize], current: usize) -> usize {
    match visible.binary_search(&current) {
        Ok(pos) => pos,
        Err(pos) => pos.min(visible.len().saturating_sub(1)),
    }
}

/// Cache positively rated positions in the current sorted filmstrip.
///
/// Sparse maps use binary-search intersection; dense maps scan the visible
/// sequence once. Rebuilding happens only when ratings or visibility change.
fn rebuild_starred_visible_positions(
    positions: &mut Vec<usize>,
    visible: &[usize],
    ratings: &HashMap<usize, u8>,
) {
    positions.clear();
    let binary_search_cost = visible.len().max(1).ilog2() as usize + 1;
    let ratings_are_sparse = ratings.len() <= visible.len() / binary_search_cost;
    if ratings_are_sparse {
        positions.extend(ratings.iter().filter_map(|(&index, &rating)| {
            (rating > 0)
                .then(|| visible.binary_search(&index).ok())
                .flatten()
        }));
        positions.sort_unstable();
    } else {
        positions.extend(visible.iter().enumerate().filter_map(|(position, index)| {
            (ratings.get(index).copied().unwrap_or(0) > 0).then_some(position)
        }));
    }
}

fn current_texture_candidates(current: usize) -> [(usize, Tier); 1] {
    [(current, Tier::Browse)]
}

fn main_texture_should_be_kept(index: usize, tier: Tier, near: &[usize]) -> bool {
    match tier {
        Tier::Full => false,
        _ => near.contains(&index),
    }
}

fn install_metadata(
    ratings: &mut HashMap<usize, u8>,
    metas: &mut HashMap<usize, FileMeta>,
    index: usize,
    meta: FileMeta,
    filter: Filter,
    accept_rating: bool,
) -> bool {
    let mut filter_dirty = false;
    if accept_rating
        && let Some(embedded) = meta.rating
        && embedded > 0
        && !ratings.contains_key(&index)
    {
        let rating = embedded.min(5) as u8;
        ratings.insert(index, rating);
        filter_dirty = filter.passes(0) != filter.passes(rating);
    }
    metas.insert(index, meta);
    filter_dirty
}

fn merge_refreshed_ratings(
    mut persisted: HashMap<usize, u8>,
    metas: &HashMap<usize, FileMeta>,
    explicit: &HashMap<usize, u8>,
) -> HashMap<usize, u8> {
    for (&index, meta) in metas {
        if let Some(rating) = meta.rating
            && rating > 0
        {
            persisted.entry(index).or_insert(rating.min(5) as u8);
        }
    }
    persisted.extend(explicit.iter().map(|(&index, &rating)| (index, rating)));
    persisted
}

fn rating_refresh_required(database_configured: bool) -> bool {
    // Even a current-schema snapshot can race the persistence worker's
    // initial recovery pass. Reconcile once after its readiness boundary so a
    // replaced or retargeted RAW cannot leave a stale rating in this session.
    database_configured
}

fn rating_sources_blocked(database_configured: bool, database_snapshot_available: bool) -> bool {
    database_configured && !database_snapshot_available
}

fn next_rating_refresh_delay(delay: Duration) -> Duration {
    delay.saturating_mul(2).min(RATING_DB_REFRESH_MAX_POLL)
}

/// Store every explicit user choice, including zero. Absence means that no
/// higher-precedence rating source has been observed yet, so removing a zero
/// would let a delayed embedded-metadata event resurrect the camera rating.
#[cfg(test)]
fn install_user_rating(ratings: &mut HashMap<usize, u8>, index: usize, rating: u8) -> u8 {
    let old_rating = ratings.get(&index).copied().unwrap_or(0);
    ratings.insert(index, rating);
    old_rating
}

/// Per-folder session state, rebuilt by open_folder.
struct Session {
    dir: PathBuf,
    entries: Arc<Vec<FolderEntry>>,
    engine: Engine,
    events: Receiver<Event>,
    cache: Arc<RamCache>,
    library: Library,
    ratings: HashMap<usize, u8>,
    explicit_ratings: HashMap<usize, u8>,
    rating_members: Vec<Option<Arc<[usize]>>>,
    rating_refresh_pending: bool,
    rating_sources_blocked: bool,
    rating_refresh_after: Option<Instant>,
    rating_refresh_delay: Duration,
    rating_refresh_rx: Option<Receiver<RatingRefreshResult>>,
    metas: HashMap<usize, FileMeta>,
    thumbs: ByteLru<egui::TextureHandle>,
    /// Demand requests are bounded to the current viewport and time out so a
    /// publish/worker-finish race cannot leave an evicted thumbnail stuck.
    thumb_requests: HashMap<usize, Instant>,
    thumb_retry_after: HashMap<usize, Instant>,
    textures: HashMap<(usize, Tier), egui::TextureHandle>,
    /// Full-resolution texture tiles exist only for the current zoomed image.
    /// Browse stays underneath them so incomplete regions remain usable.
    full_tiles: HashMap<(usize, TileCoord), egui::TextureHandle>,
}

pub struct App {
    ctx: egui::Context,
    config: Config,
    settings: SettingsState,
    updates: UpdateManager,
    session: Option<Session>,

    current: usize,
    direction: i8,
    zoom: Zoom,
    mode: Mode,
    filter: Filter,
    visible: Vec<usize>,
    /// Ratings only affect the visible sequence lazily, on navigation.
    filter_dirty: bool,
    fullscreen: bool,
    show_metadata: bool,

    nav_started: Option<Instant>,
    /// Last known full-res logical size; carries the zoom framing onto
    /// thumb placeholders before the new image's develop lands.
    last_logical: Option<egui::Vec2>,
    status: Status,
    scroll_to_current: bool,
    starred_visible_positions: Vec<usize>,
    star_markers_dirty: bool,
    star_marker_revision: u64,
    star_marker_cache: filmstrip::StarMarkerCache,
}

impl App {
    fn empty(cc: &eframe::CreationContext<'_>) -> Self {
        let ctx = cc.egui_ctx.clone();
        let config = Config::load();
        let updates = UpdateManager::new(ctx.clone());
        Self {
            ctx,
            config,
            settings: SettingsState::default(),
            updates,
            session: None,
            current: 0,
            direction: 1,
            zoom: Zoom::Fit,
            mode: Mode::Loupe,
            filter: Filter::default(),
            visible: Vec::new(),
            filter_dirty: true,
            fullscreen: false,
            show_metadata: false,
            nav_started: None,
            last_logical: None,
            status: Status::Empty,
            scroll_to_current: true,
            starred_visible_positions: Vec::new(),
            star_markers_dirty: true,
            star_marker_revision: 0,
            star_marker_cache: filmstrip::StarMarkerCache::default(),
        }
    }

    fn open_folder(&mut self, dir: &Path, select: Option<&Path>) -> Result<()> {
        let entries = scan(dir)?;
        if entries.is_empty() {
            return Err(anyhow!("no raw files found in {}", dir.display()));
        }
        let selected = select.map(normalize_physical_path);
        let start = selected
            .as_deref()
            .and_then(|file| entries.iter().position(|entry| entry.path == file))
            .unwrap_or(0);
        let entries = Arc::new(entries);
        let ram_bytes = (self.config.ram_gb as f64 * 1e9) as u64;
        let (thumb_bytes, rgba_bytes, jpeg_bytes) = ram_cache_budgets(ram_bytes);
        let cache = Arc::new(RamCache::new(thumb_bytes, rgba_bytes, jpeg_bytes));
        let disk = DiskCache::open_default((self.config.disk_gb as f64 * 1e9) as u64);
        let rating_ctx = self.ctx.clone();
        let library = Library::start_with_database_ready_notify(Arc::new(move || {
            rating_ctx.request_repaint();
        }));
        // Resolve persisted ratings before decode workers can publish embedded
        // metadata. Legacy compatible schemas stay read-only here; if a
        // migration or repair is required, the persistence worker performs it
        // and the session refreshes this snapshot without blocking the UI.
        let db = default_db_path()
            .and_then(|path| Db::try_open_for_read(&path).ok())
            .flatten();
        let database_configured = library.database_configured();
        let (ratings, rating_owners, database_snapshot_available) = match db.as_ref() {
            Some(db) => match try_load_ratings_with_owners(&entries, Some(db)) {
                Ok((ratings, owners)) => (ratings, owners, true),
                Err(error) => {
                    eprintln!("initial rating database read failed; scheduling retry: {error}");
                    (HashMap::new(), rating_owner_keys(&entries), false)
                }
            },
            None if database_configured => (HashMap::new(), rating_owner_keys(&entries), false),
            None => {
                let (ratings, owners) = load_ratings_with_owners(&entries, None);
                (ratings, owners, false)
            }
        };
        let rating_refresh_pending = rating_refresh_required(database_configured);
        let rating_sources_blocked =
            rating_sources_blocked(database_configured, database_snapshot_available);
        let rating_members = build_owner_members(&rating_owners);

        let ctx = self.ctx.clone();
        let notify: Arc<dyn Fn() + Send + Sync> = Arc::new(move || ctx.request_repaint());
        let engine_options = EngineOptions::default().with_jpeg_quality(self.config.jpeg_quality);
        let engine_options = if let Some(worker_threads) = self.config.fixed_worker_threads() {
            engine_options.with_worker_threads(worker_threads)
        } else {
            engine_options
        };
        // Do not let folder replacement briefly run two processing pools.
        // All recoverable preparation above succeeds before the current
        // session is released; engine construction has no recoverable error
        // path and documents process-level spawn failures as panics.
        drop(self.session.take());
        let (engine, events) = Engine::new_with_options(
            entries.clone(),
            start,
            cache.clone(),
            disk,
            engine_options,
            notify,
        );

        self.session = Some(Session {
            dir: dir.to_owned(),
            entries,
            engine,
            events,
            cache,
            library,
            ratings,
            explicit_ratings: HashMap::new(),
            rating_members,
            rating_refresh_pending,
            rating_sources_blocked,
            rating_refresh_after: None,
            rating_refresh_delay: RATING_DB_REFRESH_POLL,
            rating_refresh_rx: None,
            metas: HashMap::new(),
            thumbs: ByteLru::new(THUMB_TEXTURE_BUDGET_BYTES),
            thumb_requests: HashMap::new(),
            thumb_retry_after: HashMap::new(),
            textures: HashMap::new(),
            full_tiles: HashMap::new(),
        });
        self.current = start;
        self.direction = 1;
        self.zoom = Zoom::Fit;
        self.filter = Filter::default();
        self.filter_dirty = true;
        self.nav_started = Some(Instant::now());
        self.scroll_to_current = true;
        self.starred_visible_positions.clear();
        self.star_markers_dirty = true;
        self.star_marker_cache = filmstrip::StarMarkerCache::default();
        self.apply_filter();
        self.replan();
        Ok(())
    }

    fn pick_folder(&mut self) {
        let start_dir = self.session.as_ref().map(|s| s.dir.clone());
        let mut dialog = rfd::FileDialog::new();
        if let Some(d) = start_dir {
            dialog = dialog.set_directory(d);
        }
        if let Some(dir) = dialog.pick_folder()
            && let Err(e) = self.open_folder(&dir, None)
        {
            self.status = Status::Error(format!("open failed: {e}"));
        }
    }

    /// Rebuild the visible sequence from the filter. Falls back to "all"
    /// when nothing matches so navigation never dead-ends.
    fn apply_filter(&mut self) {
        if !self.filter_dirty {
            return;
        }
        let Some(session) = &self.session else {
            return;
        };
        let identity = rebuild_visible(
            &mut self.visible,
            session.entries.len(),
            &session.ratings,
            self.filter,
        );
        // The engine treats an empty sequence as the identity order, avoiding
        // another full-size allocation for the common unfiltered case.
        let engine_sequence = if identity {
            Vec::new()
        } else {
            self.visible.clone()
        };
        session.engine.set_sequence(engine_sequence);
        self.filter_dirty = false;
        self.star_markers_dirty = true;
    }

    fn replan(&self) {
        if let Some(session) = &self.session {
            session.engine.navigate(NavState {
                current: self.current,
                direction: self.direction,
                zoomed: !matches!(self.zoom, Zoom::Fit),
            });
        }
    }

    /// Position of `current` within the visible sequence (nearest if the
    /// current image was filtered out).
    fn visible_pos(&self) -> usize {
        visible_position(&self.visible, self.current)
    }

    fn refresh_star_marker_positions(&mut self) {
        if !self.star_markers_dirty {
            return;
        }
        if let Some(session) = &self.session {
            rebuild_starred_visible_positions(
                &mut self.starred_visible_positions,
                &self.visible,
                &session.ratings,
            );
        } else {
            self.starred_visible_positions.clear();
        }
        self.star_marker_revision = self.star_marker_revision.wrapping_add(1);
        self.star_markers_dirty = false;
    }

    fn navigate(&mut self, delta: isize) {
        self.apply_filter(); // lazy refilter happens at nav time
        if self.visible.is_empty() {
            return;
        }
        let pos = self.visible_pos() as isize;
        let on_visible = self.visible.get(pos as usize) == Some(&self.current);
        // Stepping off a filtered-out image: land on the neighbor itself.
        let adjust = if !on_visible && delta > 0 { -1 } else { 0 };
        let next = (pos + delta + adjust).clamp(0, self.visible.len() as isize - 1) as usize;
        if delta != 0 {
            self.direction = if delta < 0 { -1 } else { 1 };
        }
        self.select(self.visible[next]);
    }

    fn select(&mut self, index: usize) {
        if index == self.current {
            return;
        }
        self.direction = if index < self.current { -1 } else { 1 };
        self.current = index;
        // Zoom state is intentionally retained: culling a burst at 100%
        // keeps the same framing (eye, detail) across images.
        self.scroll_to_current = true;
        self.nav_started = Some(Instant::now());
        if let Some(s) = &self.session {
            s.library.request_flush();
        }
        self.replan();
    }

    fn set_rating(&mut self, rating: u8) {
        let index = self.current;
        let Some(session) = &mut self.session else {
            return;
        };
        let singleton = [index];
        let members = session.rating_members[index]
            .as_deref()
            .unwrap_or(&singleton);
        let filter_changed = install_rating_for_members(
            &mut session.ratings,
            members,
            rating,
            |old_rating, new_rating| {
                self.filter.passes(old_rating) != self.filter.passes(new_rating)
            },
        );
        for &member in members {
            session.explicit_ratings.insert(member, rating);
        }
        session.library.set_rating(&session.entries[index], rating);
        self.star_markers_dirty = true;
        if filter_changed {
            self.filter_dirty = true;
        }
    }

    fn drain_events(&mut self) {
        let current = self.current;
        let Some(session) = &mut self.session else {
            return;
        };
        let mut replan = false;
        let mut filter_dirty = false;
        let mut star_markers_dirty = false;
        let accept_metadata_ratings = !session.rating_sources_blocked;
        while let Ok(event) = session.events.try_recv() {
            match event {
                Event::ThumbReady { index, meta } => {
                    // Pixels stay in the byte-bounded RAM ring until a visible
                    // viewport asks to upload them. If they were already evicted,
                    // the demand path below safely queues a replacement decode.
                    session.thumb_requests.remove(&index);
                    session.thumb_retry_after.remove(&index);
                    let old_rating = session.ratings.get(&index).copied().unwrap_or(0);
                    filter_dirty |= install_metadata(
                        &mut session.ratings,
                        &mut session.metas,
                        index,
                        *meta,
                        self.filter,
                        accept_metadata_ratings,
                    );
                    let new_rating = session.ratings.get(&index).copied().unwrap_or(0);
                    star_markers_dirty |= (old_rating > 0) != (new_rating > 0);
                }
                Event::MetadataReady { index, meta } => {
                    let old_rating = session.ratings.get(&index).copied().unwrap_or(0);
                    filter_dirty |= install_metadata(
                        &mut session.ratings,
                        &mut session.metas,
                        index,
                        *meta,
                        self.filter,
                        accept_metadata_ratings,
                    );
                    let new_rating = session.ratings.get(&index).copied().unwrap_or(0);
                    star_markers_dirty |= (old_rating > 0) != (new_rating > 0);
                }
                Event::ImageReady { .. } => replan = true,
                Event::ImageFailed { index, tier, error } => {
                    if tier == Tier::Thumb && session.thumb_requests.remove(&index).is_some() {
                        session
                            .thumb_retry_after
                            .insert(index, Instant::now() + THUMB_FAILURE_RETRY_AFTER);
                    } else if index == current && tier != Tier::Thumb {
                        self.status = Status::Error(format!("error: {error}"));
                    } else {
                        eprintln!("job failed {index}/{tier:?}: {error}");
                    }
                }
                Event::MetadataFailed { index, error } => {
                    eprintln!("metadata failed {index}: {error}");
                }
            }
        }
        self.filter_dirty |= filter_dirty;
        self.star_markers_dirty |= star_markers_dirty;
        if replan {
            self.replan();
        }
    }

    fn refresh_ratings_after_database_ready(&mut self) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        if !session.rating_refresh_pending {
            return;
        }

        if let Some(receiver) = session.rating_refresh_rx.as_ref() {
            let result = match receiver.try_recv() {
                Ok(result) => result,
                Err(std::sync::mpsc::TryRecvError::Empty) => return,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    Err("rating refresh worker disconnected".to_owned())
                }
            };
            let session = self.session.as_mut().expect("checked session");
            session.rating_refresh_rx = None;
            match result {
                Ok((ratings, owners)) => {
                    let ratings =
                        merge_refreshed_ratings(ratings, &session.metas, &session.explicit_ratings);
                    session.ratings = ratings;
                    session.rating_members = build_owner_members(&owners);
                    session.rating_refresh_pending = false;
                    session.rating_sources_blocked = false;
                    session.rating_refresh_after = None;
                    self.filter_dirty = true;
                    self.star_markers_dirty = true;
                }
                Err(error) => {
                    eprintln!("rating database refresh failed; scheduling retry: {error}");
                    self.defer_rating_database_refresh();
                }
            }
            return;
        }

        let session = self.session.as_ref().expect("checked session");
        if !session.library.database_ready() {
            return;
        }
        if session
            .rating_refresh_after
            .is_some_and(|retry_after| Instant::now() < retry_after)
        {
            return;
        }
        let entries = session.entries.clone();
        let ctx = self.ctx.clone();
        let (send, receive) = std::sync::mpsc::channel();
        let spawn = std::thread::Builder::new()
            .name("viewr-rating-refresh".to_owned())
            .spawn(move || {
                let result = (|| {
                    let path = default_db_path()
                        .ok_or_else(|| "configured database path is unavailable".to_owned())?;
                    let db = Db::try_open_for_read(&path)
                        .map_err(|error| error.to_string())?
                        .ok_or_else(|| "rating database migration is not complete".to_owned())?;
                    if !db.rating_schema_is_current() {
                        return Err("rating database migration is not complete".to_owned());
                    }
                    try_load_ratings_with_owners(&entries, Some(&db))
                        .map_err(|error| error.to_string())
                })();
                let _ = send.send(result);
                ctx.request_repaint();
            });
        match spawn {
            Ok(_worker) => {
                self.session
                    .as_mut()
                    .expect("checked session")
                    .rating_refresh_rx = Some(receive);
            }
            Err(error) => {
                eprintln!("cannot spawn rating refresh worker; scheduling retry: {error}");
                self.defer_rating_database_refresh();
            }
        }
    }

    fn defer_rating_database_refresh(&mut self) {
        let Some(session) = self.session.as_mut() else {
            return;
        };
        let delay = session.rating_refresh_delay;
        session.rating_refresh_after = Some(Instant::now() + delay);
        session.rating_refresh_delay = next_rating_refresh_delay(delay);
        self.ctx.request_repaint_after(delay);
    }

    /// Upload only thumbnails demanded by the current viewport. The byte LRU
    /// drops old `TextureHandle`s (and therefore their GPU allocations), while
    /// the upload budget prevents a newly opened grid from monopolizing a frame.
    fn manage_thumbnail_textures(&mut self, demanded_indices: &[usize]) {
        let ctx = self.ctx.clone();
        let now = Instant::now();
        let demanded: HashSet<usize> = demanded_indices.iter().copied().collect();
        let Some(session) = &mut self.session else {
            return;
        };

        // Scrolling must not leave bookkeeping proportional to folder size.
        session
            .thumb_requests
            .retain(|index, _| demanded.contains(index));
        session
            .thumb_retry_after
            .retain(|index, _| demanded.contains(index));

        let mut upload_budget = THUMB_UPLOADS_PER_FRAME;
        let mut uploaded = false;
        let mut next_wakeup = None;
        let mut requested_thumbs = Vec::with_capacity(demanded_indices.len());
        let mut seen = HashSet::with_capacity(demanded_indices.len());
        for &index in demanded_indices {
            if index >= session.entries.len() || !seen.insert(index) {
                continue;
            }
            if session.thumbs.touch(index) {
                continue;
            }

            if let Some(buf) = session.cache.get_rgba((index, Tier::Thumb)) {
                if upload_budget > 0 {
                    let bytes = buf.byte_len();
                    let texture = ctx.load_texture(
                        format!("thumb{index}"),
                        to_color_image(&buf),
                        egui::TextureOptions::LINEAR,
                    );
                    if session.thumbs.insert(index, texture, bytes) {
                        upload_budget -= 1;
                        uploaded = true;
                        session.thumb_requests.remove(&index);
                        session.thumb_retry_after.remove(&index);
                    }
                }
                // The pixels are available and will be uploaded in a later
                // frame if this frame's upload allowance is exhausted.
                continue;
            }

            if let Some(retry_after) = session.thumb_retry_after.get(&index)
                && now < *retry_after
            {
                let delay = retry_after.saturating_duration_since(now);
                next_wakeup =
                    Some(next_wakeup.map_or(delay, |current: Duration| current.min(delay)));
                continue;
            }
            session.thumb_retry_after.remove(&index);
            requested_thumbs.push(index);

            let request_is_stale = match session.thumb_requests.get(&index) {
                Some(requested) => {
                    let elapsed = now.duration_since(*requested);
                    if elapsed < THUMB_REQUEST_STALE_AFTER {
                        let delay = THUMB_REQUEST_STALE_AFTER - elapsed;
                        next_wakeup =
                            Some(next_wakeup.map_or(delay, |current: Duration| current.min(delay)));
                        false
                    } else {
                        true
                    }
                }
                None => true,
            };
            if request_is_stale {
                session.thumb_requests.insert(index, now);
                // Poll once to close the narrow race where pixels appeared
                // between our cache probe and the worker claiming demand.
                next_wakeup = Some(next_wakeup.map_or(THUMB_REQUEST_POLL_AFTER, |current| {
                    current.min(THUMB_REQUEST_POLL_AFTER)
                }));
            }
        }
        let _ = session.engine.set_thumbnail_demand(&requested_thumbs);

        if uploaded {
            // Uploads happen after the widgets are painted, so schedule the
            // next frame that will actually display the new texture handles.
            ctx.request_repaint();
        }
        if let Some(delay) = next_wakeup {
            // Failure backoff and stale-request recovery must make progress
            // even when no worker event or user input produces another frame.
            ctx.request_repaint_after(delay);
        }
    }

    /// Upload policy for untiled textures: current Browse first, then one
    /// neighbor Browse per frame. Full is uploaded progressively in loupe mode.
    fn manage_textures(&mut self) {
        let ctx = self.ctx.clone();
        let current = self.current;
        let zoom = self.zoom;
        // Keep-window over the visible sequence.
        let pos = self.visible_pos();
        let near: Vec<usize> = (-2isize..=2)
            .filter_map(|d| {
                let p = pos as isize + d;
                (p >= 0)
                    .then(|| self.visible.get(p as usize).copied())
                    .flatten()
            })
            .collect();
        let Some(session) = &mut self.session else {
            return;
        };
        session
            .textures
            .retain(|(i, tier), _| main_texture_should_be_kept(*i, *tier, &near));
        session
            .full_tiles
            .retain(|(index, _), _| *index == current && !matches!(zoom, Zoom::Fit));

        let mut upload = |key: (usize, Tier), budget: &mut i32| {
            if *budget <= 0 || session.textures.contains_key(&key) {
                return;
            }
            if let Some(buf) = session.cache.get_rgba(key) {
                let tex = ctx.load_texture(
                    format!("img{}-{:?}", key.0, key.1),
                    to_color_image(&buf),
                    egui::TextureOptions::LINEAR,
                );
                session.textures.insert(key, tex);
                *budget -= 1;
            }
        };
        let mut budget = 2;
        for key in current_texture_candidates(current) {
            upload(key, &mut budget);
        }
        let mut neighbor_budget = 1;
        for &i in near.iter().filter(|&&i| i != current) {
            upload((i, Tier::Browse), &mut neighbor_budget);
        }
    }

    /// Upload Full-resolution tiles under the current zoom rectangle first and
    /// paint them over the Browse stand-in. Once the visible rectangle is
    /// complete, one tile per frame expands the Full texture outward.
    fn progressive_full_overlay(&mut self, ui: &egui::Ui, response: &LoupeResponse) -> bool {
        if matches!(self.zoom, Zoom::Fit) {
            return false;
        }
        let current = self.current;
        let ctx = self.ctx.clone();
        let Some(buf) = self
            .session
            .as_ref()
            .and_then(|session| session.cache.get_rgba((current, Tier::Full)))
        else {
            return false;
        };
        let order = progressive_texture::priority_order(buf.width, buf.height, response.visible_uv);
        let visible_count = progressive_texture::visible_prefix_len(
            buf.width,
            buf.height,
            response.visible_uv,
            &order,
        );
        let Some(session) = &mut self.session else {
            return false;
        };
        let missing_visible = order[..visible_count]
            .iter()
            .filter(|&&tile| !session.full_tiles.contains_key(&(current, tile)))
            .count();
        let upload_budget = if missing_visible > 0 {
            VISIBLE_FULL_TILE_UPLOADS_PER_FRAME
        } else {
            BACKGROUND_FULL_TILE_UPLOADS_PER_FRAME
        };
        let mut uploaded = 0;
        let mut invalid_storage = false;
        for &tile in &order {
            if uploaded >= upload_budget {
                break;
            }
            let key = (current, tile);
            if session.full_tiles.contains_key(&key) {
                continue;
            }
            let Some(image) = progressive_texture::color_image(&buf, tile) else {
                invalid_storage = true;
                break;
            };
            let texture = ctx.load_texture(
                format!("img{current}-Full-{}-{}", tile.col, tile.row),
                image,
                egui::TextureOptions::LINEAR,
            );
            session.full_tiles.insert(key, texture);
            uploaded += 1;
        }

        let painter = ui.painter().with_clip_rect(response.viewport_rect);
        for &tile in &order[..visible_count] {
            let Some(texture) = session.full_tiles.get(&(current, tile)) else {
                continue;
            };
            let Some(geometry) = progressive_texture::paint_geometry(
                buf.width,
                buf.height,
                tile,
                response.visible_uv,
                response.image_draw_rect,
            ) else {
                continue;
            };
            painter.image(
                texture.id(),
                geometry.screen,
                geometry.texture_uv,
                egui::Color32::WHITE,
            );
        }

        let visible_complete = order[..visible_count]
            .iter()
            .all(|&tile| session.full_tiles.contains_key(&(current, tile)));
        if !invalid_storage && session.full_tiles.len() < order.len() {
            ctx.request_repaint();
        }
        visible_complete
    }

    fn handle_keys(&mut self, loupe_rect: egui::Rect, img_size: Option<egui::Vec2>) {
        if self.settings.capturing() || self.updates.blocks_app_input() {
            return; // a modal owns this frame's keystrokes
        }
        let ctx = self.ctx.clone();
        struct Keys {
            right: bool,
            left: bool,
            up: bool,
            down: bool,
            shift: bool,
            home: bool,
            end: bool,
            toggle_zoom: bool,
            grid: bool,
            info: bool,
            fullscreen: bool,
            open: bool,
            prefs: bool,
            rating: Option<u8>,
        }
        let config = &self.config;
        let in_grid = self.mode == Mode::Grid;
        let k = ctx.input(|i| Keys {
            right: config.pressed(i, Action::Next),
            left: config.pressed(i, Action::Prev),
            up: i.key_pressed(egui::Key::ArrowUp),
            down: i.key_pressed(egui::Key::ArrowDown),
            shift: i.modifiers.shift,
            home: config.pressed(i, Action::First),
            end: config.pressed(i, Action::Last),
            toggle_zoom: config.pressed(i, Action::ToggleZoom),
            grid: config.pressed(i, Action::Grid) || (in_grid && i.key_pressed(egui::Key::Enter)),
            info: config.pressed(i, Action::Metadata),
            fullscreen: config.pressed(i, Action::Fullscreen),
            open: config.pressed(i, Action::OpenFolder),
            prefs: config.pressed(i, Action::Preferences),
            rating: config.pressed_rating(i),
        });
        if k.prefs {
            self.settings.open = !self.settings.open;
        }

        if k.open {
            self.pick_folder();
            return;
        }
        if let Some(r) = k.rating {
            self.set_rating(r);
        }
        let step = if k.shift { 10 } else { 1 };
        let grid_cols = self.grid_cols() as isize;
        if k.right {
            self.navigate(step);
        }
        if k.left {
            self.navigate(-step);
        }
        if self.mode == Mode::Grid {
            if k.down {
                self.navigate(grid_cols);
            }
            if k.up {
                self.navigate(-grid_cols);
            }
        }
        if k.home {
            self.apply_filter();
            if let Some(&first) = self.visible.first() {
                self.select(first);
            }
        }
        if k.end {
            self.apply_filter();
            if let Some(&last) = self.visible.last() {
                self.select(last);
            }
        }
        if k.grid {
            self.mode = match self.mode {
                Mode::Loupe => Mode::Grid,
                Mode::Grid => Mode::Loupe,
            };
            self.scroll_to_current = true;
        }
        if k.info {
            self.show_metadata = !self.show_metadata;
        }
        if k.fullscreen {
            self.fullscreen = !self.fullscreen;
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.fullscreen));
        }
        if k.toggle_zoom
            && self.mode == Mode::Loupe
            && let Some(size) = img_size
        {
            let anchor = ctx
                .pointer_hover_pos()
                .filter(|p| loupe_rect.contains(*p))
                .unwrap_or_else(|| loupe_rect.center());
            loupe::toggle_100(&mut self.zoom, loupe_rect, size, anchor);
            self.replan();
        }
    }

    fn cache_state(session: &Session, index: usize) -> Option<CacheState> {
        let cache = &session.cache;
        if cache.has_rgba((index, Tier::Full)) {
            Some(CacheState::Full)
        } else if cache.has_rgba((index, Tier::Browse)) {
            Some(CacheState::Browse)
        } else if cache.has_jpeg((index, Tier::Browse)) || cache.has_jpeg((index, Tier::Full)) {
            Some(CacheState::Compressed)
        } else {
            None
        }
    }

    /// Cache-tier stroke color for a thumbnail when the border mode is active.
    fn tier_stroke(&self, session: &Session, index: usize) -> Option<egui::Color32> {
        if self.config.tier_indicator != TierIndicator::Border {
            return None;
        }
        Self::cache_state(session, index).map(cache_state_stroke_color)
    }

    /// Delivery-style cache state shown below thumbnails in the subtle mode.
    fn tier_mark(
        &self,
        session: &Session,
        index: usize,
    ) -> Option<(&'static str, egui::Color32, &'static str)> {
        if self.config.tier_indicator != TierIndicator::Marks {
            return None;
        }
        match Self::cache_state(session, index)? {
            CacheState::Full => Some((
                "✓✓",
                cache_state_color(CacheState::Full),
                "Full resolution ready",
            )),
            CacheState::Browse => Some((
                "✓",
                cache_state_color(CacheState::Browse),
                "Browse resolution ready",
            )),
            CacheState::Compressed => Some((
                "•",
                cache_state_color(CacheState::Compressed),
                "Compressed render cached",
            )),
        }
    }

    fn grid_cols(&self) -> usize {
        let width = self.ctx.content_rect().width();
        ((width / (self.config.grid_cell + 16.0)).floor() as usize).max(2)
    }

    fn rating_of(&self, index: usize) -> u8 {
        self.session
            .as_ref()
            .and_then(|s| s.ratings.get(&index).copied())
            .unwrap_or(0)
    }
}

fn stars(rating: u8) -> String {
    (0..5).map(|i| if i < rating { '★' } else { '☆' }).collect()
}

fn cache_state_color(state: CacheState) -> egui::Color32 {
    match state {
        CacheState::Full => egui::Color32::from_rgb(70, 200, 110),
        CacheState::Browse => egui::Color32::from_rgb(240, 180, 60),
        CacheState::Compressed => egui::Color32::from_rgb(90, 140, 240),
    }
}

fn cache_state_stroke_color(state: CacheState) -> egui::Color32 {
    match state {
        CacheState::Full => egui::Color32::from_rgba_unmultiplied(70, 200, 110, 150),
        CacheState::Browse => egui::Color32::from_rgba_unmultiplied(240, 180, 60, 150),
        CacheState::Compressed => egui::Color32::from_rgba_unmultiplied(90, 140, 240, 110),
    }
}

impl eframe::App for App {
    fn persist_egui_memory(&self) -> bool {
        // NativeOptions persists the root window. App preferences and panel
        // sizes use viewr.toml; transient widget state must not reopen dialogs
        // or restore stale scroll positions in a different folder.
        false
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = self.ctx.clone();
        self.updates.poll();
        if self.settings.open {
            self.updates.refresh_automatic_preference();
        }
        if !self.updates.blocks_app_input() {
            self.settings.maybe_capture(&ctx, &mut self.config);
        }
        let update_status = self.updates.status_text();
        let update_actions = self.settings.show(
            &ctx,
            &mut self.config,
            &update_status,
            self.updates.has_status_details(),
            self.updates.automatic_checks_enabled(),
        );
        if update_actions.check_for_updates {
            self.updates.check_now();
        }
        if update_actions.show_update_status {
            self.updates.open_status();
        }
        if let Some(enabled) = update_actions.automatic_updates_changed {
            self.updates.set_automatic_checks(enabled);
        }
        self.updates.show();
        self.refresh_ratings_after_database_ready();
        self.drain_events();
        self.manage_textures();
        if self.session.is_none() {
            ui.centered_and_justified(|u| {
                u.label("Open a folder of raws with Cmd+O");
            });
            self.handle_keys(ui.max_rect(), None);
            return;
        }

        self.top_bar(ui);
        if self.show_metadata {
            self.metadata_panel(ui);
        }
        match self.mode {
            Mode::Loupe => self.loupe_mode(ui),
            Mode::Grid => self.grid_mode(ui),
        }
    }
}

impl App {
    fn top_bar(&mut self, ui: &mut egui::Ui) {
        let mut filter_changed = false;
        let session_len = self.session.as_ref().map_or(0, |s| s.entries.len());
        let file_name = self
            .session
            .as_ref()
            .map(|s| s.entries[self.current].file_name.clone())
            .unwrap_or_default();
        egui::Panel::top("status").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(format!(
                    "{}/{}  {}",
                    self.visible_pos() + 1,
                    self.visible.len(),
                    file_name,
                ));
                if self.visible.len() != session_len {
                    ui.label(egui::RichText::new(format!("(of {session_len})")).weak());
                }
                ui.separator();

                // Filter dropdown (distinct from the rating display).
                let filter_label = |f: &Filter| -> String {
                    if f.unrated_only {
                        "unrated".into()
                    } else if f.min_rating == 0 {
                        "all".into()
                    } else {
                        format!("≥ {}", "★".repeat(f.min_rating as usize))
                    }
                };
                egui::ComboBox::from_id_salt("filter")
                    .selected_text(format!("filter: {}", filter_label(&self.filter)))
                    .show_ui(ui, |ui| {
                        let mut options: Vec<(String, Filter)> = vec![(
                            "all".into(),
                            Filter {
                                min_rating: 0,
                                unrated_only: false,
                            },
                        )];
                        for n in 1..=5u8 {
                            options.push((
                                format!("≥ {}", "★".repeat(n as usize)),
                                Filter {
                                    min_rating: n,
                                    unrated_only: false,
                                },
                            ));
                        }
                        options.push((
                            "unrated".into(),
                            Filter {
                                min_rating: 0,
                                unrated_only: true,
                            },
                        ));
                        for (label, value) in options {
                            if ui.selectable_label(self.filter == value, label).clicked() {
                                self.filter = value;
                                filter_changed = true;
                            }
                        }
                    });
                ui.separator();

                if self.config.show_exposure
                    && let Some(meta) = self
                        .session
                        .as_ref()
                        .and_then(|s| s.metas.get(&self.current))
                {
                    let mut parts: Vec<String> = Vec::new();
                    if let Some(iso) = meta.iso {
                        parts.push(format!("ISO {iso}"));
                    }
                    if let Some(s) = &meta.shutter {
                        parts.push(s.clone());
                    }
                    if let Some(a) = &meta.aperture {
                        parts.push(a.clone());
                    }
                    if let Some(f) = meta.focal_mm {
                        parts.push(format!("{f:.0}mm"));
                    }
                    ui.label(parts.join("  "));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .button("⚙")
                        .on_hover_text("Preferences (Cmd+,)")
                        .clicked()
                    {
                        self.settings.open = !self.settings.open;
                    }
                    if self.updates.has_available_update()
                        && ui
                            .button("⬇")
                            .on_hover_text("Viewr update available")
                            .clicked()
                    {
                        self.updates.open_status();
                    }
                    if self.config.show_performance
                        && let Some(s) = &self.session
                    {
                        let st = s.cache.stats();
                        ui.label(
                            egui::RichText::new(format!(
                                "rgba {}M  jpeg {}M",
                                st.rgba_bytes / (1024 * 1024),
                                st.jpeg_bytes / (1024 * 1024),
                            ))
                            .weak(),
                        );
                    }
                    match &self.status {
                        Status::Performance(message) if self.config.show_performance => {
                            ui.label(egui::RichText::new(message).weak());
                        }
                        Status::Error(message) => {
                            ui.label(egui::RichText::new(message).color(egui::Color32::LIGHT_RED));
                        }
                        Status::Empty | Status::Performance(_) => {}
                    }
                });
            });
        });
        if filter_changed {
            self.filter_dirty = true;
            self.apply_filter();
            // If the current image fell out of view, jump to the nearest.
            if self.visible.binary_search(&self.current).is_err()
                && let Some(&target) = self.visible.get(self.visible_pos())
            {
                self.select(target);
            }
            self.scroll_to_current = true;
            self.replan();
        }
    }

    fn metadata_panel(&mut self, ui: &mut egui::Ui) {
        let Some(session) = &self.session else {
            return;
        };
        let entry = &session.entries[self.current];
        let meta = session.metas.get(&self.current);
        egui::Panel::right("metadata")
            .exact_size(260.0)
            .show(ui, |ui| {
                ui.add_space(8.0);
                ui.heading(&entry.file_name);
                ui.separator();
                if let Some(m) = meta {
                    ui.label(&m.camera);
                    if let Some(lens) = &m.lens {
                        ui.label(lens);
                    }
                    ui.add_space(6.0);
                    egui::Grid::new("exif").num_columns(2).show(ui, |ui| {
                        if let Some(iso) = m.iso {
                            ui.label("ISO");
                            ui.label(iso.to_string());
                            ui.end_row();
                        }
                        if let Some(s) = &m.shutter {
                            ui.label("Shutter");
                            ui.label(s);
                            ui.end_row();
                        }
                        if let Some(a) = &m.aperture {
                            ui.label("Aperture");
                            ui.label(a);
                            ui.end_row();
                        }
                        if let Some(f) = m.focal_mm {
                            ui.label("Focal");
                            ui.label(format!("{f:.0}mm"));
                            ui.end_row();
                        }
                        if let Some(t) = &m.taken {
                            ui.label("Taken");
                            ui.label(t);
                            ui.end_row();
                        }
                        ui.label("Size");
                        ui.label(format!("{:.1} MB", entry.size as f64 / 1e6));
                        ui.end_row();
                    });
                } else {
                    ui.spinner();
                }
            });
    }

    fn loupe_mode(&mut self, ui: &mut egui::Ui) {
        self.refresh_star_marker_positions();
        let mut star_marker_cache = std::mem::take(&mut self.star_marker_cache);
        // Filmstrip over the visible sequence. Drag the divider to
        // resize; the height persists to viewr.toml.
        let mut clicked: Option<usize> = None;
        let current = self.current;
        let scroll_to = self.scroll_to_current;
        let mut strip_height = None;
        let mut demanded_thumbs = vec![current];
        if let Some(session) = &self.session {
            let inner = egui::Panel::bottom("filmstrip")
                .resizable(true)
                .default_size(self.config.filmstrip_height)
                .size_range(egui::Rangef::new(70.0, 320.0))
                .show(ui, |ui| {
                    let thumb_h = (ui.available_height() - 24.0).clamp(46.0, 290.0);
                    // Every column has a stable width so the strip can create only
                    // the widgets intersecting the viewport. Previously this loop
                    // built one widget tree per file even when almost all were clipped.
                    let cell = vec2(thumb_h * 1.4, thumb_h + 18.0);
                    let spacing = ui.spacing().item_spacing.x;
                    let visible_pos = visible_position(&self.visible, current);
                    filmstrip::configure_vertical_scroll(ui, self.config.vertical_scroll_filmstrip);
                    let strip_outer_rect = ui.available_rect_before_wrap();
                    let mut strip = egui::ScrollArea::horizontal()
                        .id_salt("filmstrip")
                        .auto_shrink([false, false]);
                    if scroll_to {
                        strip = strip.horizontal_scroll_offset(filmstrip::centered_scroll_offset(
                            self.visible.len(),
                            visible_pos,
                            cell.x,
                            spacing,
                            ui.available_width(),
                        ));
                    }
                    let strip_output = strip.show_viewport(ui, |ui, viewport| {
                        filmstrip::show_columns(
                            ui,
                            viewport,
                            self.visible.len(),
                            cell,
                            filmstrip::OVERSCAN_COLUMNS,
                            |columns_ui, range| {
                                for visible_pos in range {
                                    let i = self.visible[visible_pos];
                                    demanded_thumbs.push(i);
                                    let selected = i == current;
                                    let rating = session.ratings.get(&i).copied().unwrap_or(0);
                                    columns_ui.allocate_ui_with_layout(
                                        cell,
                                        egui::Layout::top_down(egui::Align::Center),
                                        |column_ui| {
                                            let response = match session.thumbs.get(&i) {
                                                Some(tex) => {
                                                    let size = tex.size_vec2();
                                                    let padding =
                                                        column_ui.spacing().button_padding * 2.0;
                                                    let inner = (vec2(cell.x, thumb_h) - padding)
                                                        .max(vec2(1.0, 1.0));
                                                    let scale =
                                                        (inner.x / size.x).min(inner.y / size.y);
                                                    column_ui.add_sized(
                                                        vec2(cell.x, thumb_h),
                                                        // `Button::image` caps image atoms at the
                                                        // font height in egui 0.35. A regular button
                                                        // intentionally preserves this exact size.
                                                        egui::Button::new(
                                                            egui::Image::new((tex.id(), size))
                                                                .fit_to_exact_size(size * scale),
                                                        )
                                                        .selected(selected),
                                                    )
                                                }
                                                None => column_ui.add_sized(
                                                    vec2(cell.x, thumb_h),
                                                    egui::Button::new(
                                                        egui::RichText::new("…").weak(),
                                                    )
                                                    .selected(selected),
                                                ),
                                            };
                                            if let Some(color) = self.tier_stroke(session, i) {
                                                column_ui.painter().rect_stroke(
                                                    response.rect.expand(1.0),
                                                    3.0,
                                                    egui::Stroke::new(2.0, color),
                                                    egui::StrokeKind::Outside,
                                                );
                                            }
                                            column_ui.horizontal_centered(|ui| {
                                                if rating > 0 {
                                                    ui.label(
                                                        egui::RichText::new(
                                                            "★".repeat(rating as usize),
                                                        )
                                                        .size(10.0)
                                                        .color(egui::Color32::GOLD),
                                                    );
                                                }
                                                if let Some((mark, color, help)) =
                                                    self.tier_mark(session, i)
                                                {
                                                    ui.label(
                                                        egui::RichText::new(mark)
                                                            .size(10.0)
                                                            .color(color),
                                                    )
                                                    .on_hover_text(help);
                                                }
                                            });
                                            if response.clicked() {
                                                clicked = Some(i);
                                            }
                                        },
                                    );
                                }
                            },
                        );
                    });
                    let marker_clicked = filmstrip::show_star_markers(
                        ui,
                        filmstrip::StarMarkerSpec {
                            outer_rect: strip_outer_rect,
                            viewport_rect: strip_output.inner_rect,
                            total: self.visible.len(),
                            column_width: cell.x,
                            spacing,
                            revision: self.star_marker_revision,
                            positions: &self.starred_visible_positions,
                        },
                        &mut star_marker_cache,
                    );
                    if let Some(visible_position) = marker_clicked {
                        let mut state = strip_output.state;
                        state.offset.x = filmstrip::centered_scroll_offset(
                            self.visible.len(),
                            visible_position,
                            cell.x,
                            spacing,
                            strip_output.inner_rect.width(),
                        );
                        state.store(ui.ctx(), strip_output.id);
                        ui.ctx().request_repaint();
                    }
                });
            strip_height = Some(inner.response.rect.height());
        }
        self.star_marker_cache = star_marker_cache;
        self.manage_thumbnail_textures(&demanded_thumbs);
        // Persist a divider-drag once the mouse is released.
        if let Some(h) = strip_height
            && (h - self.config.filmstrip_height).abs() > 1.0
            && !self.ctx.input(|i| i.pointer.any_down())
        {
            self.config.filmstrip_height = h;
            self.config.save();
        }
        self.scroll_to_current = false;
        if let Some(i) = clicked {
            self.select(i);
        }

        // Loupe. The zoom state lives in full-res "logical" space so the
        // same framing holds no matter which tier backs the texture:
        // full = exact, browse = half-res (×2), thumb = stand-in drawn at
        // the retained framing (blurry→sharp in place, no flash-to-fit).
        let loupe_rect = ui.available_rect_before_wrap();
        let mut img_size = None;
        let full_logical = self.session.as_ref().and_then(|session| {
            session
                .cache
                .get_rgba((self.current, Tier::Full))
                .map(|buf| vec2(buf.width as f32, buf.height as f32))
        });
        let best = self.session.as_ref().and_then(|s| {
            s.textures.get(&(self.current, Tier::Browse)).map(|t| {
                (
                    Tier::Browse,
                    t.clone(),
                    full_logical.unwrap_or_else(|| t.size_vec2() * 2.0),
                )
            })
        });
        let mut standin = false; // zoomed onto a lower tier than Full
        match best {
            Some((tier, tex, logical)) => {
                img_size = Some(logical);
                self.last_logical = Some(logical);
                if let Some(t0) = self.nav_started.take() {
                    self.status = Status::Performance(format!("{tier:?} in {:.0?}", t0.elapsed()));
                }
                let scroll_zooms = self.config.scroll == ScrollMode::Zoom;
                let response = loupe::show(ui, &tex, logical, &mut self.zoom, scroll_zooms);
                standin = tier != Tier::Full && !matches!(self.zoom, Zoom::Fit);
                if standin && self.progressive_full_overlay(ui, &response) {
                    standin = false;
                }
                if let Some(pos) = response.double_clicked_at {
                    loupe::toggle_100(&mut self.zoom, loupe_rect, logical, pos);
                    self.replan();
                }
            }
            None => {
                let thumb = self
                    .session
                    .as_ref()
                    .and_then(|s| s.thumbs.get(&self.current).cloned());
                if let Some(tex) = thumb {
                    // Burst frames share dimensions, so the previous logical
                    // size keeps the framing; fall back to fit otherwise.
                    let logical = self.last_logical.unwrap_or_else(|| tex.size_vec2());
                    img_size = Some(logical);
                    let scroll_zooms = self.config.scroll == ScrollMode::Zoom;
                    let response = loupe::show(ui, &tex, logical, &mut self.zoom, scroll_zooms);
                    standin = !matches!(self.zoom, Zoom::Fit);
                    if standin && self.progressive_full_overlay(ui, &response) {
                        standin = false;
                    }
                } else {
                    ui.centered_and_justified(|ui| {
                        ui.spinner();
                    });
                }
            }
        }

        // Never let a low-res stand-in masquerade as full res while
        // judging focus: badge until the Full texture takes over.
        if standin && self.config.show_loading {
            egui::Area::new(egui::Id::new("standin-badge"))
                .fixed_pos(loupe_rect.center_top() + egui::vec2(-70.0, 10.0))
                .show(&self.ctx.clone(), |ui| {
                    ui.label(
                        egui::RichText::new(" PREVIEW — loading full res ")
                            .size(13.0)
                            .color(egui::Color32::BLACK)
                            .background_color(egui::Color32::from_rgb(255, 200, 60)),
                    );
                });
        }

        // Rating overlay.
        let rating = self.rating_of(self.current);
        egui::Area::new(egui::Id::new("rating-overlay"))
            .fixed_pos(loupe_rect.left_bottom() + egui::vec2(12.0, -40.0))
            .show(&self.ctx.clone(), |ui| {
                ui.label(
                    egui::RichText::new(stars(rating))
                        .size(24.0)
                        .color(if rating > 0 {
                            egui::Color32::GOLD
                        } else {
                            egui::Color32::from_white_alpha(48)
                        })
                        .background_color(egui::Color32::from_black_alpha(120)),
                );
            });

        self.handle_keys(loupe_rect, img_size);
    }

    fn grid_mode(&mut self, ui: &mut egui::Ui) {
        let cols = self.grid_cols();
        let cell = vec2(self.config.grid_cell, self.config.grid_cell * 0.75);
        let mut clicked: Option<usize> = None;
        let mut open_loupe = false;
        let current = self.current;
        let scroll_to = self.scroll_to_current;
        let rect = ui.max_rect();
        let mut demanded_thumbs = vec![current];
        if let Some(session) = &self.session {
            let rows = self.visible.len().div_ceil(cols);
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show_rows(ui, cell.y + 26.0, rows, |ui, range| {
                    for row in range {
                        ui.horizontal(|ui| {
                            for col in 0..cols {
                                let Some(&i) = self.visible.get(row * cols + col) else {
                                    break;
                                };
                                demanded_thumbs.push(i);
                                let selected = i == current;
                                let rating = session.ratings.get(&i).copied().unwrap_or(0);
                                let tier_stroke = self.tier_stroke(session, i);
                                ui.vertical(|ui| {
                                    ui.set_width(cell.x);
                                    let response = match session.thumbs.get(&i) {
                                        Some(tex) => {
                                            let size = tex.size_vec2();
                                            let padding = ui.spacing().button_padding * 2.0;
                                            let inner = (cell - padding).max(vec2(1.0, 1.0));
                                            let scale =
                                                (inner.x / size.x).min(inner.y / size.y).min(1.0);
                                            ui.add_sized(
                                                cell,
                                                egui::Button::new(
                                                    egui::Image::new((tex.id(), size))
                                                        .fit_to_exact_size(size * scale),
                                                )
                                                .selected(selected),
                                            )
                                        }
                                        None => ui.add_sized(
                                            cell,
                                            egui::Button::new(egui::RichText::new("…").weak())
                                                .selected(selected),
                                        ),
                                    };
                                    if let Some(color) = tier_stroke {
                                        ui.painter().rect_stroke(
                                            response.rect.expand(1.0),
                                            3.0,
                                            egui::Stroke::new(2.0, color),
                                            egui::StrokeKind::Outside,
                                        );
                                    }
                                    ui.horizontal_centered(|ui| {
                                        if rating > 0 {
                                            ui.label(
                                                egui::RichText::new("★".repeat(rating as usize))
                                                    .size(11.0)
                                                    .color(egui::Color32::GOLD),
                                            );
                                        }
                                        if let Some((mark, color, help)) =
                                            self.tier_mark(session, i)
                                        {
                                            ui.label(
                                                egui::RichText::new(mark).size(11.0).color(color),
                                            )
                                            .on_hover_text(help);
                                        }
                                    });
                                    if response.clicked() {
                                        clicked = Some(i);
                                    }
                                    if response.double_clicked() {
                                        clicked = Some(i);
                                        open_loupe = true;
                                    }
                                    if selected && scroll_to {
                                        response.scroll_to_me(Some(egui::Align::Center));
                                    }
                                });
                            }
                        });
                    }
                });
        }
        self.manage_thumbnail_textures(&demanded_thumbs);
        self.scroll_to_current = false;
        if let Some(i) = clicked {
            self.select(i);
        }
        if open_loupe {
            self.mode = Mode::Loupe;
        }
        self.handle_keys(rect, None);
    }
}

fn to_color_image(buf: &PixelBuf) -> egui::ColorImage {
    egui::ColorImage::from_rgba_unmultiplied([buf.width as usize, buf.height as usize], &buf.rgba)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_window_uses_stable_persistent_geometry() {
        let options = native_options();
        assert!(options.persist_window);
        assert_eq!(options.viewport.app_id.as_deref(), Some("viewr"));
        assert_eq!(options.viewport.inner_size, Some(vec2(1500.0, 950.0)));
    }

    fn metadata_with_rating(rating: Option<u32>) -> FileMeta {
        FileMeta {
            rating,
            camera: "Test Camera".into(),
            ..FileMeta::default()
        }
    }

    #[test]
    fn metadata_only_events_preserve_rating_precedence_and_filter_updates() {
        let mut ratings = HashMap::from([(1, 5)]);
        let mut metas = HashMap::new();
        let filter = Filter {
            min_rating: 4,
            unrated_only: false,
        };

        assert!(install_metadata(
            &mut ratings,
            &mut metas,
            0,
            metadata_with_rating(Some(4)),
            filter,
            true,
        ));
        assert_eq!(ratings.get(&0), Some(&4));
        assert_eq!(
            metas.get(&0).map(|meta| meta.camera.as_str()),
            Some("Test Camera")
        );

        assert!(!install_metadata(
            &mut ratings,
            &mut metas,
            1,
            metadata_with_rating(Some(2)),
            filter,
            true,
        ));
        assert_eq!(ratings.get(&1), Some(&5), "persisted rating must win");
    }

    #[test]
    fn delayed_database_refresh_preserves_source_precedence() {
        let persisted = HashMap::from([(0, 5), (1, 4)]);
        let metas = HashMap::from([
            (0, metadata_with_rating(Some(1))),
            (2, metadata_with_rating(Some(9))),
            (3, metadata_with_rating(Some(0))),
        ]);
        let explicit = HashMap::from([(1, 0), (3, 2)]);

        assert_eq!(
            merge_refreshed_ratings(persisted, &metas, &explicit),
            HashMap::from([(0, 5), (1, 0), (2, 5), (3, 2)])
        );
    }

    #[test]
    fn configured_database_refreshes_until_its_snapshot_loads() {
        assert!(rating_refresh_required(true));
        assert!(!rating_refresh_required(false));
        assert!(rating_sources_blocked(true, false));
        assert!(!rating_sources_blocked(true, true));
        assert!(!rating_sources_blocked(false, false));

        assert_eq!(
            next_rating_refresh_delay(Duration::from_millis(50)),
            Duration::from_millis(100)
        );
        assert_eq!(
            next_rating_refresh_delay(Duration::from_secs(4)),
            RATING_DB_REFRESH_MAX_POLL
        );
        assert_eq!(
            next_rating_refresh_delay(RATING_DB_REFRESH_MAX_POLL),
            RATING_DB_REFRESH_MAX_POLL
        );
    }

    #[test]
    fn unavailable_database_blocks_embedded_rating_but_retains_metadata() {
        let mut ratings = HashMap::new();
        let mut metas = HashMap::new();
        let filter = Filter {
            min_rating: 4,
            unrated_only: false,
        };

        assert!(!install_metadata(
            &mut ratings,
            &mut metas,
            0,
            metadata_with_rating(Some(5)),
            filter,
            false,
        ));
        assert!(ratings.is_empty());
        assert_eq!(
            metas.get(&0).and_then(|meta| meta.rating),
            Some(5),
            "metadata remains available for reconciliation after recovery"
        );
    }

    #[test]
    fn delayed_metadata_and_thumbnail_events_do_not_resurrect_a_cleared_rating() {
        let mut ratings = HashMap::from([(0, 5)]);
        let mut metas = HashMap::new();
        let filter = Filter {
            min_rating: 0,
            unrated_only: true,
        };

        assert_eq!(install_user_rating(&mut ratings, 0, 0), 5);
        assert_eq!(ratings.get(&0), Some(&0), "zero remains authoritative");

        // Models a MetadataReady event that was decoded before the user's
        // clear-rating command reached the sidecar.
        assert!(!install_metadata(
            &mut ratings,
            &mut metas,
            0,
            metadata_with_rating(Some(4)),
            filter,
            true,
        ));
        assert_eq!(ratings.get(&0), Some(&0));

        // Models the duplicate metadata carried by a later ThumbReady event.
        assert!(!install_metadata(
            &mut ratings,
            &mut metas,
            0,
            metadata_with_rating(Some(3)),
            filter,
            true,
        ));
        assert_eq!(ratings.get(&0), Some(&0));

        let mut visible = Vec::new();
        assert!(rebuild_visible(&mut visible, 1, &ratings, filter));
        assert_eq!(
            visible,
            [0],
            "explicit zero still passes the unrated filter"
        );
    }

    #[test]
    fn user_rating_updates_every_entry_with_the_same_sidecar_owner() {
        let owner = PathBuf::from("/photos/photo.xmp");
        let owners = vec![
            Some(owner.clone()),
            Some(owner),
            Some(PathBuf::from("/photos/other.xmp")),
            None,
        ];
        let mut ratings = HashMap::from([(0, 1), (1, 2), (2, 3), (3, 4)]);
        let filter = Filter {
            min_rating: 5,
            unrated_only: false,
        };
        let members = build_owner_members(&owners);

        assert!(install_rating_for_members(
            &mut ratings,
            members[0].as_deref().unwrap(),
            5,
            |old, new| filter.passes(old) != filter.passes(new)
        ));
        assert_eq!(ratings, HashMap::from([(0, 5), (1, 5), (2, 3), (3, 4)]));

        assert!(!install_rating_for_members(
            &mut ratings,
            &[3],
            4,
            |old, new| filter.passes(old) != filter.passes(new)
        ));
        assert_eq!(ratings.get(&3), Some(&4));
    }

    #[test]
    fn visible_position_finds_exact_and_nearest_items() {
        let visible = [1, 4, 8, 12];
        assert_eq!(visible_position(&visible, 4), 1);
        assert_eq!(visible_position(&visible, 0), 0);
        assert_eq!(visible_position(&visible, 5), 2);
        assert_eq!(visible_position(&visible, 99), 3);
        assert_eq!(visible_position(&[], 5), 0);
    }

    #[test]
    fn star_markers_follow_the_filtered_filmstrip_and_ignore_zero_ratings() {
        let visible = [2, 7, 40, 99];
        let ratings = HashMap::from([(2, 0), (7, 1), (99, 5), (120, 4)]);
        let mut positions = Vec::new();
        rebuild_starred_visible_positions(&mut positions, &visible, &ratings);

        assert_eq!(positions, [1, 3]);
    }

    #[test]
    fn sparse_and_dense_rating_maps_build_the_same_marker_positions() {
        let visible: Vec<_> = (0..100).collect();
        let sparse = HashMap::from([(7, 1), (80, 5)]);
        let mut dense: HashMap<_, _> = visible.iter().map(|&index| (index, 0)).collect();
        dense.insert(7, 1);
        dense.insert(80, 5);
        let mut sparse_positions = Vec::new();
        let mut dense_positions = Vec::new();

        rebuild_starred_visible_positions(&mut sparse_positions, &visible, &sparse);
        rebuild_starred_visible_positions(&mut dense_positions, &visible, &dense);

        assert_eq!(sparse_positions, [7, 80]);
        assert_eq!(dense_positions, sparse_positions);
    }

    #[test]
    fn untiled_texture_policy_keeps_browse_and_delegates_full_to_tiles() {
        let near = [6, 7, 8];
        assert_eq!(current_texture_candidates(7), [(7, Tier::Browse)]);
        assert!(!main_texture_should_be_kept(7, Tier::Full, &near));
        assert!(!main_texture_should_be_kept(8, Tier::Full, &near));
        assert!(main_texture_should_be_kept(8, Tier::Browse, &near));
        assert!(!main_texture_should_be_kept(9, Tier::Browse, &near));
    }

    #[test]
    fn configured_ram_budget_includes_all_three_cache_rings() {
        for total in [0, 1, 3, 1_000_000_000, 64_000_000_000, u64::MAX] {
            let (thumbs, rgba, jpeg) = ram_cache_budgets(total);
            assert_eq!(thumbs + rgba + jpeg, total);
            assert!(thumbs <= THUMB_BUDGET);
            assert!(thumbs <= total / 2);
            assert!(rgba >= jpeg || total <= 1);
        }

        let (thumbs, rgba, jpeg) = ram_cache_budgets(1_000_000_000);
        assert_eq!(thumbs, THUMB_BUDGET);
        assert_eq!(rgba + jpeg, 1_000_000_000 - THUMB_BUDGET);
    }

    #[test]
    fn rebuild_visible_filters_in_source_order() {
        let ratings = HashMap::from([(0, 1), (2, 4), (3, 5), (5, 3)]);
        let mut visible = Vec::new();
        let identity = rebuild_visible(
            &mut visible,
            6,
            &ratings,
            Filter {
                min_rating: 4,
                unrated_only: false,
            },
        );
        assert_eq!(visible, [2, 3]);
        assert!(!identity);
    }

    #[test]
    fn rebuild_visible_falls_back_to_identity_when_filter_matches_nothing() {
        let ratings = HashMap::from([(1, 2)]);
        let mut visible = vec![99];
        let identity = rebuild_visible(
            &mut visible,
            3,
            &ratings,
            Filter {
                min_rating: 5,
                unrated_only: false,
            },
        );
        assert_eq!(visible, [0, 1, 2]);
        assert!(identity);
    }

    #[test]
    fn rebuild_visible_handles_unfiltered_and_unrated_views() {
        let ratings = HashMap::from([(1, 3)]);
        let mut visible = Vec::new();
        assert!(rebuild_visible(
            &mut visible,
            3,
            &ratings,
            Filter::default()
        ));
        assert_eq!(visible, [0, 1, 2]);

        let identity = rebuild_visible(
            &mut visible,
            3,
            &ratings,
            Filter {
                min_rating: 0,
                unrated_only: true,
            },
        );
        assert_eq!(visible, [0, 2]);
        assert!(!identity);
    }
}
