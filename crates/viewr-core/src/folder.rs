//! Folder scanning: find the raws, ordered by filename (Sony DSC/HCA
//! numbering ≡ capture order for bursts).

use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
/// Immutable identity and display metadata for one scanned RAW file.
///
/// `size` and `mtime_ns` are a fast cache identity, not proof of content
/// equality. A file replaced while preserving both values can retain a stale
/// render-cache key.
pub struct FolderEntry {
    /// Native filesystem path to the RAW file.
    pub path: PathBuf,
    /// Lossy display name used for deterministic lexical sorting.
    pub file_name: String,
    /// File size + mtime feed the disk-cache key: any change to the raw
    /// invalidates its cached develops.
    pub size: u64,
    /// Modification time in nanoseconds since the Unix epoch, or zero when the
    /// filesystem does not provide a usable timestamp.
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

/// Scans one directory for regular ARW and DNG files in filename order.
///
/// Extension matching is ASCII case-insensitive. Hidden entries, AppleDouble
/// files, non-files, unsupported extensions, and entries whose metadata cannot
/// be read are skipped. The function does not recurse.
///
/// # Errors
///
/// Returns the error from opening the directory itself. Per-entry iterator and
/// metadata errors are treated as skipped entries.
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
    if len == 0 {
        return Vec::new();
    }
    let start = start.min(len - 1);
    let mut order = Vec::with_capacity(len);
    let mut fwd = start + 1;
    let mut back = start.checked_sub(1);
    order.push(start);
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
    use super::{outward_order, scan};

    #[test]
    fn scan_finds_supported_files_with_sorted_metadata() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("C003.ArW"), b"ccc").unwrap();
        std::fs::write(dir.path().join("A001.DNG"), b"a").unwrap();
        std::fs::write(dir.path().join("B002.arw"), b"bb").unwrap();

        // Unsupported files, hidden files, AppleDouble files, and directories
        // that merely have a raw extension must not enter the scan.
        std::fs::write(dir.path().join("notes.txt"), b"not a raw").unwrap();
        std::fs::write(dir.path().join(".hidden.ARW"), b"hidden").unwrap();
        std::fs::write(dir.path().join("._A001.ARW"), b"metadata").unwrap();
        std::fs::create_dir(dir.path().join("nested.dng")).unwrap();

        let entries = scan(dir.path()).unwrap();
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.file_name.as_str())
                .collect::<Vec<_>>(),
            ["A001.DNG", "B002.arw", "C003.ArW"]
        );
        assert_eq!(
            entries.iter().map(|entry| entry.size).collect::<Vec<_>>(),
            [1, 2, 3]
        );
        assert!(entries.iter().all(|entry| entry.mtime_ns > 0));
        assert_eq!(entries[0].path, dir.path().join("A001.DNG"));
        assert_eq!(entries[0].sidecar_path(), dir.path().join("A001.xmp"));
    }

    #[test]
    fn scan_reports_a_missing_directory() {
        let dir = tempfile::tempdir().unwrap();
        let error = scan(&dir.path().join("missing")).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    }

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
        assert!(outward_order(0, 0).is_empty());
        assert!(outward_order(0, usize::MAX).is_empty());
        assert_eq!(outward_order(1, 0), vec![0]);
        let order = outward_order(5, 4);
        assert_eq!(order[0], 4);
        let mut sorted = order.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, (0..5).collect::<Vec<_>>());
    }

    #[test]
    fn outward_is_a_permutation_for_all_small_inputs() {
        for len in 0usize..=128 {
            for start in 0..=len.saturating_add(2) {
                let order = outward_order(len, start);
                assert_eq!(order.len(), len, "len={len}, start={start}");
                assert!(
                    order.iter().all(|&index| index < len),
                    "len={len}, start={start}, order={order:?}"
                );
                let mut sorted = order;
                sorted.sort_unstable();
                sorted.dedup();
                assert_eq!(sorted, (0..len).collect::<Vec<_>>());
            }
        }
    }
}
