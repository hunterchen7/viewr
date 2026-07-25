//! Core RAW decoding, scheduling, caching, metadata, and rating persistence for
//! `viewr`.
//!
//! The crate keeps UI concerns out of the image pipeline. [`jobs::Engine`]
//! schedules background work, workers publish display-ready [`types::PixelBuf`]
//! values through [`cache_ram::RamCache`], and the UI converts those buffers to
//! graphics textures on its own thread. Developed images can also pass through
//! [`cache_disk::DiskCache`] so a cache hit can avoid repeating RAW work.
//!
//! Ratings use Lightroom-compatible XMP sidecars. When a platform database path
//! is configured, [`library::Library`] treats SQLite as the publication
//! authority and journals a new rating before it debounces the sidecar write.
//! It does not bypass an unavailable configured database. A successful dirty
//! journal entry lets a later launch resume an interrupted sidecar write.
//! Systems without a platform configuration directory use explicit
//! database-free XMP persistence. Cache and persistence I/O are best-effort
//! where their APIs document suppressed errors.
//!
//! # Planning example
//!
//! Navigation planning is pure and can be inspected independently of the worker
//! pool:
//!
//! ```
//! use viewr_core::planning::{PlanKind, build_plan_targets};
//! use viewr_core::types::Tier;
//!
//! let targets = build_plan_targets(100, 50, 1, false, &[], false);
//! assert!(targets.iter().any(|target| {
//!     target.index == 50
//!         && target.tier == Tier::Browse
//!         && target.kind == PlanKind::Display
//! }));
//! // Fit mode does not spend CPU or memory on full-resolution development.
//! assert!(targets.iter().all(|target| target.tier != Tier::Full));
//! ```
#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

mod atomic_write;

pub mod cache_disk;
pub mod cache_ram;
pub mod db;
pub mod decode;
pub mod develop;
pub mod folder;
pub mod jobs;
pub mod library;
pub mod meta;
pub mod planning;
pub mod resize;
pub mod types;
pub mod xmp;
