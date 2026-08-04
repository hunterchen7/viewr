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
mod image_info;
mod loupe;
#[cfg(target_os = "macos")]
mod macos_update;
mod pixels;
mod progressive_texture;
mod rating_groups;
mod settings;
mod texture_lru;
mod update;

use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use serde::Serialize;
use sha2::{Digest, Sha256};
use viewr_core::develop::DevelopTimings;
use viewr_core::develop::{Quality, develop};

#[derive(Debug, Eq, PartialEq)]
enum Command {
    #[cfg(target_os = "macos")]
    ApplyMacosUpdate {
        plan: PathBuf,
    },
    Browse(PathBuf),
    Develop {
        input: PathBuf,
        out_dir: PathBuf,
        output: DevelopOutput,
    },
    NotifyFileAssociations,
    PickFolder,
    PrintUsage,
    PrintVersion,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
enum DevelopOutput {
    Human,
    Json,
}

fn main() -> Result<()> {
    run(parse_command(std::env::args_os().skip(1))?)
}

fn parse_command(args: impl IntoIterator<Item = OsString>) -> Result<Command> {
    let args: Vec<OsString> = args.into_iter().collect();
    match args.as_slice() {
        [] => Ok(Command::PrintUsage),
        [flag] if flag == "--version" || flag == "-V" => Ok(Command::PrintVersion),
        [flag] if flag == "--notify-file-associations" => Ok(Command::NotifyFileAssociations),
        [flag] if flag == "--pick-folder" => Ok(Command::PickFolder),
        #[cfg(target_os = "macos")]
        [flag, plan] if flag == "--apply-macos-update" => Ok(Command::ApplyMacosUpdate {
            plan: PathBuf::from(plan),
        }),
        [command, flag, input] if command == "dev" && flag == "--json" => Ok(Command::Develop {
            input: PathBuf::from(input),
            out_dir: PathBuf::from("."),
            output: DevelopOutput::Json,
        }),
        [command, flag, input, out_dir] if command == "dev" && flag == "--json" => {
            Ok(Command::Develop {
                input: PathBuf::from(input),
                out_dir: PathBuf::from(out_dir),
                output: DevelopOutput::Json,
            })
        }
        [command, input] if command == "dev" => Ok(Command::Develop {
            input: PathBuf::from(input),
            out_dir: PathBuf::from("."),
            output: DevelopOutput::Human,
        }),
        [command, input, out_dir] if command == "dev" => Ok(Command::Develop {
            input: PathBuf::from(input),
            out_dir: PathBuf::from(out_dir),
            output: DevelopOutput::Human,
        }),
        [command, ..] if command == "dev" => {
            bail!("usage: viewr dev [--json] <file.arw> [out-dir]");
        }
        [flag, ..] if flag == "--pick-folder" => {
            bail!("usage: viewr --pick-folder");
        }
        #[cfg(target_os = "macos")]
        [flag, ..] if flag == "--apply-macos-update" => {
            bail!("invalid internal macOS update command");
        }
        [flag, ..] if flag == "--notify-file-associations" => {
            bail!("usage: viewr --notify-file-associations");
        }
        [flag, ..] if flag == "--version" || flag == "-V" => {
            bail!("usage: viewr --version");
        }
        [path] => Ok(Command::Browse(PathBuf::from(path))),
        _ => bail!("usage: viewr <folder|file.arw>"),
    }
}

fn run(command: Command) -> Result<()> {
    match command {
        #[cfg(target_os = "macos")]
        Command::ApplyMacosUpdate { plan } => {
            macos_update::apply(&plan).context("failed to apply the macOS update")
        }
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
        Command::Develop {
            input,
            out_dir,
            output,
        } => spike(&input, &out_dir, output),
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
            eprintln!("       viewr dev --json <file> [out]  emit one benchmark JSON record");
            Ok(())
        }
        Command::PrintVersion => {
            println!("viewr {}", env!("CARGO_PKG_VERSION"));
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

#[derive(Serialize)]
struct StageReport {
    total_us: u64,
    rescale_us: u64,
    demosaic_us: u64,
    calibrate_us: u64,
    gamma_pack_us: u64,
}

impl StageReport {
    fn new(total: Duration, timings: DevelopTimings) -> Self {
        Self {
            total_us: duration_us(total),
            rescale_us: duration_us(timings.rescale),
            demosaic_us: duration_us(timings.demosaic),
            calibrate_us: duration_us(timings.calibrate),
            gamma_pack_us: duration_us(timings.gamma_pack),
        }
    }
}

#[derive(Serialize)]
struct DevelopReport {
    schema_version: u32,
    operation: &'static str,
    viewr_version: &'static str,
    input: String,
    input_bytes: u64,
    input_sha256: String,
    make: String,
    model: String,
    raw_width: usize,
    raw_height: usize,
    raw_bits_per_sample: usize,
    browse_width: u32,
    browse_height: u32,
    full_width: u32,
    full_height: u32,
    logical_cpus: usize,
    rayon_num_threads_env: Option<String>,
    cache_condition: &'static str,
    open_parse_us: u64,
    metadata_us: u64,
    entropy_decode_us: u64,
    clone_mosaic_us: u64,
    browse: StageReport,
    full: StageReport,
    encode_browse_us: u64,
    encode_full_us: u64,
    write_browse_us: u64,
    write_full_us: u64,
    pipeline_total_us: u64,
    audit_overhead_us: u64,
    browse_rgba_sha256: String,
    full_rgba_sha256: String,
    browse_jpeg_sha256: String,
    full_jpeg_sha256: String,
    browse_jpeg_bytes: usize,
    full_jpeg_bytes: usize,
}

fn duration_us(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

fn sha256_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("failed to hash benchmark input {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Decode + develop both tiers, dump JPEGs, and report pipeline timings.
fn spike(input: &Path, out_dir: &Path, output: DevelopOutput) -> Result<()> {
    let file_bytes = std::fs::metadata(input)?.len();

    let t_total = Instant::now();
    let decoded = viewr_core::decode::load(input).context("decode failed")?;
    let raw = decoded.raw;

    let make = raw.clean_make.clone();
    let model = raw.clean_model.clone();
    let raw_width = raw.width;
    let raw_height = raw.height;
    let raw_bits_per_sample = raw.bps;

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

    std::fs::create_dir_all(out_dir)?;
    let stem = input.file_stem().unwrap_or_default().to_string_lossy();
    let browse_path = out_dir.join(format!("{stem}.browse.jpg"));
    let full_path = out_dir.join(format!("{stem}.full.jpg"));
    let t = Instant::now();
    let browse_jpeg = viewr_core::jobs::encode_jpeg(&browse, viewr_core::jobs::CACHE_JPEG_QUALITY)
        .map_err(anyhow::Error::msg)?;
    let t_encode_browse = t.elapsed();
    let t = Instant::now();
    let full_jpeg = viewr_core::jobs::encode_jpeg(&full, viewr_core::jobs::CACHE_JPEG_QUALITY)
        .map_err(anyhow::Error::msg)?;
    let t_encode_full = t.elapsed();
    let t = Instant::now();
    std::fs::write(&browse_path, &browse_jpeg)
        .with_context(|| format!("failed to write diagnostic JPEG {}", browse_path.display()))?;
    let t_write_browse = t.elapsed();
    let t = Instant::now();
    std::fs::write(&full_path, &full_jpeg)
        .with_context(|| format!("failed to write diagnostic JPEG {}", full_path.display()))?;
    let t_write_full = t.elapsed();
    let pipeline_total = t_total.elapsed();

    if output == DevelopOutput::Human {
        println!(
            "{} — {} {} | {}x{} CFA, {} bpp, {:.1} MB file",
            input.file_name().unwrap_or_default().to_string_lossy(),
            make,
            model,
            raw_width,
            raw_height,
            raw_bits_per_sample,
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
        println!("  open+parse    {:>8.1?}", decoded.t_open);
        println!("  metadata      {:>8.1?}", decoded.t_metadata);
        println!("  entropy decode{:>8.1?}", decoded.t_raw_decode);
        println!("  clone mosaic  {:>8.1?}", t_clone);
        println!(
            "  browse tier   {:>8.1?}  ({}x{}; rescale {:.1?}, demosaic {:.1?}, calibrate {:.1?}, pack {:.1?})",
            t_browse,
            browse.width,
            browse.height,
            bt.rescale,
            bt.demosaic,
            bt.calibrate,
            bt.gamma_pack
        );
        println!(
            "  full tier     {:>8.1?}  ({}x{}; rescale {:.1?}, demosaic {:.1?}, calibrate {:.1?}, pack {:.1?})",
            t_full, full.width, full.height, ft.rescale, ft.demosaic, ft.calibrate, ft.gamma_pack
        );
        println!("  encode 2 jpg  {:>8.1?}", t_encode_browse + t_encode_full);
        println!("  write 2 jpg   {:>8.1?}", t_write_browse + t_write_full);
        println!("  TOTAL         {:>8.1?}", pipeline_total);
        println!(
            "  wrote {} and {}",
            browse_path.display(),
            full_path.display()
        );
        return Ok(());
    }

    // Correctness hashes run after the timed pipeline, so the benchmark
    // remains comparable to human-mode fresh-process measurements.
    let t_audit = Instant::now();
    let input_sha256 = sha256_file(input)?;
    let browse_rgba_sha256 = sha256_bytes(&browse.rgba);
    let full_rgba_sha256 = sha256_bytes(&full.rgba);
    let browse_jpeg_sha256 = sha256_bytes(&browse_jpeg);
    let full_jpeg_sha256 = sha256_bytes(&full_jpeg);
    let audit_overhead = t_audit.elapsed();
    let report = DevelopReport {
        schema_version: 1,
        operation: "dev.raw_pipeline",
        viewr_version: env!("CARGO_PKG_VERSION"),
        input: input.display().to_string(),
        input_bytes: file_bytes,
        input_sha256,
        make,
        model,
        raw_width,
        raw_height,
        raw_bits_per_sample,
        browse_width: browse.width,
        browse_height: browse.height,
        full_width: full.width,
        full_height: full.height,
        logical_cpus: std::thread::available_parallelism().map_or(1, usize::from),
        rayon_num_threads_env: std::env::var("RAYON_NUM_THREADS").ok(),
        cache_condition: "fresh process; operating-system page cache uncontrolled",
        open_parse_us: duration_us(decoded.t_open),
        metadata_us: duration_us(decoded.t_metadata),
        entropy_decode_us: duration_us(decoded.t_raw_decode),
        clone_mosaic_us: duration_us(t_clone),
        browse: StageReport::new(t_browse, bt),
        full: StageReport::new(t_full, ft),
        encode_browse_us: duration_us(t_encode_browse),
        encode_full_us: duration_us(t_encode_full),
        write_browse_us: duration_us(t_write_browse),
        write_full_us: duration_us(t_write_full),
        pipeline_total_us: duration_us(pipeline_total),
        audit_overhead_us: duration_us(audit_overhead),
        browse_rgba_sha256,
        full_rgba_sha256,
        browse_jpeg_sha256,
        full_jpeg_sha256,
        browse_jpeg_bytes: browse_jpeg.len(),
        full_jpeg_bytes: full_jpeg.len(),
    };
    println!("{}", serde_json::to_string(&report)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Command, DevelopOutput, parse_command};
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
    fn parses_version_commands() {
        assert_eq!(parse(&["--version"]).unwrap(), Command::PrintVersion);
        assert_eq!(parse(&["-V"]).unwrap(), Command::PrintVersion);
        assert!(parse(&["--version", "extra"]).is_err());
        assert!(parse(&["-V", "extra"]).is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn parses_only_the_exact_internal_macos_update_command() {
        assert_eq!(
            parse(&["--apply-macos-update", "/tmp/update-plan.json"]).unwrap(),
            Command::ApplyMacosUpdate {
                plan: PathBuf::from("/tmp/update-plan.json"),
            }
        );
        assert!(parse(&["--apply-macos-update"]).is_err());
        assert!(parse(&["--apply-macos-update", "plan", "extra"]).is_err());
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
                output: DevelopOutput::Human,
            }
        );
        assert_eq!(
            parse(&["dev", "input.arw", "output"]).unwrap(),
            Command::Develop {
                input: PathBuf::from("input.arw"),
                out_dir: PathBuf::from("output"),
                output: DevelopOutput::Human,
            }
        );
        assert_eq!(
            parse(&["dev", "--json", "input.arw"]).unwrap(),
            Command::Develop {
                input: PathBuf::from("input.arw"),
                out_dir: PathBuf::from("."),
                output: DevelopOutput::Json,
            }
        );
        assert_eq!(
            parse(&["dev", "--json", "input.arw", "output"]).unwrap(),
            Command::Develop {
                input: PathBuf::from("input.arw"),
                out_dir: PathBuf::from("output"),
                output: DevelopOutput::Json,
            }
        );
        assert!(parse(&["dev"]).is_err());
        assert!(parse(&["dev", "input.arw", "output", "extra"]).is_err());
    }
}
