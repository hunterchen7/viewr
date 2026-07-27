use anyhow::Result;
use viewr_jpeg_bakeoff::{Codec, QUALITIES, fixtures, probe};

fn main() -> Result<()> {
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
