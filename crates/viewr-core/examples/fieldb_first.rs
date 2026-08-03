//! Field B first-call probe: fresh-process behavior across several files,
//! mimicking what viewr's metadata/thumb/develop jobs actually pay.
//! MODE=meta|load|thumb

use std::path::PathBuf;
use std::time::Instant;

fn ms(d: std::time::Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

fn main() {
    let dir = std::env::var("RAW_DIR").unwrap_or_else(|_| {
        "/Users/hunterchen/Documents/GitHub/viewr/testdata/real-raw-corpus".into()
    });
    let mode = std::env::var("MODE").unwrap_or_else(|_| "meta".into());
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .map(|e| e.to_string_lossy().eq_ignore_ascii_case("arw"))
                .unwrap_or(false)
        })
        .collect();
    files.sort();

    // Two passes over all files: pass 1 shows first-touch costs, pass 2 steady state.
    for pass in 1..=2 {
        for f in &files {
            let t = Instant::now();
            match mode.as_str() {
                "meta" => {
                    let m = viewr_core::decode::metadata(f).unwrap();
                    let el = t.elapsed();
                    println!(
                        "pass{pass} meta  {:>7.3} ms  {} ({})",
                        ms(el),
                        f.file_name().unwrap().to_string_lossy(),
                        m.camera
                    );
                }
                "load" => {
                    let d = viewr_core::decode::load(f).unwrap();
                    let el = t.elapsed();
                    println!(
                        "pass{pass} load  {:>7.3} ms  (open {:.3} / meta {:.3} / raw {:.3})  {}",
                        ms(el),
                        ms(d.t_open),
                        ms(d.t_metadata),
                        ms(d.t_raw_decode),
                        f.file_name().unwrap().to_string_lossy()
                    );
                }
                "thumb" => {
                    let r = viewr_core::decode::thumb_and_meta(f, 360).unwrap();
                    let el = t.elapsed();
                    println!(
                        "pass{pass} thumb {:>7.3} ms  {}x{}  {}",
                        ms(el),
                        r.thumb.width,
                        r.thumb.height,
                        f.file_name().unwrap().to_string_lossy()
                    );
                }
                _ => panic!("bad MODE"),
            }
        }
    }
}
