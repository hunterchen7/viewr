//! Folder scanning: find the raws, ordered by filename (Sony DSC/HCA
//! numbering ≡ capture order for bursts).

use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct FolderEntry {
    pub path: PathBuf,
    pub file_name: String,
    /// File size + mtime feed the disk-cache key: any change to the raw
    /// invalidates its cached develops.
    pub size: u64,
    pub mtime_ns: i64,
}

impl FolderEntry {
    /// Lightroom-convention sidecar path: `HCA04696.ARW` → `HCA04696.xmp`.
    pub fn sidecar_path(&self) -> PathBuf {
        self.path.with_extension("xmp")
    }
}

/// Extensions we open, lowercase. ARW first-class; DNG decodes via the same
/// rawler path so it comes for free.
const RAW_EXTENSIONS: &[&str] = &["arw", "dng"];

pub fn scan(dir: &Path) -> io::Result<Vec<FolderEntry>> {
    let mut entries: Vec<FolderEntry> = std::fs::read_dir(dir)?
        .filter_map(Result::ok)
        .filter_map(|e| {
            let path = e.path();
            let name = e.file_name().to_string_lossy().into_owned();
            // Skip dotfiles and AppleDouble ("._foo.ARW") droppings on SD cards.
            if name.starts_with('.') {
                return None;
            }
            let ext = path.extension()?.to_string_lossy().to_lowercase();
            if !RAW_EXTENSIONS.contains(&ext.as_str()) {
                return None;
            }
            let md = e.metadata().ok()?;
            if !md.is_file() {
                return None;
            }
            let mtime_ns = md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos() as i64)
                .unwrap_or(0);
            Some(FolderEntry {
                path,
                file_name: name,
                size: md.len(),
                mtime_ns,
            })
        })
        .collect();
    entries.sort_by(|a, b| a.file_name.cmp(&b.file_name));
    Ok(entries)
}

/// Iteration order for background work: outward from `start`, forward-biased
/// ~3:1 (three ahead for every one behind). This is the same expanding wave
/// used by the prefetch scheduler.
pub fn outward_order(len: usize, start: usize) -> Vec<usize> {
    let mut order = Vec::with_capacity(len);
    let mut fwd = start + 1;
    let mut back = start.checked_sub(1);
    order.push(start.min(len.saturating_sub(1)));
    while order.len() < len {
        for _ in 0..3 {
            if fwd < len {
                order.push(fwd);
                fwd += 1;
            }
        }
        if let Some(b) = back {
            order.push(b);
            back = b.checked_sub(1);
        }
        if fwd >= len && back.is_none() {
            break;
        }
    }
    order
}

#[cfg(test)]
mod tests {
    use super::outward_order;

    #[test]
    fn outward_is_forward_biased() {
        let order = outward_order(10, 3);
        assert_eq!(order[0], 3);
        // First wave: three forward, then one back.
        assert_eq!(&order[1..5], &[4, 5, 6, 2]);
        // Everything visited exactly once.
        let mut sorted = order.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, (0..10).collect::<Vec<_>>());
    }

    #[test]
    fn outward_handles_edges() {
        assert_eq!(outward_order(1, 0), vec![0]);
        let order = outward_order(5, 4);
        assert_eq!(order[0], 4);
        let mut sorted = order.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, (0..5).collect::<Vec<_>>());
    }
}
