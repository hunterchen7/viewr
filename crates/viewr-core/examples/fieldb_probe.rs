//! Field B probe: decompose open+parse (~30ms) and metadata (~12ms) stages.
//! Usage: VIEWR_TEST_RAW=/path/to/file.ARW cargo run --release -p viewr-core --example fieldb_probe

use std::path::Path;
use std::time::Instant;

use rawler::decoders::arw::ArwDecoder;
use rawler::decoders::{RawDecodeParams, supported_extensions};
use rawler::formats::tiff::GenericTiffReader;
use rawler::formats::tiff::reader::TiffReader;
use rawler::rawsource::RawSource;
use rawler::tags::ExifTag;

fn ms(d: std::time::Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

fn bench<T>(label: &str, iters: usize, mut f: impl FnMut() -> T) {
    let mut times = Vec::with_capacity(iters);
    for _ in 0..iters {
        let t = Instant::now();
        let out = f();
        times.push(t.elapsed());
        std::hint::black_box(out);
    }
    times.sort();
    let min = ms(times[0]);
    let med = ms(times[times.len() / 2]);
    let max = ms(times[times.len() - 1]);
    println!("{label:<48} min {min:8.3} ms   med {med:8.3} ms   max {max:8.3} ms");
}

fn main() {
    let path_var = std::env::var("VIEWR_TEST_RAW").unwrap_or_else(|_| {
        "/Users/hunterchen/Documents/GitHub/viewr/testdata/real-raw-corpus/HCA04875.ARW".into()
    });
    let path = Path::new(&path_var);
    let _ = supported_extensions();
    let iters: usize = std::env::var("ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(15);

    // One-time global inits (lazy statics) — time first touch explicitly.
    let t = Instant::now();
    rawler::force_initialization();
    println!(
        "{:<48} {:8.3} ms (once per process)",
        "LOADER init (cameras.toml parse)",
        ms(t.elapsed())
    );
    let t = Instant::now();
    let lenses = rawler::lens::get_lenses();
    println!(
        "{:<48} {:8.3} ms (once per process, {} lenses)",
        "LENSES_DB init (lenses.toml parse)",
        ms(t.elapsed()),
        lenses.len()
    );

    println!(
        "--- file: {} ({} bytes)",
        path.display(),
        std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
    );

    // Warm page cache.
    let _ = std::fs::read(path).unwrap();

    bench("std::fs::read (whole file)", iters, || {
        std::fs::read(path).unwrap().len()
    });
    bench("File::open only", iters, || {
        std::fs::File::open(path).unwrap()
    });
    bench("path.canonicalize", iters, || path.canonicalize().unwrap());
    bench("RawSource::new (mmap populate + advise)", iters, || {
        RawSource::new(path).unwrap()
    });

    let source = RawSource::new(path).unwrap();

    bench("GenericTiffReader::new (full IFD walk)", iters, || {
        GenericTiffReader::new(&mut source.reader(), 0, 0, None, &[]).unwrap()
    });

    let tiff = GenericTiffReader::new(&mut source.reader(), 0, 0, None, &[]).unwrap();
    bench(
        "ArwDecoder::new (check_supported + makernote)",
        iters,
        || ArwDecoder::new(&source, tiff.clone(), rawler::global_loader()).unwrap(),
    );
    // Isolate the tiff.clone() overhead paid inside the previous bench.
    bench("tiff.clone() (overhead of prior bench)", iters, || {
        tiff.clone()
    });

    // makernote parse alone
    bench("parse_makernote alone", iters, || {
        let exif = tiff.find_first_ifd_with_tag(ExifTag::MakerNotes).unwrap();
        exif.parse_makernote(
            &mut source.reader(),
            rawler::formats::tiff::ifd::OffsetMode::Absolute,
            &[],
        )
        .unwrap()
    });

    bench("get_decoder (probes + tiff + arw::new)", iters, || {
        rawler::get_decoder(&source).unwrap()
    });

    let decoder = rawler::get_decoder(&source).unwrap();
    let params = RawDecodeParams::default();
    bench("decoder.raw_metadata", iters, || {
        decoder.raw_metadata(&source, &params).unwrap()
    });

    // Full viewr-side stages
    bench("viewr decode::metadata (open+parse+meta)", iters, || {
        viewr_core::decode::metadata(path).unwrap()
    });
    bench("viewr decode::thumb_and_meta(360)", 5, || {
        viewr_core::decode::thumb_and_meta(path, 360).unwrap()
    });
    bench("open+parse (RawSource+get_decoder) combined", iters, || {
        let s = RawSource::new(path).unwrap();
        let d = rawler::get_decoder(&s).unwrap();
        (s, d)
    });

    // What decode::load pays: open + parse + metadata + raw decode
    bench("viewr decode::load (full)", 5, || {
        viewr_core::decode::load(path).unwrap()
    });
    let d = viewr_core::decode::load(path).unwrap();
    println!(
        "decode::load internal timings: open {:.3} ms, metadata {:.3} ms, raw {:.3} ms",
        ms(d.t_open),
        ms(d.t_metadata),
        ms(d.t_raw_decode)
    );

    // Reuse experiment: one source + one decoder serving metadata repeatedly
    let source2 = RawSource::new(path).unwrap();
    let decoder2 = rawler::get_decoder(&source2).unwrap();
    bench("raw_metadata on cached decoder (reuse)", iters, || {
        decoder2.raw_metadata(&source2, &params).unwrap()
    });
    bench("raw_image on cached decoder", 3, || {
        decoder2.raw_image(&source2, &params, false).unwrap()
    });
}
