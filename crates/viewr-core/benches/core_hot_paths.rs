use std::hint::black_box;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use viewr_core::cache_disk::DiskCache;
use viewr_core::cache_ram::{RamCache, RamCacheBudgets};
use viewr_core::db::{
    Db, benchmark_insert_rating, benchmark_pending_sidecars, benchmark_rating_cardinalities,
    benchmark_rating_lookup,
};
use viewr_core::decode;
use viewr_core::develop::{self, Quality};
use viewr_core::folder::{FolderEntry, benchmark_sidecar_owner_keys, outward_order};
use viewr_core::jobs::{
    BenchmarkMetadataQueue, BenchmarkNavigationQueue, benchmark_decode_jpeg_serial,
    benchmark_encode_jpeg_plain, benchmark_jpeg_quality, decode_jpeg, encode_jpeg,
};
use viewr_core::library::{benchmark_load_ratings_legacy_full_scan, try_load_ratings_with_owners};
use viewr_core::planning::{
    BrowsePrefetchBudget, FullPrefetchBudget, NavigationPrefetchBudgets, build_plan_targets,
    build_plan_targets_with_full_prefetch,
};
use viewr_core::resize::{apply_orient, downscale_to_fit, resize_exact};
use viewr_core::types::{Orient, PixelBuf, Tier};
use viewr_core::xmp::{parse_rating, update_rating_xml};

const PHOTO_WIDTH: u32 = 4_032;
const PHOTO_HEIGHT: u32 = 3_024;

/// A deterministic, moderately compressible image with gradients, edges, and
/// fine-grained texture. It is intentionally more photographic than either a
/// flat test card or incompressible random noise.
fn synthetic_photo(width: u32, height: u32) -> PixelBuf {
    let mut rgba = vec![0_u8; width as usize * height as usize * 4];

    for y in 0..height {
        for x in 0..width {
            let hash =
                x.wrapping_mul(0x9e37_79b9).rotate_left(y & 15) ^ y.wrapping_mul(0x85eb_ca6b);
            let texture = ((hash ^ (hash >> 13)) & 0x1f) as u8;
            let gradient = ((x * 173 / width.max(1)) + (y * 61 / height.max(1))) as u8;
            let checker = if ((x / 96) + (y / 96)) & 1 == 0 {
                18
            } else {
                0
            };
            let offset = ((y as usize * width as usize) + x as usize) * 4;
            rgba[offset] = gradient.saturating_add(texture / 2);
            rgba[offset + 1] = gradient
                .wrapping_add((x / 19) as u8)
                .saturating_add(checker);
            rgba[offset + 2] = gradient
                .wrapping_add((y / 17) as u8)
                .saturating_sub(texture / 3);
            rgba[offset + 3] = 255;
        }
    }

    PixelBuf {
        width,
        height,
        rgba,
    }
}

fn bench_outward_order(c: &mut Criterion) {
    let mut group = c.benchmark_group("outward_order");
    group.sample_size(20);
    group.warm_up_time(Duration::from_millis(300));
    group.measurement_time(Duration::from_millis(1_500));

    for len in [1_000_usize, 100_000, 1_000_000] {
        group.throughput(Throughput::Elements(len as u64));
        group.bench_with_input(BenchmarkId::from_parameter(len), &len, |b, &len| {
            b.iter(|| black_box(outward_order(black_box(len), black_box(len / 2))));
        });
    }

    group.finish();
}

fn bench_navigation_plan(c: &mut Criterion) {
    let mut group = c.benchmark_group("navigation_plan");
    group.sample_size(20);
    group.warm_up_time(Duration::from_millis(300));
    group.measurement_time(Duration::from_millis(1_500));
    let adaptive_budgets = NavigationPrefetchBudgets::new(
        FullPrefetchBudget::new(1024 * 1024 * 1024, 128 * 1024 * 1024, Default::default()),
        BrowsePrefetchBudget::new(512 * 1024 * 1024, 32 * 1024 * 1024, Default::default()),
    );

    for len in [100_usize, 1_000, 10_000] {
        group.bench_with_input(BenchmarkId::new("identity", len), &len, |b, &len| {
            b.iter(|| {
                black_box(build_plan_targets(
                    black_box(len),
                    black_box(len / 2),
                    black_box(1),
                    black_box(false),
                    black_box(&[]),
                    black_box(false),
                ))
            });
        });

        group.bench_with_input(
            BenchmarkId::new("adaptive_identity", len),
            &len,
            |b, &len| {
                b.iter(|| {
                    black_box(build_plan_targets_with_full_prefetch(
                        black_box(len),
                        black_box(len / 2),
                        black_box(1),
                        black_box(false),
                        black_box(&[]),
                        black_box(&adaptive_budgets),
                        black_box(false),
                    ))
                });
            },
        );

        // This includes the production heap/index synchronization but keeps
        // decoder threads and filesystem cache probes out of the measurement.
        let queue = BenchmarkNavigationQueue::new(len);
        assert!(
            queue.navigate(len / 2) > 0,
            "navigation benchmark must install production queue work"
        );
        let mut queue_current = len / 2;
        group.bench_with_input(
            BenchmarkId::new("production_queue_sync", len),
            &len,
            |b, &len| {
                b.iter(|| {
                    queue_current = (queue_current + 1) % len;
                    black_box(queue.navigate(black_box(queue_current)))
                });
            },
        );

        let fixed_queue = BenchmarkNavigationQueue::new(len);
        let mut fixed_current = len / 2;
        group.bench_with_input(
            BenchmarkId::new("fixed_queue_sync_reference", len),
            &len,
            |b, &len| {
                b.iter(|| {
                    fixed_current = (fixed_current + 1) % len;
                    black_box(fixed_queue.navigate_fixed_reference(black_box(fixed_current)))
                });
            },
        );

        // Retain the former O(N) planner as a reference measurement. The
        // engine no longer calls this path during navigation.
        group.bench_with_input(
            BenchmarkId::new("identity_with_disk_warm", len),
            &len,
            |b, &len| {
                b.iter(|| {
                    black_box(build_plan_targets(
                        black_box(len),
                        black_box(len / 2),
                        black_box(1),
                        black_box(false),
                        black_box(&[]),
                        black_box(true),
                    ))
                });
            },
        );

        let sparse: Vec<usize> = (0..len).step_by(10).collect();
        let current = sparse[sparse.len() / 2];
        group.bench_with_input(
            BenchmarkId::new("ten_percent_filter", len),
            &(current, sparse.clone()),
            |b, (current, sparse)| {
                b.iter(|| {
                    black_box(build_plan_targets(
                        black_box(len),
                        black_box(*current),
                        black_box(-1),
                        black_box(true),
                        black_box(sparse),
                        black_box(false),
                    ))
                });
            },
        );
        group.bench_with_input(
            BenchmarkId::new("adaptive_ten_percent_filter_public", len),
            &(current, sparse.clone()),
            |b, (current, sparse)| {
                b.iter(|| {
                    black_box(build_plan_targets_with_full_prefetch(
                        black_box(len),
                        black_box(*current),
                        black_box(-1),
                        black_box(true),
                        black_box(sparse),
                        black_box(&adaptive_budgets),
                        black_box(false),
                    ))
                });
            },
        );

        let mut filtered_queue = BenchmarkNavigationQueue::new(len);
        filtered_queue.set_sequence(sparse.clone());
        assert!(filtered_queue.navigate(current) > 0);
        let mut filtered_position = sparse.len() / 2;
        group.bench_with_input(
            BenchmarkId::new("adaptive_filtered_queue_sync", len),
            &sparse,
            |b, sparse| {
                b.iter(|| {
                    filtered_position = (filtered_position + 1) % sparse.len();
                    black_box(filtered_queue.navigate(black_box(sparse[filtered_position])))
                });
            },
        );
    }

    group.finish();
}

fn bench_metadata_queue_setup(c: &mut Criterion) {
    let mut group = c.benchmark_group("metadata_queue_setup");
    group.sample_size(20);
    group.warm_up_time(Duration::from_millis(300));
    group.measurement_time(Duration::from_millis(1_500));

    for len in [1_000_usize, 10_000, 100_000] {
        assert_eq!(
            BenchmarkMetadataQueue::new(len).resident_jobs(),
            len,
            "metadata benchmark must retain every production queue item"
        );
        group.throughput(Throughput::Elements(len as u64));
        group.bench_with_input(BenchmarkId::from_parameter(len), &len, |b, &len| {
            b.iter_batched(
                || (),
                |()| BenchmarkMetadataQueue::new(black_box(len)),
                BatchSize::LargeInput,
            );
        });
    }

    group.finish();
}

fn bench_rating_db_lookup(c: &mut Criterion) {
    let mut group = c.benchmark_group("rating_db_lookup");
    group.sample_size(10);
    group.warm_up_time(Duration::from_millis(300));
    group.measurement_time(Duration::from_secs(2));

    for len in [1_000_usize, 10_000, 50_000] {
        let db = Db::open_in_memory().expect("benchmark database opens");
        let paths = (0..len)
            .map(|index| PathBuf::from(format!("/benchmark/photo-{index:08}.arw")))
            .collect::<Vec<_>>();
        for (index, path) in paths.iter().enumerate() {
            benchmark_insert_rating(&db, path, index as u64, index as i64, 4)
                .expect("benchmark row inserts");
        }
        assert_eq!(
            benchmark_rating_cardinalities(&db).expect("benchmark cardinalities"),
            (len, len),
            "lookup corpus must scale both image and owner ledgers"
        );

        group.throughput(Throughput::Elements(len as u64));
        group.bench_with_input(BenchmarkId::from_parameter(len), &len, |b, _| {
            b.iter(|| {
                assert_eq!(
                    black_box(benchmark_rating_lookup(black_box(&db), black_box(&paths))),
                    len
                );
            });
        });
    }

    group.finish();
}

fn bench_rating_folder_load(c: &mut Criterion) {
    let mut group = c.benchmark_group("rating_folder_load");
    group.sample_size(10);
    group.warm_up_time(Duration::from_millis(300));
    group.measurement_time(Duration::from_millis(1_500));

    for len in [1_000_usize, 10_000, 50_000] {
        let directory = tempfile::tempdir().expect("benchmark RAW directory");
        let canonical =
            std::fs::canonicalize(directory.path()).expect("benchmark directory canonicalizes");
        let first = canonical.join("photo-00000000.ARW");
        std::fs::write(&first, b"raw").expect("owner probe RAW placeholder");
        let entries = (0..len)
            .map(|index| FolderEntry {
                path: canonical.join(format!("photo-{index:08}.ARW")),
                file_name: format!("photo-{index:08}.ARW"),
                size: 3,
                mtime_ns: index as i64,
            })
            .collect::<Vec<_>>();
        let db = Db::open_in_memory().expect("benchmark database opens");
        for entry in &entries {
            benchmark_insert_rating(&db, &entry.path, entry.size, entry.mtime_ns, 4)
                .expect("benchmark row inserts");
        }
        assert_eq!(
            benchmark_rating_cardinalities(&db).expect("benchmark cardinalities"),
            (len, len),
            "folder-load corpus must scale both image and owner ledgers"
        );

        group.throughput(Throughput::Elements(len as u64));
        group.bench_with_input(BenchmarkId::new("clean_database", len), &len, |b, _| {
            b.iter(|| {
                let (ratings, owners) =
                    try_load_ratings_with_owners(black_box(&entries), black_box(Some(&db)))
                        .expect("current folder snapshot succeeds");
                assert_eq!(ratings.len(), black_box(len));
                assert_eq!(
                    owners.iter().filter(|owner| owner.is_some()).count(),
                    black_box(len)
                );
            });
        });
    }

    group.finish();
}

#[derive(Clone, Copy)]
enum LegacyRatingSchema {
    DirtyOnly,
    OwnerAware,
}

#[derive(Clone, Copy)]
enum LegacyHistoryShape {
    UniqueClean,
    UniqueDirty,
    RepeatedStemClean,
}

impl LegacyRatingSchema {
    fn label(self) -> &'static str {
        match self {
            Self::DirtyOnly => "legacy_dirty",
            Self::OwnerAware => "legacy_owner",
        }
    }
}

impl LegacyHistoryShape {
    fn label(self) -> &'static str {
        match self {
            Self::UniqueClean => "zero_dirty",
            Self::UniqueDirty => "dense_dirty",
            Self::RepeatedStemClean => "repeated_stem_clean",
        }
    }

    fn is_dirty(self) -> bool {
        matches!(self, Self::UniqueDirty)
    }

    fn raw_path(self, root: &std::path::Path, index: usize) -> PathBuf {
        match self {
            Self::UniqueClean | Self::UniqueDirty => root.join(format!("photo-{index:08}.ARW")),
            Self::RepeatedStemClean => root
                .join(format!("directory-{index:08}"))
                .join("photo-000.ARW"),
        }
    }
}

fn create_legacy_rating_database(
    path: &std::path::Path,
    rows: usize,
    schema: LegacyRatingSchema,
    history_shape: LegacyHistoryShape,
    dirty: Option<(&std::path::Path, u64, i64)>,
) {
    let mut connection = rusqlite::Connection::open(path).expect("legacy benchmark database opens");
    let missing_history = path
        .parent()
        .expect("benchmark database has a parent")
        .join(format!(
            "missing-history-{}-{}-{}",
            schema.label(),
            history_shape.label(),
            rows
        ));
    assert!(
        !missing_history.exists(),
        "legacy history fixture must stay unresolved"
    );
    match schema {
        LegacyRatingSchema::DirtyOnly => connection
            .execute_batch(
                "CREATE TABLE images (
                    path TEXT PRIMARY KEY,
                    size INTEGER NOT NULL,
                    mtime_ns INTEGER NOT NULL,
                    rating INTEGER,
                    sidecar_mtime_ns INTEGER NOT NULL DEFAULT 0,
                    sidecar_dirty INTEGER NOT NULL DEFAULT 0,
                    last_seen INTEGER NOT NULL DEFAULT 0
                );",
            )
            .expect("legacy dirty schema initializes"),
        LegacyRatingSchema::OwnerAware => connection
            .execute_batch(
                "CREATE TABLE images (
                    path TEXT PRIMARY KEY,
                    size INTEGER NOT NULL,
                    mtime_ns INTEGER NOT NULL,
                    rating INTEGER,
                    sidecar_mtime_ns INTEGER NOT NULL DEFAULT 0,
                    sidecar_dirty INTEGER NOT NULL DEFAULT 0,
                    sidecar_quarantined INTEGER NOT NULL DEFAULT 0,
                    sidecar_owner,
                    revision INTEGER NOT NULL DEFAULT 0,
                    last_seen INTEGER NOT NULL DEFAULT 0
                );
                CREATE TABLE viewr_schema_migrations (
                    name TEXT PRIMARY KEY
                ) WITHOUT ROWID;
                INSERT INTO viewr_schema_migrations (name)
                VALUES ('rating-generation-and-owner-v6');",
            )
            .expect("legacy owner schema initializes"),
    }

    let transaction = connection
        .transaction()
        .expect("legacy benchmark transaction begins");
    match schema {
        LegacyRatingSchema::DirtyOnly => {
            let mut insert = transaction
                .prepare(
                    "INSERT INTO images
                        (path, size, mtime_ns, rating, sidecar_mtime_ns,
                         sidecar_dirty, last_seen)
                     VALUES (?1, ?2, ?3, 4, 1, ?4, 0)",
                )
                .expect("legacy dirty insert prepares");
            for index in 0..rows {
                let raw = history_shape.raw_path(&missing_history, index);
                insert
                    .execute(rusqlite::params![
                        raw.to_string_lossy(),
                        index as u64,
                        index as i64,
                        history_shape.is_dirty(),
                    ])
                    .expect("legacy dirty history row inserts");
            }
            drop(insert);
            if let Some((dirty_path, dirty_size, dirty_mtime_ns)) = dirty {
                transaction
                    .execute(
                        "INSERT INTO images
                            (path, size, mtime_ns, rating, sidecar_mtime_ns,
                             sidecar_dirty, last_seen)
                         VALUES (?1, ?2, ?3, 5, 0, 1, 0)",
                        rusqlite::params![
                            dirty_path.to_string_lossy(),
                            dirty_size,
                            dirty_mtime_ns,
                        ],
                    )
                    .expect("legacy dirty pending row inserts");
            }
        }
        LegacyRatingSchema::OwnerAware => {
            let mut insert = transaction
                .prepare(
                    "INSERT INTO images
                        (path, size, mtime_ns, rating, sidecar_mtime_ns,
                         sidecar_dirty, sidecar_quarantined, sidecar_owner,
                         revision, last_seen)
                     VALUES (?1, ?2, ?3, 4, 1, ?4, 0, ?5, 1, 0)",
                )
                .expect("legacy owner insert prepares");
            for index in 0..rows {
                let raw = history_shape.raw_path(&missing_history, index);
                insert
                    .execute(rusqlite::params![
                        raw.to_string_lossy(),
                        index as u64,
                        index as i64,
                        history_shape.is_dirty(),
                        raw.with_extension("xmp").to_string_lossy(),
                    ])
                    .expect("legacy owner history row inserts");
            }
            drop(insert);
            if let Some((dirty_path, dirty_size, dirty_mtime_ns)) = dirty {
                transaction
                    .execute(
                        "INSERT INTO images
                            (path, size, mtime_ns, rating, sidecar_mtime_ns,
                             sidecar_dirty, sidecar_quarantined, sidecar_owner,
                             revision, last_seen)
                         VALUES (?1, ?2, ?3, 5, 0, 1, 0, ?4, 1, 0)",
                        rusqlite::params![
                            dirty_path.to_string_lossy(),
                            dirty_size,
                            dirty_mtime_ns,
                            dirty_path.with_extension("xmp").to_string_lossy(),
                        ],
                    )
                    .expect("legacy owner pending row inserts");
            }
        }
    }
    transaction
        .commit()
        .expect("legacy benchmark transaction commits");
    if matches!(schema, LegacyRatingSchema::OwnerAware) {
        connection
            .execute_batch(
                "CREATE UNIQUE INDEX images_sidecar_owners
                    ON images(sidecar_owner)
                 WHERE sidecar_owner IS NOT NULL;",
            )
            .expect("legacy owner index initializes");
    }
}

fn bench_legacy_rating_folder_load(c: &mut Criterion) {
    const FOLDER_LEN: usize = 100;
    let mut group = c.benchmark_group("rating_legacy_folder_load");
    group.sample_size(10);
    group.warm_up_time(Duration::from_millis(300));
    group.measurement_time(Duration::from_millis(1_500));

    for len in [1_000_usize, 10_000, 50_000] {
        let directory = tempfile::tempdir().expect("legacy benchmark directory");
        let physical = directory.path().join("physical");
        std::fs::create_dir(&physical).expect("legacy benchmark RAW directory initializes");
        let physical =
            std::fs::canonicalize(physical).expect("legacy benchmark directory canonicalizes");
        let mut entries = Vec::with_capacity(FOLDER_LEN);
        for index in 0..FOLDER_LEN {
            let file_name = format!("photo-{index:03}.ARW");
            let path = physical.join(&file_name);
            std::fs::write(&path, b"raw").expect("legacy benchmark RAW placeholder");
            let metadata = std::fs::metadata(&path).expect("legacy benchmark RAW metadata");
            let mtime_ns = metadata
                .modified()
                .expect("legacy benchmark RAW mtime")
                .duration_since(std::time::UNIX_EPOCH)
                .expect("legacy benchmark RAW mtime after epoch")
                .as_nanos() as i64;
            entries.push(FolderEntry {
                path,
                file_name,
                size: metadata.len(),
                mtime_ns,
            });
        }
        let dirty = &entries[0];
        let dirty_path = dirty.path.clone();

        for schema in [
            LegacyRatingSchema::DirtyOnly,
            LegacyRatingSchema::OwnerAware,
        ] {
            let database_path = directory
                .path()
                .join(format!("{}-{len}.db", schema.label()));
            create_legacy_rating_database(
                &database_path,
                len,
                schema,
                LegacyHistoryShape::UniqueClean,
                Some((&dirty_path, dirty.size, dirty.mtime_ns)),
            );
            let probe = Db::try_open_for_read(&database_path)
                .expect("legacy benchmark database opens read-only")
                .expect("legacy benchmark schema is read-compatible");
            let (ratings, _) = try_load_ratings_with_owners(&entries, Some(&probe))
                .expect("targeted legacy preflight succeeds");
            assert_eq!(
                ratings.get(&0),
                Some(&5),
                "the matching physical dirty row must remain authoritative"
            );
            let (reference_ratings, reference_owners) =
                benchmark_load_ratings_legacy_full_scan(&entries, &probe)
                    .expect("full legacy preflight succeeds");
            assert_eq!(
                ratings, reference_ratings,
                "targeted legacy reads must preserve full-scan decisions"
            );
            assert_eq!(reference_owners.len(), entries.len());
            drop(probe);

            group.bench_with_input(
                BenchmarkId::new(schema.label(), len),
                &database_path,
                |b, database_path| {
                    b.iter(|| {
                        let db = Db::try_open_for_read(black_box(database_path.as_path()))
                            .expect("legacy benchmark database opens read-only")
                            .expect("legacy benchmark schema is read-compatible");
                        let (ratings, owners) =
                            try_load_ratings_with_owners(black_box(&entries), Some(&db))
                                .expect("targeted legacy snapshot succeeds");
                        assert_eq!(ratings.get(&0), Some(&5));
                        assert_eq!(owners.len(), FOLDER_LEN);
                    });
                },
            );
            group.bench_with_input(
                BenchmarkId::new(format!("{}_full_scan_reference", schema.label()), len),
                &database_path,
                |b, database_path| {
                    b.iter(|| {
                        let db = Db::try_open_for_read(black_box(database_path.as_path()))
                            .expect("legacy benchmark database opens read-only")
                            .expect("legacy benchmark schema is read-compatible");
                        let (ratings, owners) =
                            benchmark_load_ratings_legacy_full_scan(black_box(&entries), &db)
                                .expect("full legacy snapshot succeeds");
                        assert_eq!(ratings.get(&0), Some(&5));
                        assert_eq!(owners.len(), FOLDER_LEN);
                    });
                },
            );
        }
    }

    group.finish();
}

fn bench_legacy_rating_stress(c: &mut Criterion) {
    let mut group = c.benchmark_group("rating_legacy_stress");
    group.sample_size(10);
    group.warm_up_time(Duration::from_millis(300));
    group.measurement_time(Duration::from_millis(1_500));

    for len in [1_000_usize, 10_000] {
        let directory = tempfile::tempdir().expect("legacy stress directory");
        let physical = directory.path().join("physical");
        std::fs::create_dir(&physical).expect("legacy stress RAW directory initializes");
        let path = physical.join("photo-000.ARW");
        std::fs::write(&path, b"raw").expect("legacy stress RAW placeholder");
        let metadata = std::fs::metadata(&path).expect("legacy stress RAW metadata");
        let mtime_ns = metadata
            .modified()
            .expect("legacy stress RAW mtime")
            .duration_since(std::time::UNIX_EPOCH)
            .expect("legacy stress RAW mtime after epoch")
            .as_nanos() as i64;
        let entries = vec![FolderEntry {
            file_name: "photo-000.ARW".to_owned(),
            path,
            size: metadata.len(),
            mtime_ns,
        }];

        for history_shape in [
            LegacyHistoryShape::UniqueClean,
            LegacyHistoryShape::UniqueDirty,
            LegacyHistoryShape::RepeatedStemClean,
        ] {
            for schema in [
                LegacyRatingSchema::DirtyOnly,
                LegacyRatingSchema::OwnerAware,
            ] {
                let database_path = directory.path().join(format!(
                    "stress-{}-{}-{len}.db",
                    schema.label(),
                    history_shape.label(),
                ));
                create_legacy_rating_database(&database_path, len, schema, history_shape, None);
                let probe = Db::try_open_for_read(&database_path)
                    .expect("legacy stress database opens read-only")
                    .expect("legacy stress schema is read-compatible");
                let targeted = try_load_ratings_with_owners(&entries, Some(&probe))
                    .expect("targeted legacy stress snapshot succeeds");
                let reference = benchmark_load_ratings_legacy_full_scan(&entries, &probe)
                    .expect("full legacy stress snapshot succeeds");
                assert_eq!(
                    targeted, reference,
                    "stress corpus must preserve full-scan decisions"
                );
                assert!(targeted.0.is_empty());
                drop(probe);

                group.bench_with_input(
                    BenchmarkId::new(format!("{}_{}", schema.label(), history_shape.label()), len),
                    &database_path,
                    |b, database_path| {
                        b.iter(|| {
                            let db = Db::try_open_for_read(black_box(database_path.as_path()))
                                .expect("legacy stress database opens read-only")
                                .expect("legacy stress schema is read-compatible");
                            let (ratings, owners) =
                                try_load_ratings_with_owners(black_box(&entries), Some(&db))
                                    .expect("targeted legacy stress snapshot succeeds");
                            assert!(ratings.is_empty());
                            assert_eq!(owners.len(), entries.len());
                        });
                    },
                );
            }
        }
    }

    group.finish();
}

fn bench_rating_db_reopen(c: &mut Criterion) {
    let mut group = c.benchmark_group("rating_db_reopen");
    group.sample_size(20);
    group.warm_up_time(Duration::from_millis(300));
    group.measurement_time(Duration::from_millis(1_500));

    for len in [1_000_usize, 10_000, 50_000] {
        let directory = tempfile::tempdir().expect("benchmark database directory");
        let database_path = directory.path().join("viewr.db");
        let db = Db::open(&database_path).expect("benchmark database opens");
        for index in 0..len {
            benchmark_insert_rating(
                &db,
                &PathBuf::from(format!("/benchmark/photo-{index:08}.arw")),
                index as u64,
                index as i64,
                4,
            )
            .expect("benchmark row inserts");
        }
        assert_eq!(
            benchmark_rating_cardinalities(&db).expect("benchmark cardinalities"),
            (len, len),
            "reopen corpus must scale both image and owner ledgers"
        );
        drop(db);

        let probe = Db::open(&database_path).expect("populated benchmark database reopens");
        assert!(
            probe
                .get_image(&format!("/benchmark/photo-{:08}.arw", len - 1))
                .is_some(),
            "reopen benchmark must retain its populated database"
        );
        drop(probe);

        group.bench_with_input(
            BenchmarkId::new("warm", len),
            &database_path,
            |b, database_path| {
                b.iter(|| {
                    black_box(
                        Db::open(black_box(database_path.as_path()))
                            .expect("benchmark database reopens"),
                    )
                });
            },
        );
        group.bench_with_input(
            BenchmarkId::new("read_only_current", len),
            &database_path,
            |b, database_path| {
                b.iter(|| {
                    black_box(
                        Db::try_open_for_read(black_box(database_path.as_path()))
                            .expect("current benchmark database opens read-only")
                            .expect("current benchmark schema is read-compatible"),
                    )
                });
            },
        );
    }

    group.finish();
}

#[derive(Clone, Copy)]
struct V01MigrationExpectations {
    surviving_images: usize,
    owner_ledgers: usize,
    pending_sidecars: usize,
    quarantined_ratings: usize,
}

#[derive(Clone, Copy)]
enum V01Release {
    V010,
    V011,
}

impl V01Release {
    const fn label(self) -> &'static str {
        match self {
            Self::V010 => "v0.1.0",
            Self::V011 => "v0.1.1",
        }
    }

    const fn has_dirty_column(self) -> bool {
        matches!(self, Self::V011)
    }
}

#[derive(Clone, Copy)]
enum V01Corpus {
    MixedOffline,
    OnlineClean,
}

impl V01Corpus {
    const fn label(self) -> &'static str {
        match self {
            Self::MixedOffline => "mixed-offline",
            Self::OnlineClean => "online-clean",
        }
    }
}

fn create_v01_migration_template(
    path: &std::path::Path,
    rows: usize,
    release: V01Release,
    corpus: V01Corpus,
) -> V01MigrationExpectations {
    const EXISTING_STRIDE: usize = 250;
    const DIRTY_STRIDE: usize = 997;

    let fixture_root = path.parent().expect("v0.1 migration template has a parent");
    let existing_root = fixture_root.join(format!(
        "existing-{}-{}-{rows}",
        release.label(),
        corpus.label()
    ));
    std::fs::create_dir(&existing_root).expect("v0.1 existing RAW directory initializes");
    let existing_root =
        std::fs::canonicalize(existing_root).expect("v0.1 existing RAW directory canonicalizes");
    let missing_root = existing_root
        .parent()
        .expect("v0.1 existing RAW directory has a parent")
        .join(format!(
            "missing-{}-{}-{rows}",
            release.label(),
            corpus.label()
        ));
    assert!(
        !missing_root.exists(),
        "v0.1 missing RAW directory must stay unresolved"
    );

    let mut connection =
        rusqlite::Connection::open(path).expect("v0.1 migration template database opens");
    connection
        .pragma_update(None, "journal_mode", "WAL")
        .expect("released v0.1 template enables WAL");
    let journal_mode = connection
        .pragma_query_value(None, "journal_mode", |row| row.get::<_, String>(0))
        .expect("released v0.1 template journal mode reads");
    assert!(
        journal_mode.eq_ignore_ascii_case("wal"),
        "released v0.1 template must begin in persistent WAL mode"
    );
    connection
        .execute_batch(match release {
            V01Release::V010 => {
                "CREATE TABLE images (
                    path TEXT PRIMARY KEY,
                    size INTEGER NOT NULL,
                    mtime_ns INTEGER NOT NULL,
                    rating INTEGER,
                    sidecar_mtime_ns INTEGER NOT NULL DEFAULT 0,
                    last_seen INTEGER NOT NULL DEFAULT 0
                );"
            }
            V01Release::V011 => {
                "CREATE TABLE images (
                    path TEXT PRIMARY KEY,
                    size INTEGER NOT NULL,
                    mtime_ns INTEGER NOT NULL,
                    rating INTEGER,
                    sidecar_mtime_ns INTEGER NOT NULL DEFAULT 0,
                    sidecar_dirty INTEGER NOT NULL DEFAULT 0,
                    last_seen INTEGER NOT NULL DEFAULT 0
                );"
            }
        })
        .expect("released v0.1 rating schema initializes");

    let transaction = connection
        .transaction()
        .expect("v0.1 migration template transaction begins");
    let mut insert = transaction
        .prepare(match release {
            V01Release::V010 => {
                "INSERT INTO images
                    (path, size, mtime_ns, rating, sidecar_mtime_ns, last_seen)
                 VALUES (?1, ?2, ?3, 4, 1, 0)"
            }
            V01Release::V011 => {
                "INSERT INTO images
                    (path, size, mtime_ns, rating, sidecar_mtime_ns,
                     sidecar_dirty, last_seen)
                 VALUES (?1, ?2, ?3, 4, 1, ?4, 0)"
            }
        })
        .expect("v0.1 migration template insert prepares");
    let mut existing_dirty = 0_usize;
    let mut missing_dirty = 0_usize;
    let mut existing_rows = 0_usize;
    for index in 0..rows {
        let exists = matches!(corpus, V01Corpus::OnlineClean) || index % EXISTING_STRIDE == 0;
        let dirty = matches!(corpus, V01Corpus::MixedOffline)
            && release.has_dirty_column()
            && index % DIRTY_STRIDE == 0;
        let raw = if exists {
            existing_rows += 1;
            let raw = existing_root.join(format!("photo-{index:08}.ARW"));
            std::fs::write(&raw, b"raw").expect("v0.1 existing RAW placeholder writes");
            raw
        } else {
            missing_root.join(format!("photo-{index:08}.ARW"))
        };
        let (size, mtime_ns) = if exists {
            let metadata = std::fs::metadata(&raw).expect("v0.1 existing RAW metadata reads");
            let mtime_ns = metadata
                .modified()
                .expect("v0.1 existing RAW mtime reads")
                .duration_since(std::time::UNIX_EPOCH)
                .expect("v0.1 existing RAW mtime follows epoch")
                .as_nanos() as i64;
            (metadata.len(), mtime_ns)
        } else {
            (index as u64 + 1, index as i64 + 1)
        };
        let raw = raw.to_str().expect("v0.1 benchmark path is UTF-8");
        match release {
            V01Release::V010 => insert.execute(rusqlite::params![raw, size, mtime_ns]),
            V01Release::V011 => insert.execute(rusqlite::params![raw, size, mtime_ns, dirty]),
        }
        .expect("v0.1 migration template row inserts");
        if dirty {
            if exists {
                existing_dirty += 1;
            } else {
                missing_dirty += 1;
            }
        }
    }
    drop(insert);
    transaction
        .commit()
        .expect("v0.1 migration template transaction commits");
    drop(connection);

    if matches!(
        (release, corpus),
        (V01Release::V011, V01Corpus::MixedOffline)
    ) {
        assert!(
            existing_dirty > 0 && missing_dirty > 0,
            "v0.1.1 mixed corpus must include recoverable and quarantined dirty rows"
        );
    }
    V01MigrationExpectations {
        surviving_images: rows - missing_dirty,
        owner_ledgers: existing_rows,
        pending_sidecars: existing_dirty,
        quarantined_ratings: missing_dirty,
    }
}

fn bench_rating_db_cold_released_migrations(c: &mut Criterion) {
    let mut group = c.benchmark_group("rating_db_cold_released_migrations");
    group.sample_size(10);
    group.warm_up_time(Duration::from_millis(300));
    group.measurement_time(Duration::from_millis(1_500));

    for release in [V01Release::V010, V01Release::V011] {
        for corpus in [V01Corpus::MixedOffline, V01Corpus::OnlineClean] {
            let row_counts: &[usize] = match corpus {
                V01Corpus::MixedOffline => &[1_000, 10_000],
                V01Corpus::OnlineClean => &[1_000],
            };
            for &rows in row_counts {
                let template_directory = tempfile::tempdir().expect("v0.1 template directory");
                let template_path = template_directory.path().join(format!(
                    "viewr-{}-{}.db",
                    release.label(),
                    corpus.label()
                ));
                let expected = create_v01_migration_template(&template_path, rows, release, corpus);

                let preflight_directory = tempfile::tempdir().expect("v0.1 preflight directory");
                let preflight_path = preflight_directory.path().join("viewr.db");
                std::fs::copy(&template_path, &preflight_path)
                    .expect("v0.1 migration preflight template copies");
                let preflight =
                    Db::open(&preflight_path).expect("v0.1 migration preflight succeeds");
                assert_eq!(
                    benchmark_rating_cardinalities(&preflight)
                        .expect("v0.1 migration cardinalities read"),
                    (expected.surviving_images, expected.owner_ledgers),
                    "released migration must retain the expected history and unfinished work"
                );
                assert_eq!(
                    preflight
                        .pending_sidecars()
                        .expect("v0.1 migrated pending sidecars read")
                        .len(),
                    expected.pending_sidecars,
                    "released migration must retain only recoverable unfinished work"
                );
                drop(preflight);
                let quarantine = rusqlite::Connection::open(&preflight_path)
                    .expect("v0.1 migrated preflight database reopens")
                    .query_row(
                        "SELECT COUNT(*) FROM quarantined_legacy_ratings",
                        [],
                        |row| row.get::<_, usize>(0),
                    )
                    .expect("v0.1 migration quarantine cardinality reads");
                assert_eq!(
                    quarantine, expected.quarantined_ratings,
                    "released migration must archive every unresolved unfinished row"
                );

                group.bench_with_input(
                    BenchmarkId::new(format!("{}-{}", release.label(), corpus.label()), rows),
                    &rows,
                    |b, _| {
                        b.iter_batched(
                            || {
                                let directory = tempfile::tempdir()
                                    .expect("v0.1 migration iteration directory");
                                let database_path = directory.path().join("viewr.db");
                                std::fs::copy(&template_path, &database_path)
                                    .expect("v0.1 migration iteration template copies");
                                (directory, database_path)
                            },
                            |(directory, database_path)| {
                                let db = Db::open(black_box(&database_path))
                                    .expect("v0.1 migration succeeds");
                                black_box((db, directory))
                            },
                            BatchSize::SmallInput,
                        );
                    },
                );
            }
        }
    }

    group.finish();
}

#[derive(Clone, Copy)]
enum V7Corpus {
    Unresolved,
    OnlineOwned,
}

impl V7Corpus {
    const fn label(self) -> &'static str {
        match self {
            Self::Unresolved => "unresolved-removal",
            Self::OnlineOwned => "online-owned",
        }
    }
}

fn create_v7_migration_template(path: &std::path::Path, rows: usize, corpus: V7Corpus) {
    let db = Db::open(path).expect("migration template database opens");
    let fixture_root = path.parent().expect("migration template has a parent");
    let unresolved_root = fixture_root.join("unresolved-v7");
    let mut online_root = fixture_root.join("online-v7");
    if matches!(corpus, V7Corpus::OnlineOwned) {
        std::fs::create_dir(&online_root).expect("online v7 fixture directory initializes");
        online_root =
            std::fs::canonicalize(online_root).expect("online v7 fixture directory canonicalizes");
    } else {
        assert!(
            !unresolved_root.exists(),
            "migration fixture paths must remain unresolved"
        );
    }
    for index in 0..rows {
        let raw = match corpus {
            V7Corpus::Unresolved => unresolved_root.join(format!("photo-{index:08}.ARW")),
            V7Corpus::OnlineOwned => {
                let raw = online_root.join(format!("photo-{index:08}.ARW"));
                std::fs::write(&raw, b"raw").expect("online v7 RAW placeholder writes");
                raw
            }
        };
        let (size, mtime_ns) = if matches!(corpus, V7Corpus::OnlineOwned) {
            let metadata = std::fs::metadata(&raw).expect("online v7 RAW metadata reads");
            let mtime_ns = metadata
                .modified()
                .expect("online v7 RAW mtime reads")
                .duration_since(std::time::UNIX_EPOCH)
                .expect("online v7 RAW mtime follows epoch")
                .as_nanos() as i64;
            (metadata.len(), mtime_ns)
        } else {
            (index as u64, index as i64)
        };
        benchmark_insert_rating(&db, &raw, size, mtime_ns, 4)
            .expect("migration template row inserts");
    }
    drop(db);

    let connection = rusqlite::Connection::open(path).expect("migration template database reopens");
    connection
        .execute_batch(
            "DELETE FROM viewr_schema_migrations
             WHERE name = 'sidecar-owner-filesystem-identity-v8';
             DROP TRIGGER images_reject_legacy_owner_insert;
             DROP TRIGGER images_reject_legacy_rating_update;
             DROP TRIGGER images_reject_unowned_dirty_insert;
             DROP TRIGGER images_reject_unowned_dirty_update;
             ALTER TABLE images DROP COLUMN owner_key_version;
             ALTER TABLE rating_global_revision DROP COLUMN ownerless_revision;
             CREATE TRIGGER images_reject_unowned_dirty_insert
             BEFORE INSERT ON images
             WHEN NEW.sidecar_dirty = 1 AND NEW.sidecar_owner IS NULL
             BEGIN
                 SELECT RAISE(ABORT, 'dirty rating requires a sidecar owner');
             END;
             CREATE TRIGGER images_reject_unowned_dirty_update
             BEFORE UPDATE OF sidecar_dirty, sidecar_owner ON images
             WHEN NEW.sidecar_dirty = 1 AND NEW.sidecar_owner IS NULL
             BEGIN
                 SELECT RAISE(ABORT, 'dirty rating requires a sidecar owner');
             END;
             PRAGMA wal_checkpoint(TRUNCATE);",
        )
        .expect("migration template is downgraded to the exact v7 column shape");
}

fn bench_rating_db_cold_v7_migration(c: &mut Criterion) {
    let mut group = c.benchmark_group("rating_db_cold_v7_migration");
    group.sample_size(10);
    group.warm_up_time(Duration::from_millis(300));
    group.measurement_time(Duration::from_millis(1_500));

    for corpus in [V7Corpus::Unresolved, V7Corpus::OnlineOwned] {
        let row_counts: &[usize] = match corpus {
            V7Corpus::Unresolved => &[1_000, 10_000],
            V7Corpus::OnlineOwned => &[1_000],
        };
        for &rows in row_counts {
            let template_directory = tempfile::tempdir().expect("migration template directory");
            let template_path = template_directory
                .path()
                .join(format!("viewr-v7-{}.db", corpus.label()));
            create_v7_migration_template(&template_path, rows, corpus);

            let preflight_directory = tempfile::tempdir().expect("migration preflight directory");
            let preflight_path = preflight_directory.path().join("viewr.db");
            std::fs::copy(&template_path, &preflight_path)
                .expect("migration preflight template copies");
            let preflight = Db::open(&preflight_path).expect("v7 migration preflight succeeds");
            let expected_images = match corpus {
                V7Corpus::Unresolved => 0,
                V7Corpus::OnlineOwned => rows,
            };
            assert_eq!(
                benchmark_rating_cardinalities(&preflight).expect("migration cardinalities"),
                (expected_images, rows),
                "v7 migration must preserve online rows and retain ordering tombstones"
            );
            drop(preflight);

            group.bench_with_input(BenchmarkId::new(corpus.label(), rows), &rows, |b, _| {
                b.iter_batched(
                    || {
                        let directory = tempfile::tempdir().expect("migration iteration directory");
                        let database_path = directory.path().join("viewr.db");
                        std::fs::copy(&template_path, &database_path)
                            .expect("migration iteration template copies");
                        (directory, database_path)
                    },
                    |(directory, database_path)| {
                        let db =
                            Db::open(black_box(&database_path)).expect("v7 migration succeeds");
                        black_box((db, directory))
                    },
                    BatchSize::SmallInput,
                );
            });
        }
    }

    group.finish();
}

fn bench_rating_db_journal(c: &mut Criterion) {
    let mut group = c.benchmark_group("rating_db_journal");
    group.sample_size(10);
    group.warm_up_time(Duration::from_millis(300));
    group.measurement_time(Duration::from_millis(1_500));

    for len in [1_000_usize, 10_000, 50_000] {
        let directory = tempfile::tempdir().expect("benchmark RAW directory");
        let raw = directory.path().join("target.ARW");
        std::fs::write(&raw, b"raw").expect("benchmark RAW placeholder");
        let metadata = std::fs::metadata(&raw).expect("benchmark RAW metadata");
        let mtime_ns = metadata
            .modified()
            .expect("benchmark RAW mtime")
            .duration_since(std::time::UNIX_EPOCH)
            .expect("benchmark RAW mtime after epoch")
            .as_nanos() as i64;
        let db = Db::open_in_memory().expect("benchmark database opens");
        for index in 0..len {
            benchmark_insert_rating(
                &db,
                &PathBuf::from(format!("/benchmark/photo-{index:08}.arw")),
                index as u64,
                index as i64,
                4,
            )
            .expect("benchmark row inserts");
        }
        assert_eq!(
            benchmark_rating_cardinalities(&db).expect("benchmark cardinalities"),
            (len, len),
            "journal corpus must scale both image and owner ledgers"
        );
        db.record_rating_pending_sidecar(
            raw.to_str().expect("benchmark path is UTF-8"),
            metadata.len(),
            mtime_ns,
            3,
        )
        .expect("journal target prefills");
        assert_eq!(
            benchmark_rating_cardinalities(&db).expect("prefilled benchmark cardinalities"),
            (len + 1, len + 1),
            "timed journal operation must update an existing image and owner ledger"
        );

        group.bench_with_input(BenchmarkId::from_parameter(len), &len, |b, _| {
            b.iter(|| {
                db.record_rating_pending_sidecar(
                    black_box(raw.to_str().expect("benchmark path is UTF-8")),
                    black_box(metadata.len()),
                    black_box(mtime_ns),
                    black_box(4),
                )
                .expect("canonical journal update");
            });
        });
    }

    group.finish();
}

fn bench_rating_db_pending_scan(c: &mut Criterion) {
    let mut group = c.benchmark_group("rating_db_pending_scan");
    group.sample_size(20);
    group.warm_up_time(Duration::from_millis(300));
    group.measurement_time(Duration::from_millis(1_500));

    for len in [1_000_usize, 10_000, 50_000] {
        let db = Db::open_in_memory().expect("benchmark database opens");
        for index in 0..len {
            benchmark_insert_rating(
                &db,
                &PathBuf::from(format!("/benchmark/photo-{index:08}.arw")),
                index as u64,
                index as i64,
                4,
            )
            .expect("benchmark row inserts");
        }
        assert_eq!(
            benchmark_rating_cardinalities(&db).expect("benchmark cardinalities"),
            (len, len),
            "pending-scan corpus must scale both image and owner ledgers"
        );

        group.bench_with_input(BenchmarkId::new("zero_dirty", len), &len, |b, _| {
            b.iter(|| {
                assert_eq!(
                    black_box(benchmark_pending_sidecars(black_box(&db)).unwrap()),
                    0
                );
            });
        });

        let directory = tempfile::tempdir().expect("benchmark RAW directory");
        let raw = directory.path().join("pending.ARW");
        std::fs::write(&raw, b"raw").expect("benchmark RAW placeholder");
        let metadata = std::fs::metadata(&raw).expect("benchmark RAW metadata");
        let mtime_ns = metadata
            .modified()
            .expect("benchmark RAW mtime")
            .duration_since(std::time::UNIX_EPOCH)
            .expect("benchmark RAW mtime after epoch")
            .as_nanos() as i64;
        db.record_rating_pending_sidecar(
            raw.to_str().expect("benchmark path is UTF-8"),
            metadata.len(),
            mtime_ns,
            5,
        )
        .expect("benchmark pending row inserts");
        group.bench_with_input(BenchmarkId::new("one_dirty", len), &len, |b, _| {
            b.iter(|| {
                assert_eq!(
                    black_box(benchmark_pending_sidecars(black_box(&db)).unwrap()),
                    1
                );
            });
        });
    }

    group.finish();
}

fn bench_sidecar_owner_batch(c: &mut Criterion) {
    let mut group = c.benchmark_group("sidecar_owner_batch");
    group.sample_size(10);
    group.warm_up_time(Duration::from_millis(300));
    group.measurement_time(Duration::from_millis(1_500));

    for len in [1_000_usize, 10_000, 50_000] {
        let directory = tempfile::tempdir().expect("benchmark RAW directory");
        let canonical =
            std::fs::canonicalize(directory.path()).expect("benchmark directory canonicalizes");
        std::fs::write(canonical.join("photo-00000000.ARW"), b"raw")
            .expect("owner probe RAW placeholder");
        let entries = (0..len)
            .map(|index| FolderEntry {
                path: canonical.join(format!("photo-{index:08}.ARW")),
                file_name: format!("photo-{index:08}.ARW"),
                size: 3,
                mtime_ns: 0,
            })
            .collect::<Vec<_>>();

        group.throughput(Throughput::Elements(len as u64));
        group.bench_with_input(BenchmarkId::from_parameter(len), &len, |b, _| {
            b.iter(|| {
                assert_eq!(
                    benchmark_sidecar_owner_keys(black_box(&entries)),
                    black_box(len)
                );
            });
        });
    }

    group.finish();
}

fn bench_unicode_sidecar_owner_batch(c: &mut Criterion) {
    let mut group = c.benchmark_group("sidecar_owner_unicode_batch");
    group.sample_size(10);
    group.warm_up_time(Duration::from_millis(300));
    group.measurement_time(Duration::from_millis(1_500));

    for len in [1_000_usize, 10_000] {
        let directory = tempfile::tempdir().expect("benchmark RAW directory");
        let canonical =
            std::fs::canonicalize(directory.path()).expect("benchmark directory canonicalizes");
        let mut entries = Vec::with_capacity(len);
        for index in 0..len {
            let file_name = format!("caf\u{e9}-{index:08}.ARW");
            let path = canonical.join(&file_name);
            std::fs::write(&path, b"raw").expect("Unicode owner probe RAW placeholder");
            entries.push(FolderEntry {
                path,
                file_name,
                size: 3,
                mtime_ns: 0,
            });
        }

        group.throughput(Throughput::Elements(len as u64));
        group.bench_with_input(BenchmarkId::from_parameter(len), &len, |b, _| {
            b.iter(|| {
                assert_eq!(
                    benchmark_sidecar_owner_keys(black_box(&entries)),
                    black_box(len)
                );
            });
        });
    }

    group.finish();
}

fn bench_resize(c: &mut Criterion) {
    let photo = synthetic_photo(PHOTO_WIDTH, PHOTO_HEIGHT);
    let source_pixels = u64::from(photo.width) * u64::from(photo.height);
    let mut group = c.benchmark_group("resize");
    group.sample_size(10);
    group.warm_up_time(Duration::from_millis(300));
    group.measurement_time(Duration::from_secs(2));
    group.throughput(Throughput::Elements(source_pixels));

    for max_edge in [2_048_u32, 360] {
        group.bench_with_input(
            BenchmarkId::new("downscale_to_fit", max_edge),
            &max_edge,
            |b, &max_edge| {
                b.iter_batched(
                    || photo.clone(),
                    |buf| black_box(downscale_to_fit(black_box(buf), max_edge).unwrap()),
                    BatchSize::PerIteration,
                );
            },
        );
    }

    group.bench_function("resize_exact/1920x1280", |b| {
        b.iter_batched(
            || photo.clone(),
            |buf| black_box(resize_exact(black_box(buf), 1_920, 1_280).unwrap()),
            BatchSize::PerIteration,
        );
    });

    group.finish();
}

fn bench_orientation(c: &mut Criterion) {
    let photo = synthetic_photo(PHOTO_WIDTH, PHOTO_HEIGHT);
    let source_pixels = u64::from(photo.width) * u64::from(photo.height);
    let mut group = c.benchmark_group("orientation");
    group.sample_size(10);
    group.warm_up_time(Duration::from_millis(300));
    group.measurement_time(Duration::from_secs(2));
    group.throughput(Throughput::Elements(source_pixels));

    for (name, orient) in [
        ("r90", Orient::R90),
        ("r180", Orient::R180),
        ("r270", Orient::R270),
    ] {
        group.bench_function(name, |b| {
            b.iter_batched(
                || photo.clone(),
                |buf| black_box(apply_orient(black_box(buf), orient)),
                BatchSize::PerIteration,
            );
        });
    }

    group.finish();

    let full_resolution = synthetic_photo(7_008, 4_672);
    let full_pixels = u64::from(full_resolution.width) * u64::from(full_resolution.height);
    let mut full_group = c.benchmark_group("orientation_33mp");
    full_group.sample_size(10);
    full_group.warm_up_time(Duration::from_millis(300));
    full_group.measurement_time(Duration::from_secs(3));
    full_group.throughput(Throughput::Elements(full_pixels));
    for (name, orient) in [("r90", Orient::R90), ("r270", Orient::R270)] {
        full_group.bench_function(name, |b| {
            b.iter_batched(
                || full_resolution.clone(),
                |buf| black_box(apply_orient(black_box(buf), orient)),
                BatchSize::PerIteration,
            );
        });
    }
    full_group.finish();
}

fn bench_jpeg(c: &mut Criterion) {
    let cases = [
        (
            "browse_8mp",
            synthetic_photo(3_504, 2_336),
            benchmark_jpeg_quality(Tier::Browse),
        ),
        (
            "full_33mp",
            synthetic_photo(7_008, 4_672),
            benchmark_jpeg_quality(Tier::Full),
        ),
    ];

    let mut encode_group = c.benchmark_group("jpeg_encode");
    encode_group.sample_size(10);
    encode_group.warm_up_time(Duration::from_millis(300));
    encode_group.measurement_time(Duration::from_secs(2));
    for (name, photo, quality) in &cases {
        encode_group.throughput(Throughput::Bytes(photo.byte_len() as u64));
        encode_group.bench_function(format!("{name}_q{quality}"), |b| {
            b.iter(|| black_box(encode_jpeg(black_box(photo), *quality).unwrap()));
        });
    }
    encode_group.finish();

    // The former markerless encode stays measurable so the restart-marker
    // cost remains independently comparable on any host.
    let mut plain_group = c.benchmark_group("jpeg_encode_plain");
    plain_group.sample_size(10);
    plain_group.warm_up_time(Duration::from_millis(300));
    plain_group.measurement_time(Duration::from_secs(2));
    for (name, photo, quality) in &cases {
        plain_group.throughput(Throughput::Bytes(photo.byte_len() as u64));
        plain_group.bench_function(format!("{name}_q{quality}"), |b| {
            b.iter(|| black_box(benchmark_encode_jpeg_plain(black_box(photo), *quality).unwrap()));
        });
    }
    plain_group.finish();

    let mut decode_group = c.benchmark_group("jpeg_decode");
    decode_group.sample_size(10);
    decode_group.warm_up_time(Duration::from_millis(300));
    decode_group.measurement_time(Duration::from_secs(2));
    for (name, photo, quality) in &cases {
        // Encoding is deliberately outside the decode timing.
        let encoded = encode_jpeg(photo, *quality).expect("synthetic photo must encode");
        decode_group.throughput(Throughput::Bytes(encoded.len() as u64));
        decode_group.bench_function(format!("{name}_q{quality}"), |b| {
            b.iter(|| black_box(decode_jpeg(black_box(encoded.as_slice())).unwrap()));
        });
    }
    decode_group.finish();

    // The whole-buffer serial decode stays measurable so the restart-marker
    // split remains independently comparable on any host.
    let mut serial_group = c.benchmark_group("jpeg_decode_serial");
    serial_group.sample_size(10);
    serial_group.warm_up_time(Duration::from_millis(300));
    serial_group.measurement_time(Duration::from_secs(2));
    for (name, photo, quality) in &cases {
        let encoded = encode_jpeg(photo, *quality).expect("synthetic photo must encode");
        serial_group.throughput(Throughput::Bytes(encoded.len() as u64));
        serial_group.bench_function(format!("{name}_q{quality}"), |b| {
            b.iter(|| {
                black_box(benchmark_decode_jpeg_serial(black_box(encoded.as_slice())).unwrap())
            });
        });
    }
    serial_group.finish();
}

fn bench_ram_cache(c: &mut Criterion) {
    const RESIDENT_ENTRIES: usize = 32;
    let rgba = Arc::new(synthetic_photo(512, 384));
    let rgba_bytes = rgba.byte_len() as u64;
    let jpeg = Arc::new(encode_jpeg(&rgba, 85).expect("cache fixture must encode"));
    let cache = RamCache::new(RamCacheBudgets::new(
        0,
        rgba_bytes * RESIDENT_ENTRIES as u64,
        0,
        jpeg.len() as u64 * RESIDENT_ENTRIES as u64,
    ));

    for index in 0..RESIDENT_ENTRIES {
        cache.insert_rgba((index, Tier::Browse), Arc::clone(&rgba));
        cache.insert_jpeg((index, Tier::Browse), Arc::clone(&jpeg));
    }

    let mut group = c.benchmark_group("ram_cache");
    group.sample_size(30);
    group.warm_up_time(Duration::from_millis(300));
    group.measurement_time(Duration::from_millis(1_500));
    group.throughput(Throughput::Elements(1));

    let mut rgba_hit = 0_usize;
    group.bench_function("rgba_hit", |b| {
        b.iter(|| {
            let key = (rgba_hit % RESIDENT_ENTRIES, Tier::Browse);
            rgba_hit = rgba_hit.wrapping_add(1);
            black_box(cache.get_rgba(black_box(key))).expect("resident cache entry");
        });
    });

    let mut jpeg_hit = 0_usize;
    group.bench_function("jpeg_hit", |b| {
        b.iter(|| {
            let key = (jpeg_hit % RESIDENT_ENTRIES, Tier::Browse);
            jpeg_hit = jpeg_hit.wrapping_add(1);
            black_box(cache.get_jpeg(black_box(key))).expect("resident cache entry");
        });
    });

    // A navigation replan classifies every plan target by residency. Compare
    // the batched single-lock snapshot against the per-target has_* probes it
    // replaced (kept as a reference).
    let plan_keys: Vec<(usize, Tier)> = (0..200)
        .map(|index| {
            (
                index,
                if index % 2 == 0 {
                    Tier::Browse
                } else {
                    Tier::Full
                },
            )
        })
        .collect();
    group.throughput(Throughput::Elements(plan_keys.len() as u64));
    group.bench_function("plan_probe_batched_200", |b| {
        b.iter(|| black_box(cache.probe_residency(black_box(&plan_keys).iter())));
    });
    group.bench_function("plan_probe_individual_200", |b| {
        b.iter(|| {
            let probes: Vec<(bool, bool)> = plan_keys
                .iter()
                .map(|&key| {
                    (
                        cache.has_rgba(black_box(key)),
                        cache.has_jpeg(black_box(key)),
                    )
                })
                .collect();
            black_box(probes)
        });
    });

    // Reuse the payload and cycle over one bounded synthetic folder so this
    // isolates LRU/hash-map churn rather than allocation or an impossible
    // unbounded stream of new folder indices. Every insert evicts one entry.
    let churn = RamCache::new(RamCacheBudgets::new(0, rgba_bytes * 8, 0, 0));
    for index in 0..8 {
        churn.insert_rgba((index, Tier::Browse), Arc::clone(&rgba));
    }
    let mut next_key = 8_usize;
    group.throughput(Throughput::Elements(1));
    group.bench_function("rgba_insert_with_eviction", |b| {
        b.iter(|| {
            let key = (next_key % 9, Tier::Browse);
            next_key = next_key.wrapping_add(1);
            churn.insert_rgba(black_box(key), Arc::clone(black_box(&rgba)));
        });
    });

    group.finish();
}

fn bench_ram_cache_eviction_scaling(c: &mut Criterion) {
    let payload = Arc::new(PixelBuf {
        width: 1,
        height: 1,
        rgba: vec![0; 4],
    });
    let mut group = c.benchmark_group("ram_cache_eviction_scaling");
    group.sample_size(20);
    group.warm_up_time(Duration::from_millis(300));
    group.measurement_time(Duration::from_millis(1_500));
    group.throughput(Throughput::Elements(1));

    for entries in [8_usize, 128, 1_024, 10_000] {
        let cache = RamCache::new(RamCacheBudgets::new(0, entries as u64 * 4, 0, 0));
        for index in 0..entries {
            cache.insert_rgba((index, Tier::Browse), Arc::clone(&payload));
        }
        let mut next_key = entries;
        group.bench_with_input(
            BenchmarkId::new("one_in_one_out", entries),
            &entries,
            |b, _| {
                b.iter(|| {
                    cache.insert_rgba(
                        black_box((next_key, Tier::Browse)),
                        Arc::clone(black_box(&payload)),
                    );
                    next_key = next_key.wrapping_add(1);
                });
            },
        );
    }

    group.finish();
}

fn bench_full_cache_policy(c: &mut Criterion) {
    let payload = || {
        Arc::new(PixelBuf {
            width: 1,
            height: 1,
            rgba: vec![0; 4],
        })
    };
    let cache = RamCache::new(RamCacheBudgets::new(0, 0, 8 * 4, 0));
    let first: Vec<_> = (0..8).map(|index| (index, Tier::Full)).collect();
    let second: Vec<_> = (8..16).map(|index| (index, Tier::Full)).collect();
    cache.set_navigation_policy([], first.iter().copied());
    for &key in &first {
        cache.insert_rgba(key, payload());
    }

    let snapshot_cache = RamCache::new(RamCacheBudgets::new(0, 0, 10_000 * 4, 0));
    let snapshot_keys: Vec<_> = (0..10_000).map(|index| (index, Tier::Full)).collect();
    snapshot_cache.set_navigation_policy([], snapshot_keys.iter().copied());
    for &key in &snapshot_keys {
        snapshot_cache.insert_rgba((key.0, Tier::Browse), payload());
        snapshot_cache.insert_rgba(key, payload());
    }

    let mut group = c.benchmark_group("full_cache_policy");
    group.sample_size(30);
    group.warm_up_time(Duration::from_millis(300));
    group.measurement_time(Duration::from_millis(1_500));

    let mut use_second = true;
    group.throughput(Throughput::Elements(8));
    group.bench_function("replace_and_refill_eight_unique_entries", |b| {
        b.iter(|| {
            let desired = if use_second { &second } else { &first };
            cache.set_navigation_policy([], desired.iter().copied());
            for &key in desired {
                cache.insert_rgba(black_box(key), black_box(payload()));
            }
            use_second = !use_second;
        });
    });

    const EVICTION_PAYLOAD_BYTES: usize = 16 * 1024 * 1024;
    group.throughput(Throughput::Bytes((8 * EVICTION_PAYLOAD_BYTES) as u64));
    // A working-set change alone no longer evicts (retention is lazy); the
    // stale final owners now drop when refilling the ring crosses its budget.
    group.bench_function("refill_evicts_eight_unique_16mib_final_owners", |b| {
        b.iter_batched(
            || {
                let cache = RamCache::new(RamCacheBudgets::new(
                    0,
                    0,
                    (8 * EVICTION_PAYLOAD_BYTES) as u64,
                    0,
                ));
                let working_set: Vec<_> = (0..16).map(|index| (index, Tier::Full)).collect();
                cache.set_navigation_policy([], working_set);
                for index in 0..8 {
                    cache.insert_rgba(
                        (index, Tier::Full),
                        Arc::new(PixelBuf {
                            width: 1,
                            height: 1,
                            rgba: vec![0; EVICTION_PAYLOAD_BYTES],
                        }),
                    );
                }
                let replacements: Vec<_> = (8..16)
                    .map(|index| {
                        (
                            (index, Tier::Full),
                            Arc::new(PixelBuf {
                                width: 1,
                                height: 1,
                                rgba: vec![0; EVICTION_PAYLOAD_BYTES],
                            }),
                        )
                    })
                    .collect();
                (cache, replacements)
            },
            |(cache, replacements)| {
                for (key, payload) in replacements {
                    cache.insert_rgba(key, payload);
                }
                black_box(cache.stats())
            },
            BatchSize::LargeInput,
        );
    });

    group.throughput(Throughput::Bytes(EVICTION_PAYLOAD_BYTES as u64));
    let replacement = Arc::new(PixelBuf {
        width: 1,
        height: 1,
        rgba: vec![0; EVICTION_PAYLOAD_BYTES],
    });
    group.bench_function("insert_evicts_one_16mib_final_owner", |b| {
        b.iter_batched_ref(
            || {
                let cache = RamCache::new(RamCacheBudgets::new(
                    0,
                    0,
                    (8 * EVICTION_PAYLOAD_BYTES) as u64,
                    0,
                ));
                let working_set: Vec<_> = (0..9).map(|index| (index, Tier::Full)).collect();
                cache.set_navigation_policy([], working_set);
                for index in 0..8 {
                    cache.insert_rgba(
                        (index, Tier::Full),
                        Arc::new(PixelBuf {
                            width: 1,
                            height: 1,
                            rgba: vec![0; EVICTION_PAYLOAD_BYTES],
                        }),
                    );
                }
                cache
            },
            |cache| {
                cache.insert_rgba((8, Tier::Full), Arc::clone(&replacement));
                black_box(cache.stats())
            },
            BatchSize::LargeInput,
        );
    });

    group.throughput(Throughput::Elements(1));
    group.bench_function("snapshot_10000_observations", |b| {
        b.iter(|| black_box(snapshot_cache.full_prefetch_snapshot()));
    });
    let mut larger_observation = false;
    group.bench_function("live_snapshots_changed_browse_observation_10000", |b| {
        b.iter(|| {
            let snapshots = snapshot_cache.prefetch_snapshots();
            let bytes = if larger_observation { 8 } else { 4 };
            snapshot_cache.insert_rgba(
                (10_000, Tier::Browse),
                Arc::new(PixelBuf {
                    width: 1,
                    height: 1,
                    rgba: vec![0; bytes],
                }),
            );
            larger_observation = !larger_observation;
            black_box(snapshots)
        });
    });
    group.finish();
}

fn realistic_xmp(element_rating: bool) -> String {
    let attribute = if element_rating {
        ""
    } else {
        " xmp:Rating=\"3\""
    };
    let mut xml = format!(
        r#"<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about="" xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/"{attribute} crs:Exposure2012="+0.35" crs:Contrast2012="12">
   <dc:subject xmlns:dc="http://purl.org/dc/elements/1.1/"><rdf:Bag>"#,
    );
    for index in 0..256 {
        xml.push_str(&format!("<rdf:li>keyword-{index:03}</rdf:li>"));
    }
    xml.push_str("</rdf:Bag></dc:subject><crs:ToneCurvePV2012><rdf:Seq>");
    for index in 0..128 {
        xml.push_str(&format!(
            "<rdf:li>{index}, {}</rdf:li>",
            (index * 2).min(255)
        ));
    }
    xml.push_str("</rdf:Seq></crs:ToneCurvePV2012>");
    if element_rating {
        xml.push_str("<xmp:Rating>3</xmp:Rating>");
    }
    xml.push_str("</rdf:Description></rdf:RDF></x:xmpmeta>");
    xml
}

fn bench_xmp(c: &mut Criterion) {
    let attribute_xmp = realistic_xmp(false);
    let element_xmp = realistic_xmp(true);
    assert_eq!(parse_rating(&attribute_xmp), Some(3));
    assert_eq!(parse_rating(&element_xmp), Some(3));
    assert_eq!(
        parse_rating(&update_rating_xml(&attribute_xmp, 5).unwrap()),
        Some(5)
    );
    let mut group = c.benchmark_group("xmp");
    group.sample_size(20);
    group.warm_up_time(Duration::from_millis(300));
    group.measurement_time(Duration::from_millis(1_500));

    group.throughput(Throughput::Bytes(attribute_xmp.len() as u64));
    group.bench_function("parse_attribute", |b| {
        b.iter(|| black_box(parse_rating(black_box(attribute_xmp.as_str()))));
    });

    // Element form places the rating after realistic metadata, exercising the
    // full-document scan rather than the attribute fast path.
    group.throughput(Throughput::Bytes(element_xmp.len() as u64));
    group.bench_function("parse_element_late", |b| {
        b.iter(|| black_box(parse_rating(black_box(element_xmp.as_str()))));
    });

    // Foreign sidecars without any rating skip the full parse via the
    // substring prefilter.
    let unrated_xmp = attribute_xmp.replace(" xmp:Rating=\"3\"", "");
    assert!(!unrated_xmp.contains("Rating"));
    group.throughput(Throughput::Bytes(unrated_xmp.len() as u64));
    group.bench_function("parse_no_rating", |b| {
        b.iter(|| black_box(parse_rating(black_box(unrated_xmp.as_str()))));
    });

    group.throughput(Throughput::Bytes(attribute_xmp.len() as u64));
    group.bench_function("update_attribute", |b| {
        b.iter(|| {
            black_box(update_rating_xml(black_box(attribute_xmp.as_str()), black_box(5)).unwrap())
        });
    });

    group.finish();
}

fn disk_entry(path: PathBuf) -> FolderEntry {
    let file_name = path
        .file_name()
        .expect("benchmark path has a file name")
        .to_string_lossy()
        .into_owned();
    FolderEntry {
        path,
        file_name,
        size: 128_734_921,
        mtime_ns: 1_752_600_123_456_789_000,
    }
}

fn bench_disk_cache_key(c: &mut Criterion) {
    let entries = [
        disk_entry(PathBuf::from("/Volumes/Photos/2026/07/HCA04696.ARW")),
        disk_entry(PathBuf::from(format!(
            "/Volumes/Studio Archive/Clients/{}/2026-07-21/HCA04696.ARW",
            "international-campaign-".repeat(6)
        ))),
    ];
    let mut group = c.benchmark_group("disk_cache_key");
    group.sample_size(30);
    group.warm_up_time(Duration::from_millis(300));
    group.measurement_time(Duration::from_millis(1_500));

    for entry in &entries {
        let path_len = entry.path.to_string_lossy().len();
        group.throughput(Throughput::Bytes((path_len + 21) as u64));
        group.bench_with_input(BenchmarkId::new("browse", path_len), entry, |b, entry| {
            b.iter(|| black_box(DiskCache::key(black_box(entry), black_box(Tier::Browse))));
        });
    }

    group.finish();
}

fn bench_disk_cache_gc_scan(c: &mut Criterion) {
    let mut group = c.benchmark_group("disk_cache_gc_scan");
    group.sample_size(10);
    group.warm_up_time(Duration::from_millis(300));
    group.measurement_time(Duration::from_secs(2));

    for len in [1_000_usize, 10_000] {
        let directory = tempfile::tempdir().expect("benchmark cache directory");
        let cache = DiskCache::open_at(directory.path().to_owned());
        for index in 0..len {
            let entry = FolderEntry {
                path: PathBuf::from(format!("/benchmark/photo-{index:08}.arw")),
                file_name: format!("photo-{index:08}.arw"),
                size: index as u64,
                mtime_ns: index as i64,
            };
            cache
                .put(&DiskCache::key(&entry, Tier::Browse), b"x")
                .expect("benchmark cache object writes");
        }

        group.throughput(Throughput::Elements(len as u64));
        group.bench_with_input(BenchmarkId::from_parameter(len), &len, |b, _| {
            b.iter(|| assert_eq!(black_box(cache.gc(black_box(u64::MAX))), 0));
        });
    }

    group.finish();
}

/// Set VIEWR_BENCH_RAW to an ARW or DNG path to include disk decode and both
/// development qualities. Decode happens in iter_batched's setup closure for
/// the develop cases, so only the consuming development pipeline is timed.
fn bench_opt_in_raw(c: &mut Criterion) {
    let Some(raw_path) = std::env::var_os("VIEWR_BENCH_RAW").map(PathBuf::from) else {
        eprintln!(
            "raw_opt_in skipped: set VIEWR_BENCH_RAW to an untracked ARW or DNG fixture to run it"
        );
        return;
    };

    let probe = decode::load(&raw_path).unwrap_or_else(|error| {
        panic!(
            "VIEWR_BENCH_RAW={} could not be decoded: {error}",
            raw_path.display()
        )
    });
    let sensor_pixels = (probe.raw.width * probe.raw.height) as u64;
    drop(probe);

    let mut group = c.benchmark_group("raw_opt_in");
    group.sample_size(10);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(5));

    let raw_bytes = std::fs::metadata(&raw_path)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    group.throughput(Throughput::Bytes(raw_bytes));
    group.bench_function("decode", |b| {
        b.iter_batched(
            || raw_path.clone(),
            |path| black_box(decode::load(black_box(path.as_path())).unwrap()),
            BatchSize::PerIteration,
        );
    });

    group.bench_function("thumb_and_meta_360", |b| {
        b.iter(|| {
            black_box(
                decode::thumb_and_meta(black_box(raw_path.as_path()), black_box(360)).unwrap(),
            )
        });
    });

    group.bench_function("metadata_only", |b| {
        b.iter(|| black_box(decode::metadata(black_box(raw_path.as_path())).unwrap()));
    });

    for (name, scale) in [
        (
            "legacy_two_pass",
            develop::benchmark_scale_cfa_legacy as fn(rawler::RawImage) -> Vec<f32>,
        ),
        (
            "fused",
            develop::benchmark_scale_cfa_fused as fn(rawler::RawImage) -> Vec<f32>,
        ),
    ] {
        group.throughput(Throughput::Elements(sensor_pixels));
        group.bench_function(format!("scale_cfa/{name}"), |b| {
            b.iter_batched(
                || {
                    decode::load(&raw_path)
                        .expect("RAW decoded during benchmark setup")
                        .raw
                },
                |raw| black_box(scale(black_box(raw))),
                BatchSize::PerIteration,
            );
        });
    }

    for quality in [Quality::Browse, Quality::Full] {
        group.throughput(Throughput::Elements(sensor_pixels));
        group.bench_with_input(
            BenchmarkId::new("develop", format!("{quality:?}")),
            &quality,
            |b, &quality| {
                b.iter_batched(
                    || {
                        decode::load(&raw_path)
                            .expect("RAW decoded during benchmark setup")
                            .raw
                    },
                    |raw| black_box(develop::develop(black_box(raw), quality).unwrap()),
                    BatchSize::PerIteration,
                );
            },
        );
    }

    group.finish();
}

criterion_group! {
    name = core_hot_paths;
    config = Criterion::default()
        .sample_size(20)
        .warm_up_time(Duration::from_millis(500))
        .measurement_time(Duration::from_secs(2));
    targets =
        bench_outward_order,
        bench_navigation_plan,
        bench_metadata_queue_setup,
        bench_rating_db_lookup,
        bench_rating_folder_load,
        bench_legacy_rating_folder_load,
        bench_legacy_rating_stress,
        bench_rating_db_reopen,
        bench_rating_db_cold_released_migrations,
        bench_rating_db_cold_v7_migration,
        bench_rating_db_journal,
        bench_rating_db_pending_scan,
        bench_sidecar_owner_batch,
        bench_unicode_sidecar_owner_batch,
        bench_resize,
        bench_orientation,
        bench_jpeg,
        bench_ram_cache,
        bench_ram_cache_eviction_scaling,
        bench_full_cache_policy,
        bench_xmp,
        bench_disk_cache_key,
        bench_disk_cache_gc_scan,
        bench_opt_in_raw
}
criterion_main!(core_hot_paths);
