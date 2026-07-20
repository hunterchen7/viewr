mod app;
mod loupe;

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use viewr_core::develop::{Quality, develop};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("dev") => {
            let [_, input, rest @ ..] = args.as_slice() else {
                bail!("usage: viewr dev <file.arw> [out-dir]");
            };
            let out_dir = rest
                .first()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."));
            spike(Path::new(input), &out_dir)
        }
        Some(arg) => {
            let path = Path::new(arg);
            if path.is_dir() {
                app::run(path, None)
            } else if path.is_file() {
                let parent = path.parent().context("file has no parent directory")?;
                app::run(parent, Some(path))
            } else {
                bail!("not a file or directory: {arg}");
            }
        }
        None => {
            eprintln!("usage: viewr <folder|file.arw>     browse raws");
            eprintln!("       viewr dev <file.arw> [out]  decode spike with timings");
            Ok(())
        }
    }
}

/// M0 spike: decode + develop both tiers, dump JPEGs, print a timings table.
fn spike(input: &Path, out_dir: &Path) -> Result<()> {
    let file_bytes = std::fs::metadata(input)?.len();

    let t_total = Instant::now();
    let decoded = viewr_core::decode::load(input).context("decode failed")?;
    let raw = decoded.raw;

    println!(
        "{} — {} {} | {}x{} CFA, {} bpp, {:.1} MB file",
        input.file_name().unwrap_or_default().to_string_lossy(),
        raw.clean_make,
        raw.clean_model,
        raw.width,
        raw.height,
        raw.bps,
        file_bytes as f64 / 1e6,
    );
    let exif = &decoded.metadata.exif;
    println!(
        "  exif: iso {:?}, exposure {:?}, f/{:?}, lens {:?}",
        exif.iso_speed_ratings,
        exif.exposure_time,
        exif.fnumber,
        decoded
            .metadata
            .lens
            .as_ref()
            .map_or("?", |l| l.lens_name.as_str()),
    );

    // Browse tier needs its own copy of the mosaic; Full consumes the original.
    let t = Instant::now();
    let raw_copy = raw.clone();
    let t_clone = t.elapsed();

    let t = Instant::now();
    let (browse, bt) = develop(raw_copy, Quality::Browse).context("browse develop failed")?;
    let t_browse = t.elapsed();

    let t = Instant::now();
    let (full, ft) = develop(raw, Quality::Full).context("full develop failed")?;
    let t_full = t.elapsed();

    let t = Instant::now();
    std::fs::create_dir_all(out_dir)?;
    let stem = input.file_stem().unwrap_or_default().to_string_lossy();
    let browse_path = out_dir.join(format!("{stem}.browse.jpg"));
    let full_path = out_dir.join(format!("{stem}.full.jpg"));
    write_jpeg(&browse_path, &browse, 87)?;
    write_jpeg(&full_path, &full, 90)?;
    let t_encode = t.elapsed();

    println!("  open+parse    {:>8.1?}", decoded.t_open);
    println!("  metadata      {:>8.1?}", decoded.t_metadata);
    println!("  entropy decode{:>8.1?}", decoded.t_raw_decode);
    println!("  clone mosaic  {:>8.1?}", t_clone);
    println!(
        "  browse tier   {:>8.1?}  ({}x{}; rescale {:.1?}, demosaic {:.1?}, calibrate {:.1?}, pack {:.1?})",
        t_browse, browse.width, browse.height, bt.rescale, bt.demosaic, bt.calibrate, bt.gamma_pack
    );
    println!(
        "  full tier     {:>8.1?}  ({}x{}; rescale {:.1?}, demosaic {:.1?}, calibrate {:.1?}, pack {:.1?})",
        t_full, full.width, full.height, ft.rescale, ft.demosaic, ft.calibrate, ft.gamma_pack
    );
    println!("  encode 2 jpg  {:>8.1?}", t_encode);
    println!("  TOTAL         {:>8.1?}", t_total.elapsed());
    println!(
        "  wrote {} and {}",
        browse_path.display(),
        full_path.display()
    );
    Ok(())
}

fn write_jpeg(path: &Path, buf: &viewr_core::types::PixelBuf, quality: u8) -> Result<()> {
    let encoder = jpeg_encoder::Encoder::new_file(path, quality)?;
    encoder.encode(
        &buf.rgba,
        u16::try_from(buf.width).context("width exceeds JPEG limit")?,
        u16::try_from(buf.height).context("height exceeds JPEG limit")?,
        jpeg_encoder::ColorType::Rgba,
    )?;
    Ok(())
}
