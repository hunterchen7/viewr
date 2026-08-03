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
use viewr_core::cache_ram::{RamCache, RamCacheBudgets};
use viewr_core::db::{Db, default_db_path};
use viewr_core::folder::{FolderEntry, normalize_physical_path, scan};
use viewr_core::jobs::{Engine, EngineOptions, Event, NavState, ViewHint};
use viewr_core::library::{
    Library, RatingLoad, load_ratings_with_owners, rating_owner_keys, try_load_ratings_with_owners,
};
use viewr_core::meta::FileMeta;
use viewr_core::types::Tier;

use crate::config::{Action, Config, ScrollMode, TierIndicator};
use crate::filmstrip;
use crate::image_info;
use crate::loupe::{self, LoupeResponse, Zoom};
use crate::pixels::to_color_image;
use crate::progressive_texture::{self, TileCoord};
use crate::rating_groups::{build_owner_members, install_rating_for_members};
use crate::settings::SettingsState;
use crate::texture_lru::ByteLru;
use crate::update::UpdateManager;

const THUMB_BUDGET: u64 = 384 * 1024 * 1024;
/// Logical RGBA bytes retained by thumbnail texture handles. Actual backend
/// allocation can be slightly higher, but remains proportional to this cap.
const THUMB_TEXTURE_BUDGET_BYTES: usize = 256 * 1024 * 1024;
const THUMB_UPLOADS_PER_FRAME: usize = 24;
/// Upper bound on visible Full tiles uploaded in one frame. The visible rect
/// normally uploads completely in its arrival frame (4-6 tiles on common
/// viewports, ~0.5-0.9 ms each); the cap bounds pathological viewports.
const MAX_VISIBLE_FULL_TILE_UPLOADS_PER_FRAME: usize = 16;
const BACKGROUND_FULL_TILE_UPLOADS_PER_FRAME: usize = 1;
const THUMB_REQUEST_POLL_AFTER: Duration = Duration::from_millis(16);
const THUMB_REQUEST_STALE_AFTER: Duration = Duration::from_millis(500);
const THUMB_FAILURE_RETRY_AFTER: Duration = Duration::from_secs(2);
const RATING_DB_REFRESH_POLL: Duration = Duration::from_millis(50);
const RATING_DB_REFRESH_MAX_POLL: Duration = Duration::from_secs(5);

type RatingRefreshResult = std::result::Result<RatingLoad, String>;

fn ram_cache_budgets(total: u64) -> RamCacheBudgets {
    let thumbs = THUMB_BUDGET.min(total / 2);
    let developed = total - thumbs;
    let fifth = developed / 5;
    let remainder = developed % 5;
    let full = fifth * 3 + remainder.min(3);
    let browse = fifth + remainder.saturating_sub(3).min(1);
    let jpeg = developed - full - browse;
    RamCacheBudgets::new(thumbs, browse, full, jpeg)
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
            // Build the shared develop transfer table before the first image
            // job needs it, off the UI thread.
            std::thread::spawn(viewr_core::develop::warm_gamma_lut);
            let mut app = App::empty(cc);
            if app.config.clear_disk_cache_on_exit {
                // Crash leftovers: a clean exit already purged, so this is
                // normally a no-op scan. Runs before the engine can start
                // warming from (or writing to) the cache.
                purge_disk_cache();
            }
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

fn full_resolution_is_urgent(mode: Mode, zoom: Zoom) -> bool {
    mode == Mode::Loupe && !matches!(zoom, Zoom::Fit)
}

fn image_failure_is_visible(
    current: usize,
    mode: Mode,
    zoom: Zoom,
    failed_index: usize,
    failed_tier: Tier,
) -> bool {
    failed_index == current
        && (failed_tier == Tier::Browse
            || (failed_tier == Tier::Full && full_resolution_is_urgent(mode, zoom)))
}

fn top_bar_title(
    mode: Mode,
    visible_position: usize,
    visible_len: usize,
    file_name: &str,
) -> String {
    let position = format!("{}/{visible_len}", visible_position + 1);
    if mode == Mode::Grid && !file_name.is_empty() {
        format!("{position}  {file_name}")
    } else {
        position
    }
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

/// The image the next arrow-key navigation reaches: the visible-sequence
/// neighbor one step from `pos` in the navigation direction.
fn next_ahead_index(visible: &[usize], pos: usize, direction: i8, current: usize) -> Option<usize> {
    let next_pos = if direction < 0 {
        pos.checked_sub(1)?
    } else {
        pos.checked_add(1)?
    };
    let index = visible.get(next_pos).copied()?;
    (index != current).then_some(index)
}

/// Full tiles are bounded to the current zoomed image plus exactly one
/// pre-uploaded neighbor (the next-ahead image), so zoomed arrow-key
/// navigation can paint sharp on its first frame without unbounded GPU growth.
fn full_tile_should_be_kept(
    index: usize,
    current: usize,
    next_ahead: Option<usize>,
    zoomed: bool,
) -> bool {
    zoomed && (index == current || Some(index) == next_ahead)
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
    /// Full-resolution texture tiles exist only for the current zoomed image
    /// and, once its own tiles are complete, the next-ahead image whose Full
    /// pixels are already resident. Browse stays underneath them so
    /// incomplete regions remain usable.
    full_tiles: HashMap<(usize, TileCoord), egui::TextureHandle>,
    /// Tiles in `full_tiles` uploaded from a provisional decode band rather
    /// than the installed Full buffer. Normally the finished buffer contains
    /// the same bytes and these simply reclassify; if the band's backing
    /// object turns out corrupt (band gone, no Full pixels), they are
    /// dropped so a RAW re-development cannot hide behind stale tiles.
    full_tiles_from_band: HashSet<(usize, TileCoord)>,
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
    /// The loupe rect allocated by the last painted frame; with the retained
    /// zoom and logical size it reproduces the visible UV for view hints.
    last_loupe_rect: Option<egui::Rect>,
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
            last_loupe_rect: None,
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
        let cache = Arc::new(RamCache::new(ram_cache_budgets(ram_bytes)));
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
            full_tiles_from_band: HashSet::new(),
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
            // Publish the hint before the plan so a rehydrate claimed right
            // after set_plan already sees the framing it decodes for. The
            // hint is advisory and outside job identity: updating it cannot
            // cancel in-flight work, and staleness only costs latency.
            session.engine.set_view_hint(self.view_hint(session));
            session.engine.navigate(NavState {
                current: self.current,
                direction: self.direction,
                zoomed: full_resolution_is_urgent(self.mode, self.zoom),
            });
        }
    }

    /// The advisory visible-band hint for the current zoomed framing.
    ///
    /// Zoom framing is retained across navigation, so the previous logical
    /// size reproduces the new image's visible rows for burst sequences; a
    /// dimension mismatch only degrades the advisory band, never the output.
    fn view_hint(&self, session: &Session) -> Option<ViewHint> {
        if !full_resolution_is_urgent(self.mode, self.zoom) {
            return None;
        }
        let rect = self.last_loupe_rect?;
        let img_size = session
            .cache
            .get_rgba((self.current, Tier::Full))
            .map(|buf| vec2(buf.width as f32, buf.height as f32))
            .or(self.last_logical)?;
        let uv = loupe::visible_uv_for(rect, img_size, self.zoom);
        Some(ViewHint {
            index: self.current,
            uv_y0: uv.min.y,
            uv_y1: uv.max.y,
            align_px: progressive_texture::TILE_EDGE,
            gutter_px: progressive_texture::SAMPLE_GUTTER,
        })
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
        let mode = self.mode;
        let zoom = self.zoom;
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
                    } else if image_failure_is_visible(current, mode, zoom, index, tier) {
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
        let next_ahead = next_ahead_index(&self.visible, pos, self.direction, current);
        let Some(session) = &mut self.session else {
            return;
        };
        session
            .textures
            .retain(|(i, tier), _| main_texture_should_be_kept(*i, *tier, &near));
        let zoomed = !matches!(zoom, Zoom::Fit);
        session
            .full_tiles
            .retain(|(index, _), _| full_tile_should_be_kept(*index, current, next_ahead, zoomed));
        session
            .full_tiles_from_band
            .retain(|(index, _)| full_tile_should_be_kept(*index, current, next_ahead, zoomed));

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
    /// complete, the background budget pre-uploads the next-ahead image's
    /// predicted-visible tiles so zoomed navigation paints sharp on frame
    /// one, then trickles this image's remaining tiles outward.
    ///
    /// Before the installed Full buffer lands, the provisional decode band
    /// (published between the rehydrate's decode phases) backs the same
    /// tiles: same keys, same bytes, so tiles uploaded from the band are
    /// reused as-is once the full buffer arrives.
    fn progressive_full_overlay(&mut self, ui: &egui::Ui, response: &LoupeResponse) -> bool {
        enum OverlaySource {
            Full(std::sync::Arc<viewr_core::types::PixelBuf>),
            Band(std::sync::Arc<viewr_core::cache_ram::FullBand>),
        }
        if matches!(self.zoom, Zoom::Fit) {
            return false;
        }
        let current = self.current;
        let next_ahead =
            next_ahead_index(&self.visible, self.visible_pos(), self.direction, current);
        let ctx = self.ctx.clone();
        let source = {
            let Some(session) = &self.session else {
                return false;
            };
            if let Some(buf) = session.cache.get_rgba((current, Tier::Full)) {
                OverlaySource::Full(buf)
            } else if let Some(band) = session.cache.get_full_band(current) {
                OverlaySource::Band(band)
            } else {
                // Neither pixels nor a band. Any band-sourced tiles have lost
                // their backing cache object (corrupt-object fallback): drop
                // them so the RAW re-development cannot sit behind stale
                // JPEG-derived tiles.
                let Some(session) = &mut self.session else {
                    return false;
                };
                for key in session.full_tiles_from_band.drain() {
                    session.full_tiles.remove(&key);
                }
                return false;
            }
        };
        let (image_width, image_height) = match &source {
            OverlaySource::Full(buf) => (buf.width, buf.height),
            OverlaySource::Band(band) => (band.full_width, band.full_height),
        };
        let order =
            progressive_texture::priority_order(image_width, image_height, response.visible_uv);
        let visible_count = progressive_texture::visible_prefix_len(
            image_width,
            image_height,
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
        // Sharpen the whole visible rect in the arrival frame: per-tile cost
        // is well under a millisecond, so even a 5K viewport's tiles fit one
        // 120 Hz frame; the cap only bounds pathological viewports.
        let upload_budget = if missing_visible > 0 {
            missing_visible.min(MAX_VISIBLE_FULL_TILE_UPLOADS_PER_FRAME)
        } else {
            BACKGROUND_FULL_TILE_UPLOADS_PER_FRAME
        };
        let mut uploaded = 0;
        let mut invalid_storage = false;
        // Extracts one tile from whichever pixel source backs this frame:
        // the installed Full buffer, or the provisional decode band (whose
        // covered tiles carry identical bytes). Band-uncovered tiles wait
        // for the complete frame without counting as invalid storage.
        enum TileImage {
            Ready(egui::ColorImage),
            NotCovered,
            Invalid,
        }
        let extract = |source: &OverlaySource, tile| match source {
            OverlaySource::Full(buf) => match progressive_texture::color_image(buf, tile) {
                Some(image) => TileImage::Ready(image),
                None => TileImage::Invalid,
            },
            OverlaySource::Band(band) => match progressive_texture::color_image_band(band, tile) {
                Some(image) => TileImage::Ready(image),
                None => TileImage::NotCovered,
            },
        };
        if missing_visible > 0 {
            for &tile in &order {
                if uploaded >= upload_budget {
                    break;
                }
                let key = (current, tile);
                if session.full_tiles.contains_key(&key) {
                    continue;
                }
                let image = match extract(&source, tile) {
                    TileImage::Ready(image) => image,
                    TileImage::NotCovered => continue,
                    TileImage::Invalid => {
                        invalid_storage = true;
                        break;
                    }
                };
                let texture = ctx.load_texture(
                    format!("img{current}-Full-{}-{}", tile.col, tile.row),
                    image,
                    egui::TextureOptions::LINEAR,
                );
                session.full_tiles.insert(key, texture);
                if matches!(source, OverlaySource::Band(_)) {
                    session.full_tiles_from_band.insert(key);
                }
                uploaded += 1;
            }
        }
        if matches!(source, OverlaySource::Full(_)) {
            // The finished buffer contains the same bytes the band carried;
            // its tiles are no longer provisional.
            session.full_tiles_from_band.clear();
        }
        // With the visible rect complete, the background budget goes first to
        // the next-ahead warm below — an imminent zoomed navigation values the
        // neighbor's visible tiles above this image's off-screen remainder —
        // and any leftover trickles the current image afterwards.

        let painter = ui.painter().with_clip_rect(response.viewport_rect);
        for &tile in &order[..visible_count] {
            let Some(texture) = session.full_tiles.get(&(current, tile)) else {
                continue;
            };
            let Some(geometry) = progressive_texture::paint_geometry(
                image_width,
                image_height,
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
        // The tile map may also hold next-ahead tiles, so progress is counted
        // per image rather than by the map's total size.
        let current_resident = order
            .iter()
            .filter(|&&tile| session.full_tiles.contains_key(&(current, tile)))
            .count();
        let mut more_upload_work = current_resident < order.len();

        // The visible rect is complete, so the background budget warms the
        // next-ahead image's predicted-visible tiles first. Zoom framing
        // persists across images, making the current visible_uv the best
        // prediction; an aspect-ratio mismatch is harmless because extra
        // tiles simply go unpainted and the order is recomputed on arrival.
        if !invalid_storage
            && missing_visible == 0
            && uploaded < upload_budget
            && let Some(next_ahead) = next_ahead
            && let Some(next_buf) = session.cache.get_rgba((next_ahead, Tier::Full))
        {
            let next_order = progressive_texture::priority_order(
                next_buf.width,
                next_buf.height,
                response.visible_uv,
            );
            let next_visible = progressive_texture::visible_prefix_len(
                next_buf.width,
                next_buf.height,
                response.visible_uv,
                &next_order,
            );
            let mut next_invalid = false;
            for &tile in &next_order[..next_visible] {
                if uploaded >= upload_budget {
                    break;
                }
                let key = (next_ahead, tile);
                if session.full_tiles.contains_key(&key) {
                    continue;
                }
                let Some(image) = progressive_texture::color_image(&next_buf, tile) else {
                    next_invalid = true;
                    break;
                };
                let texture = ctx.load_texture(
                    format!("img{next_ahead}-Full-{}-{}", tile.col, tile.row),
                    image,
                    egui::TextureOptions::LINEAR,
                );
                session.full_tiles.insert(key, texture);
                uploaded += 1;
            }
            if !next_invalid {
                let next_resident = next_order[..next_visible]
                    .iter()
                    .filter(|&&tile| session.full_tiles.contains_key(&(next_ahead, tile)))
                    .count();
                more_upload_work |= next_resident < next_visible;
            }
        }

        // Background budget the neighbor warm left over trickles the current
        // image's remaining off-screen tiles.
        if missing_visible == 0 && uploaded < upload_budget && !invalid_storage {
            for &tile in &order {
                if uploaded >= upload_budget {
                    break;
                }
                let key = (current, tile);
                if session.full_tiles.contains_key(&key) {
                    continue;
                }
                let image = match extract(&source, tile) {
                    TileImage::Ready(image) => image,
                    TileImage::NotCovered => continue,
                    TileImage::Invalid => {
                        invalid_storage = true;
                        break;
                    }
                };
                let texture = ctx.load_texture(
                    format!("img{current}-Full-{}-{}", tile.col, tile.row),
                    image,
                    egui::TextureOptions::LINEAR,
                );
                session.full_tiles.insert(key, texture);
                uploaded += 1;
            }
        }

        match &source {
            OverlaySource::Full(_) => {
                if !invalid_storage && more_upload_work {
                    ctx.request_repaint();
                }
            }
            OverlaySource::Band(_) => {
                // The rest of the frame is still decoding; keep polling for
                // the installed buffer and the tiles the band cannot back.
                ctx.request_repaint();
            }
        }
        visible_complete
    }

    /// Handles every key that needs no frame-local layout, at frame START —
    /// before events drain and textures upload — so a navigation keypress
    /// paints the new image in the same frame instead of one frame later.
    /// Zoom toggling needs this frame's loupe geometry and stays in
    /// [`handle_keys`](Self::handle_keys); each bound key is read in exactly
    /// one of the two methods (`key_pressed` does not consume).
    fn handle_nav_keys(&mut self) {
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
            self.replan();
        }
        if k.info {
            self.show_metadata = !self.show_metadata;
        }
        if k.fullscreen {
            self.fullscreen = !self.fullscreen;
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.fullscreen));
        }
    }

    /// Handles the zoom toggle, which anchors on this frame's loupe geometry
    /// and therefore runs after the loupe lays out. Every other key is
    /// handled at frame start by [`handle_nav_keys`](Self::handle_nav_keys).
    fn handle_keys(&mut self, loupe_rect: egui::Rect, img_size: Option<egui::Vec2>) {
        if self.settings.capturing() || self.updates.blocks_app_input() {
            return; // a modal owns this frame's keystrokes
        }
        let ctx = self.ctx.clone();
        let toggle_zoom = {
            let config = &self.config;
            ctx.input(|i| config.pressed(i, Action::ToggleZoom))
        };
        if toggle_zoom
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
        // One lock acquisition per cell; the border and mark indicators call
        // this for every visible thumbnail on every painted frame.
        let (full_rgba, browse_rgba, any_jpeg) = session.cache.image_residency(index);
        if full_rgba {
            Some(CacheState::Full)
        } else if browse_rgba {
            Some(CacheState::Browse)
        } else if any_jpeg {
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

/// Deletes every disk-cache develop through the hardened GC traversal.
fn purge_disk_cache() {
    if let Some(cache) = DiskCache::open_default(0) {
        cache.purge();
    }
}

impl eframe::App for App {
    fn on_exit(&mut self) {
        if !self.config.clear_disk_cache_on_exit {
            return;
        }
        // Stop the engine's writers first so the purge does not race fresh
        // cache objects, then delete synchronously: exit blocks briefly on
        // file removal instead of leaving the cache behind.
        self.session = None;
        purge_disk_cache();
    }

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
        if self.session.is_some() {
            // Frame start: a navigation keypress changes `current` before
            // events drain and textures upload, so the new image paints in
            // this frame instead of the next one.
            self.handle_nav_keys();
        }
        self.drain_events();
        self.manage_textures();
        if self.session.is_none() {
            ui.centered_and_justified(|u| {
                u.label("Open a folder of raws with Cmd+O");
            });
            self.handle_nav_keys();
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
        let file_name = &self
            .session
            .as_ref()
            .expect("the toolbar is only shown for an open session")
            .entries[self.current]
            .file_name;
        let title = top_bar_title(self.mode, self.visible_pos(), self.visible.len(), file_name);
        egui::Panel::top("status").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(title);
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
                        if let Some(captured) = &m.captured {
                            ui.label("Captured");
                            ui.label(captured.to_string());
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

        let loupe_rect = if let Some(session) = &self.session {
            let items = image_info::build_items(
                &self.config.image_info,
                &session.entries[self.current],
                session.metas.get(&self.current),
            );
            image_info::reserve_loupe(ui, self.config.image_info.position, &items).loupe_rect()
        } else {
            ui.available_rect_before_wrap()
        };
        self.last_loupe_rect = Some(loupe_rect);

        // Loupe. The zoom state lives in full-res "logical" space so the
        // same framing holds no matter which tier backs the texture:
        // full = exact, browse = half-res (×2), thumb = stand-in drawn at
        // the retained framing (blurry→sharp in place, no flash-to-fit).
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
        let full_was_urgent = full_resolution_is_urgent(self.mode, self.zoom);
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
        if full_was_urgent != full_resolution_is_urgent(self.mode, self.zoom) {
            self.replan();
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
        if open_loupe {
            self.mode = Mode::Loupe;
        }
        if let Some(i) = clicked {
            let selection_changed = i != self.current;
            self.select(i);
            if open_loupe && !selection_changed {
                self.replan();
            }
        }
    }
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

    #[test]
    fn full_resolution_urgency_requires_a_zoomed_loupe() {
        let anchored = Zoom::Anchored {
            scale: 1.0,
            center: egui::vec2(0.5, 0.5),
        };
        assert!(!full_resolution_is_urgent(Mode::Loupe, Zoom::Fit));
        assert!(full_resolution_is_urgent(Mode::Loupe, anchored));
        assert!(!full_resolution_is_urgent(Mode::Grid, Zoom::Fit));
        assert!(!full_resolution_is_urgent(Mode::Grid, anchored));
    }

    #[test]
    fn speculative_full_failures_do_not_replace_visible_status() {
        let anchored = Zoom::Anchored {
            scale: 1.0,
            center: egui::vec2(0.5, 0.5),
        };
        assert!(image_failure_is_visible(
            7,
            Mode::Loupe,
            Zoom::Fit,
            7,
            Tier::Browse
        ));
        assert!(!image_failure_is_visible(
            7,
            Mode::Loupe,
            Zoom::Fit,
            7,
            Tier::Full
        ));
        assert!(image_failure_is_visible(
            7,
            Mode::Loupe,
            anchored,
            7,
            Tier::Full
        ));
        assert!(!image_failure_is_visible(
            7,
            Mode::Loupe,
            anchored,
            8,
            Tier::Full
        ));
        assert!(!image_failure_is_visible(
            7,
            Mode::Grid,
            anchored,
            7,
            Tier::Full
        ));
    }

    #[test]
    fn grid_title_keeps_the_selected_filename_without_duplicating_loupe() {
        assert_eq!(
            top_bar_title(Mode::Grid, 4, 12, "DSC00005.ARW"),
            "5/12  DSC00005.ARW"
        );
        assert_eq!(top_bar_title(Mode::Loupe, 4, 12, "DSC00005.ARW"), "5/12");
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
    fn next_ahead_follows_the_navigation_direction_within_the_visible_sequence() {
        let visible = [2, 5, 9];
        assert_eq!(next_ahead_index(&visible, 1, 1, 5), Some(9));
        assert_eq!(next_ahead_index(&visible, 1, -1, 5), Some(2));
        // Sequence boundaries have no ahead neighbor.
        assert_eq!(next_ahead_index(&visible, 2, 1, 9), None);
        assert_eq!(next_ahead_index(&visible, 0, -1, 2), None);
        assert_eq!(next_ahead_index(&[], 0, 1, 0), None);
        // A duplicate of the current image is not a pre-upload target.
        assert_eq!(next_ahead_index(&[3, 3], 0, 1, 3), None);
    }

    #[test]
    fn full_tile_retention_is_bounded_to_current_plus_the_next_ahead_neighbor() {
        // While zoomed, exactly {current, next_ahead} tiles survive.
        assert!(full_tile_should_be_kept(7, 7, Some(8), true));
        assert!(full_tile_should_be_kept(8, 7, Some(8), true));
        assert!(!full_tile_should_be_kept(6, 7, Some(8), true));
        assert!(!full_tile_should_be_kept(9, 7, Some(8), true));
        // Without an ahead neighbor only the current image is kept.
        assert!(full_tile_should_be_kept(7, 7, None, true));
        assert!(!full_tile_should_be_kept(8, 7, None, true));
        // Fit mode drops every Full tile, as before.
        assert!(!full_tile_should_be_kept(7, 7, Some(8), false));
        assert!(!full_tile_should_be_kept(8, 7, Some(8), false));
    }

    #[test]
    fn configured_ram_budget_includes_all_four_cache_rings() {
        for total in [0, 1, 3, 1_000_000_000, 64_000_000_000, u64::MAX] {
            let budgets = ram_cache_budgets(total);
            assert_eq!(
                budgets.thumb_rgba_bytes
                    + budgets.browse_rgba_bytes
                    + budgets.full_rgba_bytes
                    + budgets.jpeg_bytes,
                total
            );
            assert!(budgets.thumb_rgba_bytes <= THUMB_BUDGET);
            assert!(budgets.thumb_rgba_bytes <= total / 2);
            assert!(budgets.full_rgba_bytes >= budgets.browse_rgba_bytes);
            assert!(budgets.full_rgba_bytes >= budgets.jpeg_bytes);
        }

        let budgets = ram_cache_budgets(1_000_000_000);
        assert_eq!(budgets.thumb_rgba_bytes, THUMB_BUDGET);
        assert_eq!(
            budgets.browse_rgba_bytes + budgets.full_rgba_bytes + budgets.jpeg_bytes,
            1_000_000_000 - THUMB_BUDGET
        );
        assert!(
            budgets
                .full_rgba_bytes
                .abs_diff((1_000_000_000 - THUMB_BUDGET) / 5 * 3)
                <= 3
        );

        let default_budgets = ram_cache_budgets(4_500_000_000);
        assert_eq!(default_budgets.thumb_rgba_bytes, 402_653_184);
        assert_eq!(default_budgets.full_rgba_bytes, 2_458_408_090);
        assert_eq!(default_budgets.browse_rgba_bytes, 819_469_363);
        assert_eq!(default_budgets.jpeg_bytes, 819_469_363);
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
