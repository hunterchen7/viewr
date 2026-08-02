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
//! use std::collections::HashMap;
//! use viewr_core::planning::{
//!     BrowsePrefetchBudget, FullPrefetchBudget, NavigationPrefetchBudgets, PlanKind,
//!     build_plan_targets_with_full_prefetch,
//! };
//! use viewr_core::types::Tier;
//!
//! let budgets = NavigationPrefetchBudgets::new(
//!     FullPrefetchBudget::new(700, 100, HashMap::new()),
//!     BrowsePrefetchBudget::new(3_300, 100, HashMap::new()),
//! );
//! let targets = build_plan_targets_with_full_prefetch(
//!     100,
//!     50,
//!     1,
//!     false,
//!     &[],
//!     &budgets,
//!     false,
//! );
//! assert!(targets.iter().any(|target| {
//!     target.index == 50
//!         && target.tier == Tier::Browse
//!         && target.kind == PlanKind::Display
//! }));
//! // Fit mode requires current and adjacent Full renders. Optional Full work
//! // then grows toward the byte budget.
//! let full_indices: Vec<_> = targets
//!     .iter()
//!     .filter(|target| target.tier == Tier::Full)
//!     .map(|target| target.index)
//!     .collect();
//! assert_eq!(full_indices, [50, 49, 51, 52, 53, 54, 55]);
//! assert!(targets.iter().any(|target| target.kind == PlanKind::Prefetch));
//! ```
#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

mod atomic_write;
mod jpeg_restart;

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
