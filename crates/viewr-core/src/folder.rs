//! Folder scanning: find the raws, ordered by filename (Sony DSC/HCA
//! numbering ≡ capture order for bursts).

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};

use icu_casemap::CaseMapper;
use icu_normalizer::DecomposingNormalizerBorrowed;

#[derive(Debug, Clone)]
/// Immutable identity and display metadata for one scanned RAW file.
///
/// `size` and `mtime_ns` are a fast cache identity, not proof of content
/// equality. A file replaced while preserving both values can retain a stale
/// render-cache key.
pub struct FolderEntry {
    /// Native filesystem path to the RAW file. [`scan`] returns a stable
    /// absolute physical spelling; manually constructed entries should do the
    /// same when multiple processes share rating state.
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

/// Resolves an absolute RAW path to a stable physical spelling.
///
/// A missing RAW is accepted only when its immediate parent still resolves.
/// Relative paths and paths with an unresolved parent are ambiguous.
fn resolve_physical_path(path: &Path) -> io::Result<PathBuf> {
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "sidecar owner path must be absolute",
        ));
    }
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "RAW path has no parent"))?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "RAW path has no file name"))?;
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Ok(std::fs::canonicalize(parent)?.join(name))
        }
        Ok(_) => std::fs::canonicalize(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            Ok(std::fs::canonicalize(parent)?.join(name))
        }
        Err(error) => Err(error),
    }
}

/// Returns a stable absolute spelling for a physical path when possible.
///
/// Existing paths are canonicalized directly. For a temporarily missing leaf,
/// the deepest existing ancestor is canonicalized and the unresolved suffix is
/// appended. This lets background persistence normalize aliased parents
/// without turning a transiently missing RAW into a blocking caller error.
pub fn normalize_physical_path(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        match std::env::current_dir() {
            Ok(current) => current.join(path),
            Err(_) => return path.to_path_buf(),
        }
    };
    if let Ok(path) = resolve_physical_path(&absolute) {
        return path;
    }

    let mut unresolved = Vec::new();
    let mut ancestor = absolute.as_path();
    while let Some(name) = ancestor.file_name() {
        unresolved.push(name.to_os_string());
        let Some(parent) = ancestor.parent() else {
            break;
        };
        ancestor = parent;
        if let Ok(mut canonical) = std::fs::canonicalize(ancestor) {
            for component in unresolved.iter().rev() {
                canonical.push(component);
            }
            return canonical;
        }
    }
    absolute
}

/// Returns whether a stored path already names its normalized physical
/// spelling, allowing only Windows' ordinary-to-verbatim namespace prefix.
///
/// Older Windows releases stored paths such as `C:\photos\image.ARW`, while
/// [`std::fs::canonicalize`] returns `\\?\C:\photos\image.ARW`. That prefix
/// change is not an alias. Every remaining component must still match exactly
/// so junctions, symlinks, case changes, and drive-relative paths remain
/// untrusted legacy history.
pub(crate) fn path_spelling_is_stable(stored: &Path, normalized: &Path) -> bool {
    if stored.as_os_str() == normalized.as_os_str() {
        return true;
    }
    #[cfg(windows)]
    {
        differs_only_by_windows_verbatim_prefix(stored, normalized)
    }
    #[cfg(not(windows))]
    {
        false
    }
}

#[cfg(windows)]
fn differs_only_by_windows_verbatim_prefix(stored: &Path, normalized: &Path) -> bool {
    use std::os::windows::ffi::OsStrExt as _;
    use std::path::{Component, Prefix};

    let mut stored_components = stored.components();
    let mut normalized_components = normalized.components();
    let (Some(Component::Prefix(stored_prefix)), Some(Component::Prefix(normalized_prefix))) =
        (stored_components.next(), normalized_components.next())
    else {
        return false;
    };
    let prefix_matches = match (stored_prefix.kind(), normalized_prefix.kind()) {
        (Prefix::Disk(stored_drive), Prefix::VerbatimDisk(normalized_drive)) => {
            stored_drive.eq_ignore_ascii_case(&normalized_drive)
        }
        (
            Prefix::UNC(stored_server, stored_share),
            Prefix::VerbatimUNC(normalized_server, normalized_share),
        ) => stored_server == normalized_server && stored_share == normalized_share,
        _ => false,
    };
    if !prefix_matches {
        return false;
    }
    let stored_prefix_len = stored_prefix.as_os_str().encode_wide().count();
    let normalized_prefix_len = normalized_prefix.as_os_str().encode_wide().count();
    let stored_units = stored.as_os_str().encode_wide().collect::<Vec<_>>();
    let normalized_units = normalized.as_os_str().encode_wide().collect::<Vec<_>>();
    stored_units[stored_prefix_len..] == normalized_units[normalized_prefix_len..]
}

/// Returns the exact ordinary Windows spellings that older releases could
/// have stored for one current verbatim path.
///
/// Drive letters are returned in both ASCII cases because Windows treats
/// those prefixes case-insensitively. UNC server/share spelling and every
/// code unit after the prefix are preserved exactly.
pub(crate) fn legacy_windows_path_spellings(path: &Path) -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::{OsStrExt as _, OsStringExt as _};
        use std::path::{Component, Prefix};

        let Some(Component::Prefix(prefix)) = path.components().next() else {
            return Vec::new();
        };
        let path_units = path.as_os_str().encode_wide().collect::<Vec<_>>();
        let prefix_len = prefix.as_os_str().encode_wide().count();
        let tail = &path_units[prefix_len..];
        match prefix.kind() {
            Prefix::VerbatimDisk(drive) => {
                let mut spellings = Vec::with_capacity(2);
                for drive in [drive.to_ascii_uppercase(), drive.to_ascii_lowercase()] {
                    let mut units = vec![u16::from(drive), u16::from(b':')];
                    units.extend_from_slice(tail);
                    let spelling = PathBuf::from(OsString::from_wide(&units));
                    if spellings
                        .iter()
                        .all(|existing: &PathBuf| existing.as_os_str() != spelling.as_os_str())
                    {
                        spellings.push(spelling);
                    }
                }
                spellings
            }
            Prefix::VerbatimUNC(server, share) => {
                let mut units = vec![u16::from(b'\\'), u16::from(b'\\')];
                units.extend(server.encode_wide());
                units.push(u16::from(b'\\'));
                units.extend(share.encode_wide());
                units.extend_from_slice(tail);
                vec![PathBuf::from(OsString::from_wide(&units))]
            }
            _ => Vec::new(),
        }
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        Vec::new()
    }
}

/// Returns every exact database spelling in the compatible representation
/// family for a normalized path.
///
/// The first element is always `path`. On Windows, a verbatim drive or UNC
/// path is followed by the ordinary spellings that older releases stored.
/// Callers compare the raw [`OsStr`] values so `Path` component normalization
/// cannot merge distinct database keys.
pub(crate) fn path_spelling_family(path: &Path) -> Vec<PathBuf> {
    let mut family = vec![path.to_path_buf()];
    for spelling in legacy_windows_path_spellings(path) {
        if family
            .iter()
            .all(|existing| existing.as_os_str() != spelling.as_os_str())
        {
            family.push(spelling);
        }
    }
    family
}

/// Returns the database/in-memory ownership key for a RAW's XMP target.
///
/// The containing directory is probed rather than inferred from the operating
/// system, because case-sensitive and case-insensitive volumes can coexist.
/// An unresolved probe returns an error instead of activating a guessed owner.
pub(crate) fn sidecar_owner_key(path: &Path) -> io::Result<PathBuf> {
    let raw = resolve_physical_path(path)?;
    let case_insensitive = directory_is_case_insensitive(&raw)?;
    owner_from_raw(&raw, case_insensitive)
}

/// Reconstructs a physical RAW spelling from a validated XMP owner.
///
/// This is used only when a journaled RAW spelling no longer resolves, such
/// as after removal of a parent-directory symlink. The original RAW extension
/// is retained, and the candidate must derive back to the same owner.
pub(crate) fn raw_path_from_sidecar_owner(
    owner: &Path,
    journaled_raw: &Path,
) -> io::Result<PathBuf> {
    let extension = journaled_raw.extension().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "journaled RAW path has no extension",
        )
    })?;
    let raw = normalize_physical_path(&owner.with_extension(extension));
    if sidecar_owner_key(&raw)?.as_os_str() != owner.as_os_str() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "reconstructed RAW does not match its sidecar owner",
        ));
    }
    Ok(raw)
}

/// Returns a conservative filename identity for possible XMP-owner aliases.
///
/// Equal physical owners always have equal tokens, while equal tokens can
/// still belong to unrelated directories. Callers use this only to narrow a
/// legacy candidate scan before verifying each path against the filesystem.
pub(crate) fn sidecar_owner_collision_token(path: &Path) -> Option<OsString> {
    let stem = path.file_stem()?;
    let Some(stem) = stem.to_str() else {
        return Some(ascii_lower_native_name(stem));
    };
    let folded = CaseMapper::new().fold_string(stem);
    Some(
        DecomposingNormalizerBorrowed::new_nfd()
            .normalize(folded.as_ref())
            .into_owned()
            .into(),
    )
}

/// Resolves sidecar owners for a scanned folder with one physical-directory
/// and ASCII case-semantics probe per distinct parent spelling. Non-ASCII
/// transformations are verified against each exact RAW before they become an
/// ownership key.
pub(crate) fn sidecar_owner_keys(entries: &[FolderEntry]) -> Vec<Option<PathBuf>> {
    let mut parents: HashMap<PathBuf, (PathBuf, Option<bool>)> = HashMap::new();
    entries
        .iter()
        .map(|entry| {
            let parent = entry.path.parent()?.to_path_buf();
            let name = entry.path.file_name()?;
            if !parents.contains_key(&parent) {
                let physical_parent = std::fs::canonicalize(&parent).ok()?;
                parents.insert(parent.clone(), (physical_parent, None));
            }
            let (physical_parent, cached_case_insensitive) = parents.get_mut(&parent)?;
            let raw = physical_parent.join(name);
            let case_insensitive = if name.to_str().is_none() {
                // Some casefolding filesystems treat invalid native text as
                // opaque even when valid Unicode names are case-insensitive.
                // Probe this exact spelling and do not cache the result.
                directory_is_case_insensitive(&raw).ok()?
            } else if let Some(case_insensitive) = *cached_case_insensitive {
                case_insensitive
            } else {
                let case_insensitive = directory_is_case_insensitive(&raw).ok()?;
                *cached_case_insensitive = Some(case_insensitive);
                case_insensitive
            };
            owner_from_raw(&raw, case_insensitive).ok()
        })
        .collect()
}

/// Resolves the production batch of sidecar owners and returns its hit count.
#[cfg(feature = "benchmarks")]
#[doc(hidden)]
pub fn benchmark_sidecar_owner_keys(entries: &[FolderEntry]) -> usize {
    sidecar_owner_keys(entries).into_iter().flatten().count()
}

fn owner_from_raw(raw: &Path, case_insensitive: bool) -> io::Result<PathBuf> {
    let parent = raw
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "RAW path has no parent"))?;
    let name = raw
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "RAW path has no file name"))?;
    let mut owner = parent.join(owner_raw_name(raw, name, case_insensitive)?);
    owner.set_extension("xmp");
    Ok(owner)
}

/// Produces an internal owner spelling without changing the RAW or XMP path
/// used for I/O. Native names that are not valid Unicode preserve every
/// non-ASCII byte or code unit; ASCII is folded only after an exact
/// filesystem spelling probe establishes case insensitivity.
fn owner_raw_name(raw: &Path, name: &OsStr, case_insensitive: bool) -> io::Result<OsString> {
    let Some(name) = name.to_str() else {
        return Ok(if case_insensitive {
            ascii_lower_native_name(name)
        } else {
            name.to_os_string()
        });
    };
    if name.is_ascii() {
        return Ok(if case_insensitive {
            name.to_ascii_lowercase().into()
        } else {
            name.into()
        });
    }

    let mut owner_name = if case_insensitive {
        let folded = CaseMapper::new().fold_string(name);
        if folded == name || alternate_spelling_resolves_to_raw(raw, folded.as_ref())? {
            folded.into_owned()
        } else {
            // Some filesystems use a simple case map rather than Unicode's
            // full fold (for example, sharp-s can map to sharp-s rather than
            // `ss`). Accept that spelling only when the filesystem proves it.
            let lowercase = name.to_lowercase();
            if lowercase == name || alternate_spelling_resolves_to_raw(raw, &lowercase)? {
                lowercase
            } else {
                name.to_ascii_lowercase()
            }
        }
    } else {
        name.to_owned()
    };

    let decomposed = DecomposingNormalizerBorrowed::new_nfd().normalize(&owner_name);
    if decomposed != owner_name && alternate_spelling_resolves_to_raw(raw, decomposed.as_ref())? {
        owner_name = decomposed.into_owned();
    }
    Ok(owner_name.into())
}

#[cfg(unix)]
fn ascii_lower_native_name(name: &OsStr) -> OsString {
    use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};

    OsString::from_vec(name.as_bytes().iter().map(u8::to_ascii_lowercase).collect())
}

#[cfg(windows)]
fn ascii_lower_native_name(name: &OsStr) -> OsString {
    use std::os::windows::ffi::{OsStrExt as _, OsStringExt as _};

    OsString::from_wide(
        &name
            .encode_wide()
            .map(|unit| {
                if unit <= u16::from(u8::MAX) {
                    u16::from((unit as u8).to_ascii_lowercase())
                } else {
                    unit
                }
            })
            .collect::<Vec<_>>(),
    )
}

#[cfg(not(any(unix, windows)))]
fn ascii_lower_native_name(name: &OsStr) -> OsString {
    name.to_os_string()
}

fn alternate_spelling_resolves_to_raw(raw: &Path, alternate_name: &str) -> io::Result<bool> {
    let original_name = raw
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "RAW path has no file name"))?;
    if original_name == OsStr::new(alternate_name) {
        return Ok(true);
    }
    spelling_resolves_to_raw(raw, OsStr::new(alternate_name))
}

fn spelling_resolves_to_raw(raw: &Path, alternate_name: &OsStr) -> io::Result<bool> {
    let parent = raw
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "RAW path has no parent"))?;
    let original_name = raw
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "RAW path has no file name"))?;
    let canonical = std::fs::canonicalize(raw)?;
    let alternate = parent.join(alternate_name);
    match std::fs::symlink_metadata(&alternate) {
        // A separately named symlink on a spelling-sensitive filesystem can
        // resolve to the original RAW without making the spellings aliases.
        Ok(metadata) if metadata.file_type().is_symlink() => {
            let mut has_original = false;
            let mut has_alternate = false;
            for entry in std::fs::read_dir(parent)?.filter_map(Result::ok) {
                has_original |= entry.file_name() == original_name;
                has_alternate |= entry.file_name() == alternate_name;
            }
            Ok(!(has_original && has_alternate) && std::fs::canonicalize(alternate)? == canonical)
        }
        Ok(_) => std::fs::canonicalize(alternate).map(|alternate| alternate == canonical),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn directory_is_case_insensitive(path: &Path) -> io::Result<bool> {
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "RAW path has no file name"))?;
    // Requiring the RAW to resolve is intentional. If it vanished before a
    // queued rating reached the worker, guessing its case semantics could
    // merge distinct XMP targets on a case-sensitive volume.
    let alternate = toggled_ascii_case(name).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "cannot case-probe RAW filename",
        )
    })?;
    spelling_resolves_to_raw(path, &alternate)
}

#[cfg(unix)]
fn toggled_ascii_case(name: &OsStr) -> Option<OsString> {
    use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};

    let mut bytes = name.as_bytes().to_vec();
    let byte = bytes.iter_mut().find(|byte| byte.is_ascii_alphabetic())?;
    *byte = if byte.is_ascii_lowercase() {
        byte.to_ascii_uppercase()
    } else {
        byte.to_ascii_lowercase()
    };
    Some(OsString::from_vec(bytes))
}

#[cfg(windows)]
fn toggled_ascii_case(name: &OsStr) -> Option<OsString> {
    use std::os::windows::ffi::{OsStrExt as _, OsStringExt as _};

    let mut units = name.encode_wide().collect::<Vec<_>>();
    let unit = units
        .iter_mut()
        .find(|unit| (**unit as u8).is_ascii_alphabetic() && **unit <= u16::from(u8::MAX))?;
    *unit = if (*unit as u8).is_ascii_lowercase() {
        u16::from((*unit as u8).to_ascii_uppercase())
    } else {
        u16::from((*unit as u8).to_ascii_lowercase())
    };
    Some(OsString::from_wide(&units))
}

#[cfg(not(any(unix, windows)))]
fn toggled_ascii_case(name: &OsStr) -> Option<OsString> {
    let mut name = name.to_str()?.to_owned();
    let index = name.find(|character: char| character.is_ascii_alphabetic())?;
    let replacement = if name.as_bytes()[index].is_ascii_lowercase() {
        name.as_bytes()[index].to_ascii_uppercase()
    } else {
        name.as_bytes()[index].to_ascii_lowercase()
    };
    name.replace_range(index..=index, std::str::from_utf8(&[replacement]).ok()?);
    Some(name.into())
}

/// Scans one directory for regular ARW and DNG files in filename order.
///
/// Extension matching is ASCII case-insensitive. Hidden entries, AppleDouble
/// files, symlinks, non-files, unsupported extensions, and entries whose
/// metadata cannot be read are skipped. Returned paths use a canonical
/// directory spelling. The function does not recurse.
///
/// # Errors
///
/// Returns an error when the directory cannot be resolved or opened. Per-entry
/// iterator and metadata errors are treated as skipped entries.
pub fn scan(dir: &Path) -> io::Result<Vec<FolderEntry>> {
    // Canonicalizing once makes every ordinary entry path independent of the
    // caller's parent-directory casing or symlink spelling.
    let dir = std::fs::canonicalize(dir)?;
    let mut entries: Vec<FolderEntry> = std::fs::read_dir(dir)?
        .filter_map(Result::ok)
        .filter_map(|e| {
            let entry_path = e.path();
            let name = e.file_name().to_string_lossy().into_owned();
            // Skip dotfiles and AppleDouble ("._foo.ARW") droppings on SD cards.
            if name.starts_with('.') {
                return None;
            }
            let ext = entry_path.extension()?.to_string_lossy().to_lowercase();
            if !RAW_EXTENSIONS.contains(&ext.as_str()) {
                return None;
            }
            // `DirEntry::metadata` preserves the established behavior of
            // excluding RAW-file symlinks from the visible folder contents.
            let md = e.metadata().ok()?;
            if !md.is_file() {
                return None;
            }
            let mtime_ns = md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .and_then(duration_as_mtime_ns)
                .unwrap_or(0);
            Some(FolderEntry {
                path: entry_path,
                file_name: name,
                size: md.len(),
                mtime_ns,
            })
        })
        .collect();
    entries.sort_by(|a, b| a.file_name.cmp(&b.file_name));
    Ok(entries)
}

fn duration_as_mtime_ns(duration: std::time::Duration) -> Option<i64> {
    i64::try_from(duration.as_nanos()).ok()
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
    use super::{
        duration_as_mtime_ns, normalize_physical_path, outward_order, scan,
        sidecar_owner_collision_token, sidecar_owner_key, sidecar_owner_keys,
    };
    #[cfg(windows)]
    use super::{legacy_windows_path_spellings, path_spelling_family, path_spelling_is_stable};

    #[test]
    fn modification_timestamp_conversion_never_wraps() {
        assert_eq!(
            duration_as_mtime_ns(std::time::Duration::from_nanos(i64::MAX as u64)),
            Some(i64::MAX)
        );
        assert_eq!(
            duration_as_mtime_ns(std::time::Duration::from_nanos(i64::MAX as u64 + 1)),
            None
        );
    }

    #[test]
    fn collision_tokens_cover_extension_case_and_unicode_aliases() {
        assert_eq!(
            sidecar_owner_collision_token(std::path::Path::new("/p/Photo.ARW")),
            sidecar_owner_collision_token(std::path::Path::new("/p/photo.DNG"))
        );
        assert_eq!(
            sidecar_owner_collision_token(std::path::Path::new("/p/caf\u{e9}.ARW")),
            sidecar_owner_collision_token(std::path::Path::new("/p/cafe\u{301}.DNG"))
        );
        assert_ne!(
            sidecar_owner_collision_token(std::path::Path::new("/p/first.ARW")),
            sidecar_owner_collision_token(std::path::Path::new("/p/second.DNG"))
        );
    }

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
        let canonical_dir = std::fs::canonicalize(dir.path()).unwrap();
        assert_eq!(entries[0].path, canonical_dir.join("A001.DNG"));
        assert_eq!(entries[0].sidecar_path(), canonical_dir.join("A001.xmp"));
    }

    #[test]
    fn scan_reports_a_missing_directory() {
        let dir = tempfile::tempdir().unwrap();
        let error = scan(&dir.path().join("missing")).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    }

    #[cfg(unix)]
    #[test]
    fn scan_and_missing_descendants_normalize_a_symlinked_parent() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let physical = dir.path().join("physical");
        let alias = dir.path().join("alias");
        std::fs::create_dir(&physical).unwrap();
        std::fs::write(physical.join("photo.ARW"), b"raw").unwrap();
        symlink(&physical, &alias).unwrap();

        let physical_entries = scan(&physical).unwrap();
        let alias_entries = scan(&alias).unwrap();
        assert_eq!(alias_entries[0].path, physical_entries[0].path);
        assert_eq!(
            normalize_physical_path(&alias.join("missing/photo.ARW")),
            std::fs::canonicalize(&physical)
                .unwrap()
                .join("missing/photo.ARW")
        );
    }

    #[cfg(unix)]
    #[test]
    fn raw_file_symlinks_remain_excluded_from_folder_contents() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let target_dir = dir.path().join("target");
        let scan_dir = dir.path().join("scan");
        std::fs::create_dir(&target_dir).unwrap();
        std::fs::create_dir(&scan_dir).unwrap();
        let target = target_dir.join("physical.ARW");
        std::fs::write(&target, b"raw").unwrap();
        symlink(&target, scan_dir.join("display-name.ARW")).unwrap();

        let entries = scan(&scan_dir).unwrap();

        assert!(entries.is_empty());
    }

    #[cfg(any(target_os = "macos", windows))]
    #[test]
    fn scan_normalizes_case_aliases_on_case_insensitive_filesystems() {
        let dir = tempfile::tempdir().unwrap();
        let physical = dir.path().join("CaseAlias");
        let alias = dir.path().join("casealias");
        std::fs::create_dir(&physical).unwrap();
        std::fs::write(physical.join("Photo.ARW"), b"raw").unwrap();
        let Ok(alias_root) = std::fs::canonicalize(&alias) else {
            // Some supported installations deliberately use a case-sensitive
            // volume, where this spelling is not an alias.
            return;
        };
        assert_eq!(alias_root, std::fs::canonicalize(&physical).unwrap());

        assert_eq!(
            scan(&alias).unwrap()[0].path,
            scan(&physical).unwrap()[0].path
        );
    }

    #[cfg(windows)]
    #[test]
    fn stable_windows_spelling_accepts_only_a_verbatim_prefix_change() {
        use std::path::Path;

        assert!(path_spelling_is_stable(
            Path::new(r"C:\photos\photo.ARW"),
            Path::new(r"\\?\c:\photos\photo.ARW"),
        ));
        assert!(path_spelling_is_stable(
            Path::new(r"\\server\share\photos\photo.ARW"),
            Path::new(r"\\?\UNC\server\share\photos\photo.ARW"),
        ));
        assert!(!path_spelling_is_stable(
            Path::new(r"C:\alias\photo.ARW"),
            Path::new(r"\\?\C:\physical\photo.ARW"),
        ));
        assert!(!path_spelling_is_stable(
            Path::new(r"C:\Photos\photo.ARW"),
            Path::new(r"\\?\C:\photos\photo.ARW"),
        ));
        assert!(!path_spelling_is_stable(
            Path::new(r"C:\photos\.\photo.ARW"),
            Path::new(r"\\?\C:\photos\photo.ARW"),
        ));
        assert!(!path_spelling_is_stable(
            Path::new(r"C:/photos/photo.ARW"),
            Path::new(r"\\?\C:\photos\photo.ARW"),
        ));
        assert!(!path_spelling_is_stable(
            Path::new(r"\photos\photo.ARW"),
            Path::new(r"\\?\C:\photos\photo.ARW"),
        ));
        assert!(!path_spelling_is_stable(
            Path::new(r"\\.\C:\photos\photo.ARW"),
            Path::new(r"\\?\C:\photos\photo.ARW"),
        ));
        assert!(!path_spelling_is_stable(
            Path::new(r"\\?\C:\photos\\photo.ARW"),
            Path::new(r"\\?\C:\photos\photo.ARW"),
        ));
        assert!(!path_spelling_is_stable(
            Path::new(r"\\?\C:\photos\.\photo.ARW"),
            Path::new(r"\\?\C:\photos\photo.ARW"),
        ));
    }

    #[cfg(windows)]
    #[test]
    fn legacy_windows_spellings_rewrite_only_the_verbatim_prefix_losslessly() {
        use std::ffi::OsString;
        use std::os::windows::ffi::{OsStrExt as _, OsStringExt as _};
        use std::path::{Path, PathBuf};

        let drive_spellings = legacy_windows_path_spellings(Path::new(r"\\?\C:\photos\photo.ARW"));
        assert_eq!(drive_spellings.len(), 2);
        assert_eq!(
            drive_spellings[0]
                .as_os_str()
                .encode_wide()
                .collect::<Vec<_>>(),
            r"C:\photos\photo.ARW".encode_utf16().collect::<Vec<_>>()
        );
        assert_eq!(
            drive_spellings[1]
                .as_os_str()
                .encode_wide()
                .collect::<Vec<_>>(),
            r"c:\photos\photo.ARW".encode_utf16().collect::<Vec<_>>()
        );
        assert_ne!(
            drive_spellings[0].as_os_str(),
            drive_spellings[1].as_os_str()
        );
        assert_eq!(
            legacy_windows_path_spellings(Path::new(r"\\?\UNC\server\share\photos\photo.ARW")),
            [PathBuf::from(r"\\server\share\photos\photo.ARW")]
        );
        assert!(legacy_windows_path_spellings(Path::new(r"\\?\Volume{abc}\photo.ARW")).is_empty());
        assert!(legacy_windows_path_spellings(Path::new(r"\\.\C:\photos\photo.ARW")).is_empty());

        let mut malformed = r"\\?\C:\photos\".encode_utf16().collect::<Vec<_>>();
        malformed.push(0xd800);
        malformed.extend(".ARW".encode_utf16());
        let malformed = PathBuf::from(OsString::from_wide(&malformed));
        let ordinary = legacy_windows_path_spellings(&malformed);
        assert_eq!(ordinary.len(), 2);
        let ordinary_units = ordinary[0].as_os_str().encode_wide().collect::<Vec<_>>();
        assert_eq!(
            &ordinary_units[2..],
            &malformed.as_os_str().encode_wide().collect::<Vec<_>>()[6..]
        );
        assert!(path_spelling_is_stable(&ordinary[0], &malformed));
        let family = path_spelling_family(Path::new(r"\\?\C:\photos\photo.ARW"));
        assert_eq!(family.len(), 3);
        assert_eq!(
            family
                .iter()
                .map(|path| path.as_os_str().encode_wide().collect::<Vec<_>>())
                .collect::<Vec<_>>(),
            [
                r"\\?\C:\photos\photo.ARW"
                    .encode_utf16()
                    .collect::<Vec<_>>(),
                r"C:\photos\photo.ARW".encode_utf16().collect::<Vec<_>>(),
                r"c:\photos\photo.ARW".encode_utf16().collect::<Vec<_>>(),
            ]
        );
    }

    #[test]
    fn canonically_equivalent_sidecars_follow_the_filesystem_identity() {
        let dir = tempfile::tempdir().unwrap();
        let composed = dir.path().join("\u{e9}.ARW");
        let decomposed = dir.path().join("e\u{301}.DNG");
        std::fs::write(&composed, b"raw").unwrap();
        std::fs::write(&decomposed, b"raw").unwrap();

        let composed_sidecar = composed.with_extension("xmp");
        let decomposed_sidecar = decomposed.with_extension("xmp");
        std::fs::write(&composed_sidecar, b"probe").unwrap();
        let sidecars_alias = decomposed_sidecar.exists();
        std::fs::remove_file(&composed_sidecar).unwrap();

        let composed_owner = sidecar_owner_key(&composed).unwrap();
        let decomposed_owner = sidecar_owner_key(&decomposed).unwrap();
        if sidecars_alias {
            assert_eq!(composed_owner, decomposed_owner);
        } else {
            assert_ne!(composed_owner, decomposed_owner);
        }
        let entries = scan(dir.path()).unwrap();
        let owners = sidecar_owner_keys(&entries);
        if sidecars_alias {
            assert_eq!(owners[0], owners[1]);
        } else {
            assert_ne!(owners[0], owners[1]);
        }
        assert_eq!(composed.with_extension("xmp"), composed_sidecar);
        assert_eq!(decomposed.with_extension("xmp"), decomposed_sidecar);
    }

    #[test]
    fn unicode_casefold_aliases_follow_the_filesystem_identity() {
        let dir = tempfile::tempdir().unwrap();
        let sigma = dir.path().join("\u{3a3}.ARW");
        let final_sigma = dir.path().join("\u{3c2}.DNG");
        std::fs::write(&sigma, b"raw").unwrap();
        std::fs::write(&final_sigma, b"raw").unwrap();

        let sigma_sidecar = sigma.with_extension("xmp");
        let final_sigma_sidecar = final_sigma.with_extension("xmp");
        std::fs::write(&sigma_sidecar, b"probe").unwrap();
        let sidecars_alias = final_sigma_sidecar.exists();
        std::fs::remove_file(&sigma_sidecar).unwrap();

        let sigma_owner = sidecar_owner_key(&sigma).unwrap();
        let final_sigma_owner = sidecar_owner_key(&final_sigma).unwrap();
        if sidecars_alias {
            assert_eq!(sigma_owner, final_sigma_owner);
        } else {
            assert_ne!(sigma_owner, final_sigma_owner);
        }
        let entries = scan(dir.path()).unwrap();
        let owners = sidecar_owner_keys(&entries);
        if sidecars_alias {
            assert_eq!(owners[0], owners[1]);
        } else {
            assert_ne!(owners[0], owners[1]);
        }
    }

    #[test]
    fn unicode_simple_case_aliases_follow_the_filesystem_identity() {
        let dir = tempfile::tempdir().unwrap();
        let capital_sharp_s = dir.path().join("\u{1e9e}.ARW");
        let sharp_s = dir.path().join("\u{df}.DNG");
        std::fs::write(&capital_sharp_s, b"raw").unwrap();
        std::fs::write(&sharp_s, b"raw").unwrap();

        let capital_sidecar = capital_sharp_s.with_extension("xmp");
        let lowercase_sidecar = sharp_s.with_extension("xmp");
        std::fs::write(&capital_sidecar, b"probe").unwrap();
        let sidecars_alias = lowercase_sidecar.exists();
        std::fs::remove_file(&capital_sidecar).unwrap();

        let capital_owner = sidecar_owner_key(&capital_sharp_s).unwrap();
        let lowercase_owner = sidecar_owner_key(&sharp_s).unwrap();
        if sidecars_alias {
            assert_eq!(capital_owner, lowercase_owner);
        } else {
            assert_ne!(capital_owner, lowercase_owner);
        }
        let owners = sidecar_owner_keys(&scan(dir.path()).unwrap());
        if sidecars_alias {
            assert_eq!(owners[0], owners[1]);
        } else {
            assert_ne!(owners[0], owners[1]);
        }
    }

    #[cfg(unix)]
    #[test]
    fn normalization_probe_rejects_a_separately_named_symlink() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let composed = dir.path().join("\u{e9}.ARW");
        let decomposed = dir.path().join("e\u{301}.ARW");
        std::fs::write(&composed, b"raw").unwrap();
        if let Err(error) = symlink(&composed, &decomposed) {
            assert_eq!(
                error.kind(),
                std::io::ErrorKind::AlreadyExists,
                "unexpected normalization-probe setup failure: {error}"
            );
            return;
        }

        assert_eq!(
            sidecar_owner_key(&composed).unwrap(),
            composed.with_extension("xmp")
        );
    }

    #[cfg(unix)]
    #[test]
    fn native_owner_keys_fold_only_ascii_in_invalid_utf8() {
        use std::os::unix::ffi::OsStringExt as _;

        let name = std::ffi::OsString::from_vec(b"Photo-\xff.ARW".to_vec());
        let owner = super::owner_raw_name(std::path::Path::new("unused"), &name, true).unwrap();
        assert_eq!(
            owner,
            std::ffi::OsString::from_vec(b"photo-\xff.arw".to_vec())
        );
        assert_eq!(
            super::owner_raw_name(std::path::Path::new("unused"), &name, false).unwrap(),
            name
        );
    }

    #[cfg(unix)]
    #[test]
    fn batched_invalid_name_does_not_cache_semantics_for_a_later_valid_name() {
        use std::os::unix::ffi::OsStringExt as _;

        let dir = tempfile::tempdir().unwrap();
        let invalid = dir
            .path()
            .join(std::ffi::OsString::from_vec(b"Photo-\xff.ARW".to_vec()));
        let valid = dir.path().join("Photo.ARW");
        std::fs::write(&valid, b"raw").unwrap();
        let invalid_exists = std::fs::write(&invalid, b"raw").is_ok();
        let entries = vec![
            super::FolderEntry {
                path: invalid.clone(),
                file_name: "invalid".to_owned(),
                size: 3,
                mtime_ns: 0,
            },
            super::FolderEntry {
                path: valid.clone(),
                file_name: "valid".to_owned(),
                size: 3,
                mtime_ns: 0,
            },
        ];

        let owners = sidecar_owner_keys(&entries);
        assert_eq!(
            owners[0],
            invalid_exists.then(|| sidecar_owner_key(&invalid).unwrap())
        );
        assert_eq!(owners[1], Some(sidecar_owner_key(&valid).unwrap()));
    }

    #[cfg(windows)]
    #[test]
    fn native_owner_keys_fold_only_ascii_around_unpaired_utf16_units() {
        use std::os::windows::ffi::{OsStrExt as _, OsStringExt as _};

        let first = std::ffi::OsString::from_wide(&[
            u16::from(b'P'),
            u16::from(b'h'),
            u16::from(b'o'),
            u16::from(b't'),
            u16::from(b'o'),
            0xd800,
        ]);
        let second = std::ffi::OsString::from_wide(&[
            u16::from(b'P'),
            u16::from(b'h'),
            u16::from(b'o'),
            u16::from(b't'),
            u16::from(b'o'),
            0xd801,
        ]);

        let first_owner =
            super::owner_raw_name(std::path::Path::new("unused"), &first, true).unwrap();
        let second_owner =
            super::owner_raw_name(std::path::Path::new("unused"), &second, true).unwrap();
        let first_units = first_owner.encode_wide().collect::<Vec<_>>();
        let second_units = second_owner.encode_wide().collect::<Vec<_>>();
        let lowercase_photo = [
            u16::from(b'p'),
            u16::from(b'h'),
            u16::from(b'o'),
            u16::from(b't'),
            u16::from(b'o'),
        ];
        assert_eq!(&first_units[..5], &lowercase_photo);
        assert_eq!(&second_units[..5], &lowercase_photo);
        assert_eq!(first_units[5], 0xd800);
        assert_eq!(second_units[5], 0xd801);
        assert_ne!(first_owner, second_owner);
        assert_eq!(
            super::owner_raw_name(std::path::Path::new("unused"), &first, false).unwrap(),
            first
        );
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
