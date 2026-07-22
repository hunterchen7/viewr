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
    temporary.persist(path).map_err(|error| error.error)?;
    if durable {
        sync_parent(parent)?;
    }
    Ok(())
}

#[cfg(unix)]
fn sync_parent(parent: &Path) -> std::io::Result<()> {
    std::fs::File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent(_parent: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durable_replacement_publishes_complete_contents() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("rating.xmp");

        replace_durable(&path, b"first").unwrap();
        replace_durable(&path, b"replacement").unwrap();

        assert_eq!(std::fs::read(path).unwrap(), b"replacement");
    }
}
