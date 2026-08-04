use std::hint::black_box;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use viewr_jpeg_bakeoff::{
    Codec, DedicatedRusturboEncoder, QUALITIES, TurbojpegEncoder, encode, fixtures,
    full_resolution_fixture, probe,
};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|arg| arg == "stress") {
        return stress(&args[1..]);
    }
    if args.first().is_some_and(|arg| arg == "dedicated-stress") {
        return dedicated_stress(&args[1..]);
    }
    if args.first().is_some_and(|arg| arg == "restart-compare") {
        return restart_compare(&args[1..]);
    }
    if args
        .first()
        .is_some_and(|arg| arg == "dedicated-contention")
    {
        return dedicated_contention(&args[1..]);
    }
    println!(
        "codec,fixture,width,height,quality,bytes,median_ms,megapixels_per_second,psnr_rgb,max_abs_rgb,neighbor_delta_mae,sampling"
    );
    for fixture in fixtures() {
        let repeats = if fixture.megapixels() >= 8.0 { 3 } else { 5 };
        for quality in QUALITIES {
            for codec in Codec::ALL {
                let result = probe(codec, &fixture, quality, repeats)?;
                println!(
                    "{},{},{},{},{},{},{:.3},{:.3},{:.4},{},{:.6},{}",
                    result.codec,
                    result.fixture,
                    result.width,
                    result.height,
                    result.quality,
                    result.encoded_bytes,
                    result.median_encode.as_secs_f64() * 1_000.0,
                    result.megapixels_per_second,
                    result.psnr_rgb,
                    result.max_abs_rgb,
                    result.delta_mae,
                    result.sampling,
                );
            }
        }
    }
    Ok(())
}

fn restart_compare(args: &[String]) -> anyhow::Result<()> {
    if args.len() != 3 {
        bail!("usage: viewr-jpeg-bakeoff restart-compare WORKERS RUNS QUALITY");
    }
    let workers: usize = args[0].parse().context("invalid worker count")?;
    let runs: usize = args[1].parse().context("invalid run count")?;
    let quality: u8 = args[2].parse().context("invalid quality")?;
    if runs == 0 {
        bail!("run count must be at least one");
    }

    let fixture = full_resolution_fixture();
    let parallel = DedicatedRusturboEncoder::new(workers, quality)?;
    let serial_codec = Codec::JpegRusturboRows { threads: 1 };
    let serial_reference = encode(serial_codec, &fixture, quality)?;
    let parallel_reference = parallel.encode(&fixture)?;
    if parallel_reference != serial_reference {
        bail!("row-parallel output differs from the serial restart stream");
    }

    let mut serial_times = Vec::with_capacity(runs);
    let mut parallel_times = Vec::with_capacity(runs);
    println!("run,first,serial_ms,parallel_ms,encoded_bytes");
    for run in 1..=runs {
        let (serial_elapsed, parallel_elapsed) = if run & 1 == 1 {
            let started = Instant::now();
            black_box(encode(serial_codec, black_box(&fixture), quality)?);
            let serial_elapsed = started.elapsed();
            let started = Instant::now();
            black_box(parallel.encode(black_box(&fixture))?);
            (serial_elapsed, started.elapsed())
        } else {
            let started = Instant::now();
            black_box(parallel.encode(black_box(&fixture))?);
            let parallel_elapsed = started.elapsed();
            let started = Instant::now();
            black_box(encode(serial_codec, black_box(&fixture), quality)?);
            (started.elapsed(), parallel_elapsed)
        };
        serial_times.push(serial_elapsed);
        parallel_times.push(parallel_elapsed);
        println!(
            "{run},{},{:.3},{:.3},{}",
            if run & 1 == 1 { "serial" } else { "parallel" },
            millis(serial_elapsed),
            millis(parallel_elapsed),
            serial_reference.len(),
        );
    }

    let serial_median = median(serial_times);
    let parallel_median = median(parallel_times);
    println!(
        "summary workers={} runs={runs} quality={quality} serial_median_ms={:.3} parallel_median_ms={:.3} speedup={:.3} encoded_bytes={}",
        parallel.workers(),
        millis(serial_median),
        millis(parallel_median),
        serial_median.as_secs_f64() / parallel_median.as_secs_f64(),
        serial_reference.len(),
    );
    Ok(())
}

fn dedicated_stress(args: &[String]) -> anyhow::Result<()> {
    if args.len() != 3 {
        bail!("usage: viewr-jpeg-bakeoff dedicated-stress WORKERS ITERATIONS QUALITY");
    }
    let workers: usize = args[0].parse().context("invalid worker count")?;
    let iterations: usize = args[1].parse().context("invalid iteration count")?;
    let quality: u8 = args[2].parse().context("invalid quality")?;
    if iterations == 0 {
        bail!("iteration count must be at least one");
    }
    let fixture = full_resolution_fixture();
    let encoder = DedicatedRusturboEncoder::new(workers, quality)?;
    let mut total_bytes = 0_usize;
    for _ in 0..iterations {
        let jpeg = encoder.encode(black_box(&fixture))?;
        total_bytes = total_bytes.wrapping_add(black_box(jpeg.len()));
    }
    println!(
        "mode=dedicated-stress workers={} fixture={} iterations={iterations} quality={quality} total_bytes={total_bytes}",
        encoder.workers(),
        fixture.name
    );
    Ok(())
}

fn dedicated_contention(args: &[String]) -> anyhow::Result<()> {
    if args.len() != 3 {
        bail!("usage: viewr-jpeg-bakeoff dedicated-contention WORKERS RUNS QUALITY");
    }
    let workers: usize = args[0].parse().context("invalid worker count")?;
    let runs: usize = args[1].parse().context("invalid run count")?;
    let quality: u8 = args[2].parse().context("invalid quality")?;
    if runs == 0 {
        bail!("run count must be at least one");
    }

    let synthetic_fixture = full_resolution_fixture();
    let encoder = DedicatedRusturboEncoder::new(workers, quality)?;
    if let Some(raw_path) = std::env::var_os("VIEWR_BENCH_RAW").map(PathBuf::from) {
        let decoded = viewr_core::decode::load(&raw_path)
            .with_context(|| format!("failed to decode VIEWR_BENCH_RAW={}", raw_path.display()))?;
        let raw = decoded.raw;
        let (jpeg_pixels, _) =
            viewr_core::develop::develop(raw.clone(), viewr_core::develop::Quality::Full)
                .context("failed to prepare Full RAW JPEG fixture")?;
        let jpeg_fixture = viewr_jpeg_bakeoff::Fixture {
            name: "raw_full_pixels",
            width: jpeg_pixels.width,
            height: jpeg_pixels.height,
            rgba: jpeg_pixels.into_rgba(),
        };
        return run_contention(
            &encoder,
            &jpeg_fixture,
            runs,
            "raw_full_develop",
            || raw.clone(),
            |raw| {
                let (pixels, _) =
                    viewr_core::develop::develop(raw, viewr_core::develop::Quality::Full)?;
                Ok(pixel_checksum(&pixels))
            },
        );
    }

    let foreground_source = viewr_core::types::PixelBuf::try_new_opaque(
        synthetic_fixture.width,
        synthetic_fixture.height,
        synthetic_fixture.rgba.clone(),
    )
    .expect("the synthetic RGBA fixture is opaque and tightly packed");
    run_contention(
        &encoder,
        &synthetic_fixture,
        runs,
        "orient_r90_33mp",
        || foreground_source.clone(),
        |pixels| {
            let oriented = viewr_core::resize::apply_orient(pixels, viewr_core::types::Orient::R90);
            Ok(pixel_checksum(&oriented))
        },
    )
}

fn run_contention<S, Setup, Work>(
    encoder: &DedicatedRusturboEncoder,
    fixture: &viewr_jpeg_bakeoff::Fixture,
    runs: usize,
    foreground_kind: &str,
    setup_foreground: Setup,
    run_foreground: Work,
) -> anyhow::Result<()>
where
    Setup: Fn() -> S,
    Work: Fn(S) -> anyhow::Result<u64>,
{
    black_box(encoder.encode(black_box(fixture))?);
    black_box(run_foreground(setup_foreground())?);

    println!(
        "foreground={foreground_kind} global_rayon_threads={}",
        rayon::current_num_threads()
    );
    println!(
        "mode,workers,run,jpeg_ms,foreground_ms,pair_wall_ms,encoded_bytes,foreground_checksum"
    );
    let mut isolated_jpeg = Vec::with_capacity(runs);
    let mut isolated_foreground = Vec::with_capacity(runs);
    for run in 1..=runs {
        let started = Instant::now();
        let jpeg = encoder.encode(black_box(fixture))?;
        let jpeg_elapsed = started.elapsed();

        let foreground_input = setup_foreground();
        let started = Instant::now();
        let checksum = run_foreground(foreground_input)?;
        let foreground_elapsed = started.elapsed();

        isolated_jpeg.push(jpeg_elapsed);
        isolated_foreground.push(foreground_elapsed);
        println!(
            "isolated,{},{run},{:.3},{:.3},0.000,{},{}",
            encoder.workers(),
            millis(jpeg_elapsed),
            millis(foreground_elapsed),
            jpeg.len(),
            checksum
        );
    }

    let mut concurrent_jpeg = Vec::with_capacity(runs);
    let mut concurrent_foreground = Vec::with_capacity(runs);
    let mut concurrent_pair = Vec::with_capacity(runs);
    std::thread::scope(|scope| -> anyhow::Result<()> {
        let (start_tx, start_rx) = mpsc::sync_channel::<()>(0);
        let (result_tx, result_rx) = mpsc::channel();
        let jpeg_worker = scope.spawn(move || {
            while start_rx.recv().is_ok() {
                let started = Instant::now();
                let jpeg = encoder.encode(black_box(fixture));
                if result_tx.send((started.elapsed(), jpeg)).is_err() {
                    break;
                }
            }
        });

        for run in 1..=runs {
            let foreground_input = setup_foreground();
            let pair_started = Instant::now();
            start_tx
                .send(())
                .context("background JPEG worker stopped before measurement")?;
            let foreground_started = Instant::now();
            let checksum = run_foreground(foreground_input)?;
            let foreground_elapsed = foreground_started.elapsed();
            let (jpeg_elapsed, jpeg) = result_rx
                .recv()
                .context("background JPEG worker returned no result")?;
            let pair_elapsed = pair_started.elapsed();
            let jpeg = jpeg?;

            concurrent_jpeg.push(jpeg_elapsed);
            concurrent_foreground.push(foreground_elapsed);
            concurrent_pair.push(pair_elapsed);
            println!(
                "concurrent,{},{run},{:.3},{:.3},{:.3},{},{}",
                encoder.workers(),
                millis(jpeg_elapsed),
                millis(foreground_elapsed),
                millis(pair_elapsed),
                jpeg.len(),
                checksum,
            );
        }
        drop(start_tx);
        jpeg_worker
            .join()
            .map_err(|_| anyhow::anyhow!("background JPEG worker panicked"))?;
        Ok(())
    })?;

    let isolated_jpeg_median = median(isolated_jpeg);
    let isolated_foreground_median = median(isolated_foreground);
    let concurrent_jpeg_median = median(concurrent_jpeg);
    let concurrent_foreground_median = median(concurrent_foreground);
    let concurrent_pair_median = median(concurrent_pair);
    println!(
        "summary workers={} runs={runs} foreground={foreground_kind} global_rayon_threads={} isolated_jpeg_ms={:.3} isolated_foreground_ms={:.3} concurrent_jpeg_ms={:.3} concurrent_foreground_ms={:.3} pair_wall_ms={:.3} foreground_slowdown={:.3} jpeg_slowdown={:.3}",
        encoder.workers(),
        rayon::current_num_threads(),
        millis(isolated_jpeg_median),
        millis(isolated_foreground_median),
        millis(concurrent_jpeg_median),
        millis(concurrent_foreground_median),
        millis(concurrent_pair_median),
        concurrent_foreground_median.as_secs_f64() / isolated_foreground_median.as_secs_f64(),
        concurrent_jpeg_median.as_secs_f64() / isolated_jpeg_median.as_secs_f64(),
    );
    Ok(())
}

fn pixel_checksum(pixels: &viewr_core::types::PixelBuf) -> u64 {
    let mut checksum = u64::from(pixels.width) << 32 | u64::from(pixels.height);
    for byte in pixels.rgba().iter().step_by(4_099) {
        checksum = checksum.rotate_left(7) ^ u64::from(*byte);
    }
    checksum ^ pixels.rgba().len() as u64
}

fn median(mut values: Vec<Duration>) -> Duration {
    values.sort_unstable();
    values[values.len() / 2]
}

fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}

fn stress(args: &[String]) -> anyhow::Result<()> {
    if args.len() != 3 {
        bail!("usage: viewr-jpeg-bakeoff stress CODEC ITERATIONS QUALITY");
    }
    let codec = parse_codec(&args[0])?;
    let iterations: usize = args[1].parse().context("invalid iteration count")?;
    let quality: u8 = args[2].parse().context("invalid quality")?;
    let fixture = full_resolution_fixture();
    let mut total_bytes = 0_usize;
    if args[0] == "libjpeg-turbo-c-reused" {
        let mut encoder = TurbojpegEncoder::new(quality)?;
        for _ in 0..iterations {
            let jpeg = encoder.encode(black_box(&fixture))?;
            total_bytes = total_bytes.wrapping_add(black_box(jpeg.len()));
        }
    } else {
        for _ in 0..iterations {
            let jpeg = encode(codec, black_box(&fixture), quality)?;
            total_bytes = total_bytes.wrapping_add(black_box(jpeg.len()));
        }
    }
    println!(
        "codec={} fixture={} iterations={iterations} quality={quality} total_bytes={total_bytes}",
        args[0], fixture.name
    );
    Ok(())
}

fn parse_codec(name: &str) -> anyhow::Result<Codec> {
    if name == "libjpeg-turbo-c-reused" {
        return Ok(Codec::LibjpegTurboC);
    }
    Codec::ALL
        .into_iter()
        .find(|codec| codec.name() == name)
        .with_context(|| {
            let mut names = Codec::ALL.map(Codec::name).to_vec();
            names.push("libjpeg-turbo-c-reused");
            let names = names.join(", ");
            format!("unknown codec {name:?}; expected one of {names}")
        })
}
