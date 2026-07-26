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
    let existing_permissions = match path.symlink_metadata() {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "atomic replacement refuses a symbolic-link target",
            ));
        }
        Ok(metadata) if metadata.is_file() => Some(metadata.permissions()),
        Ok(_) => None,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    let mut temporary = tempfile::Builder::new()
        .prefix(".viewr-")
        .suffix(".tmp")
        .tempfile_in(parent)?;
    temporary.write_all(bytes)?;
    if let Some(permissions) = existing_permissions {
        temporary.as_file().set_permissions(permissions)?;
    }
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

    #[cfg(unix)]
    #[test]
    fn replacement_preserves_existing_file_permissions() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("rating.xmp");
        std::fs::write(&path, b"first").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();

        replace_durable(&path, b"replacement").unwrap();

        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
    }

    #[cfg(unix)]
    #[test]
    fn replacement_refuses_to_destroy_a_symbolic_link() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target.xmp");
        let link = directory.path().join("rating.xmp");
        std::fs::write(&target, b"target").unwrap();
        symlink(&target, &link).unwrap();

        let error = replace_durable(&link, b"replacement").unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert_eq!(std::fs::read(&target).unwrap(), b"target");
        assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
    }
}
