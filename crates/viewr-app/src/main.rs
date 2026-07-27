//! Desktop entry point for `viewr`, a low-latency RAW culling viewer.
//!
//! The default command opens a folder or RAW file in the graphical viewer. The
//! installer-only `--pick-folder` command opens the platform folder chooser.
//! The `dev` command decodes one file, writes diagnostic JPEGs, and prints
//! timing information for pipeline investigation.
#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

mod app;
mod color;
mod config;
mod filmstrip;
mod loupe;
mod progressive_texture;
mod rating_groups;
mod settings;
mod texture_lru;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use viewr_core::develop::{Quality, develop};

#[derive(Debug, Eq, PartialEq)]
enum Command {
    Browse(PathBuf),
    Develop { input: PathBuf, out_dir: PathBuf },
    NotifyFileAssociations,
    PickFolder,
    PrintUsage,
}

fn main() -> Result<()> {
    run(parse_command(std::env::args_os().skip(1))?)
}

fn parse_command(args: impl IntoIterator<Item = OsString>) -> Result<Command> {
    let args: Vec<OsString> = args.into_iter().collect();
    match args.as_slice() {
        [] => Ok(Command::PrintUsage),
        [flag] if flag == "--notify-file-associations" => Ok(Command::NotifyFileAssociations),
        [flag] if flag == "--pick-folder" => Ok(Command::PickFolder),
        [command, input] if command == "dev" => Ok(Command::Develop {
            input: PathBuf::from(input),
            out_dir: PathBuf::from("."),
        }),
        [command, input, out_dir] if command == "dev" => Ok(Command::Develop {
            input: PathBuf::from(input),
            out_dir: PathBuf::from(out_dir),
        }),
        [command, ..] if command == "dev" => {
            bail!("usage: viewr dev <file.arw> [out-dir]");
        }
        [flag, ..] if flag == "--pick-folder" => {
            bail!("usage: viewr --pick-folder");
        }
        [flag, ..] if flag == "--notify-file-associations" => {
            bail!("usage: viewr --notify-file-associations");
        }
        [path] => Ok(Command::Browse(PathBuf::from(path))),
        _ => bail!("usage: viewr <folder|file.arw>"),
    }
}

fn run(command: Command) -> Result<()> {
    match command {
        Command::Browse(path) => {
            if path.is_dir() {
                app::run(&path, None)
            } else if path.is_file() {
                let parent = path.parent().context("file has no parent directory")?;
                app::run(parent, Some(&path))
            } else {
                bail!("not a file or directory: {}", path.display());
            }
        }
        Command::Develop { input, out_dir } => spike(&input, &out_dir),
        Command::NotifyFileAssociations => notify_file_associations(),
        Command::PickFolder => {
            if let Some(path) = rfd::FileDialog::new().pick_folder() {
                app::run(&path, None)
            } else {
                Ok(())
            }
        }
        Command::PrintUsage => {
            eprintln!("usage: viewr <folder|file.arw>     browse raws");
            eprintln!("       viewr --pick-folder          choose a folder");
            eprintln!("       viewr dev <file.arw> [out]  decode spike with timings");
            Ok(())
        }
    }
}

#[cfg(target_os = "windows")]
fn notify_file_associations() -> Result<()> {
    use std::ptr;
    use windows_sys::Win32::UI::Shell::{
        SHCNE_ASSOCCHANGED, SHCNF_FLUSHNOWAIT, SHCNF_IDLIST, SHChangeNotify,
    };

    // SAFETY: SHCNE_ASSOCCHANGED requires null item pointers with
    // SHCNF_IDLIST. The call copies no caller-owned memory and returns no data.
    unsafe {
        SHChangeNotify(
            SHCNE_ASSOCCHANGED as i32,
            SHCNF_IDLIST | SHCNF_FLUSHNOWAIT,
            ptr::null(),
            ptr::null(),
        );
    }
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn notify_file_associations() -> Result<()> {
    bail!("file-association notification is supported only on Windows")
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

#[cfg(test)]
mod tests {
    use super::{Command, parse_command};
    use std::ffi::OsString;
    use std::path::PathBuf;

    fn parse(args: &[&str]) -> anyhow::Result<Command> {
        parse_command(args.iter().map(OsString::from))
    }

    #[test]
    fn parses_installer_folder_picker() {
        assert_eq!(parse(&["--pick-folder"]).unwrap(), Command::PickFolder);
        assert!(parse(&["--pick-folder", "extra"]).is_err());
    }

    #[test]
    fn parses_windows_association_notification() {
        assert_eq!(
            parse(&["--notify-file-associations"]).unwrap(),
            Command::NotifyFileAssociations
        );
        assert!(
            parse(&["--notify-file-associations", "extra"])
                .unwrap_err()
                .to_string()
                .contains("usage")
        );
    }

    #[test]
    fn parses_browse_paths_without_shell_interpretation() {
        for path in [
            "folder with spaces",
            "photos-été",
            "quote' dollar$ semicolon; [brackets]",
        ] {
            assert_eq!(
                parse(&[path]).unwrap(),
                Command::Browse(PathBuf::from(path))
            );
        }
    }

    #[test]
    fn parses_development_command_and_rejects_extra_arguments() {
        assert_eq!(
            parse(&["dev", "input.arw"]).unwrap(),
            Command::Develop {
                input: PathBuf::from("input.arw"),
                out_dir: PathBuf::from("."),
            }
        );
        assert_eq!(
            parse(&["dev", "input.arw", "output"]).unwrap(),
            Command::Develop {
                input: PathBuf::from("input.arw"),
                out_dir: PathBuf::from("output"),
            }
        );
        assert!(parse(&["dev"]).is_err());
        assert!(parse(&["dev", "input.arw", "output", "extra"]).is_err());
    }
}
