//! Same-directory temporary writes with cross-platform atomic replacement.

use std::io::Write as _;
use std::path::Path;

/// Replace `path` atomically. The temporary file is created in the target
/// directory so persistence cannot cross filesystem boundaries.
pub(crate) fn replace(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    replace_inner(path, bytes, false)
}

/// As [`replace`], but flush the file contents before publishing the name.
/// Used for user-authored sidecars where durability matters more than cache
/// write throughput.
pub(crate) fn replace_durable(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    replace_inner(path, bytes, true)
}

fn replace_inner(path: &Path, bytes: &[u8], durable: bool) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "atomic replacement requires a parent directory",
        )
    })?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".viewr-")
        .suffix(".tmp")
        .tempfile_in(parent)?;
    temporary.write_all(bytes)?;
    if durable {
        temporary.as_file().sync_all()?;
    }
    temporary
        .persist(path)
        .map(|_| ())
        .map_err(|error| error.error)
}
