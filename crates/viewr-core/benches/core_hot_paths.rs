use std::hint::black_box;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use viewr_core::cache_disk::DiskCache;
use viewr_core::cache_ram::RamCache;
use viewr_core::decode;
use viewr_core::develop::{self, Quality};
use viewr_core::folder::{FolderEntry, outward_order};
use viewr_core::jobs::{decode_jpeg, encode_jpeg};
use viewr_core::planning::build_plan_targets;
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

    for len in [100_usize, 1_000, 10_000] {
        group.throughput(Throughput::Elements(len as u64));
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
            &(current, sparse),
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
    let photo = synthetic_photo(PHOTO_WIDTH, PHOTO_HEIGHT);
    let decoded_bytes = photo.byte_len() as u64;

    let mut encode_group = c.benchmark_group("jpeg_encode");
    encode_group.sample_size(10);
    encode_group.warm_up_time(Duration::from_millis(300));
    encode_group.measurement_time(Duration::from_secs(2));
    encode_group.throughput(Throughput::Bytes(decoded_bytes));
    for quality in [80_u8, 92] {
        encode_group.bench_with_input(
            BenchmarkId::from_parameter(quality),
            &quality,
            |b, &quality| {
                b.iter(|| black_box(encode_jpeg(black_box(&photo), quality).unwrap()));
            },
        );
    }
    encode_group.finish();

    // Encoding is deliberately outside the decode timing.
    let encoded = encode_jpeg(&photo, 88).expect("synthetic photo must encode");
    let mut decode_group = c.benchmark_group("jpeg_decode");
    decode_group.sample_size(10);
    decode_group.warm_up_time(Duration::from_millis(300));
    decode_group.measurement_time(Duration::from_secs(2));
    decode_group.throughput(Throughput::Bytes(decoded_bytes));
    decode_group.bench_function("quality_88", |b| {
        b.iter(|| black_box(decode_jpeg(black_box(encoded.as_slice())).unwrap()));
    });
    decode_group.finish();
}

fn bench_ram_cache(c: &mut Criterion) {
    const RESIDENT_ENTRIES: usize = 32;
    let rgba = Arc::new(synthetic_photo(512, 384));
    let rgba_bytes = rgba.byte_len() as u64;
    let jpeg = Arc::new(encode_jpeg(&rgba, 85).expect("cache fixture must encode"));
    let cache = RamCache::new(
        0,
        rgba_bytes * RESIDENT_ENTRIES as u64,
        jpeg.len() as u64 * RESIDENT_ENTRIES as u64,
    );

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

    // Reuse the payload so this isolates LRU/hash-map churn rather than buffer
    // allocation. Every insert has a fresh key and evicts one resident entry.
    let churn = RamCache::new(0, rgba_bytes * 8, 0);
    let mut next_key = 0_usize;
    group.throughput(Throughput::Elements(1));
    group.bench_function("rgba_insert_with_eviction", |b| {
        b.iter(|| {
            let key = (next_key, Tier::Browse);
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
        let cache = RamCache::new(0, entries as u64 * 4, 0);
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

/// Set VIEWR_BENCH_RAW to an ARW or DNG path to include disk decode and both
/// development qualities. Decode happens in iter_batched's setup closure for
/// the develop cases, so only the consuming development pipeline is timed.
fn bench_opt_in_raw(c: &mut Criterion) {
    let Some(raw_path) = std::env::var_os("VIEWR_BENCH_RAW").map(PathBuf::from) else {
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
        bench_resize,
        bench_orientation,
        bench_jpeg,
        bench_ram_cache,
        bench_ram_cache_eviction_scaling,
        bench_xmp,
        bench_disk_cache_key,
        bench_opt_in_raw
}
criterion_main!(core_hot_paths);
