use std::hint::black_box;

use anyhow::{Context, bail};
use viewr_jpeg_bakeoff::{
    Codec, QUALITIES, TurbojpegEncoder, encode, fixtures, full_resolution_fixture, probe,
};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|arg| arg == "stress") {
        return stress(&args[1..]);
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
