//! Verified macOS application-bundle staging and shell-free replacement.
//!
//! The graphical process prepares the update beside its final destination,
//! then starts the verified staged binary in helper mode. A file lock keeps
//! the helper from replacing the bundle until the graphical process exits.

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{self, Write as _};
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use flate2::read::GzDecoder;
use semver::Version;
use serde::{Deserialize, Serialize};

use crate::update::UpdateError;

const APP_NAME: &str = "Viewr.app";
const BUNDLE_IDENTIFIER: &str = "com.hunterchen.viewr";
const MAX_ARCHIVE_ENTRIES: usize = 4096;
const MAX_EXPANDED_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_PLAN_BYTES: u64 = 64 * 1024;
const LAUNCH_PROBE_DELAY: Duration = Duration::from_millis(750);
const LSREGISTER: &str = "/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister";

#[derive(Clone, Debug)]
pub(crate) struct PreparedUpdate {
    helper_path: PathBuf,
    plan_path: PathBuf,
    lock_path: PathBuf,
    log_path: PathBuf,
    staging_root: PathBuf,
}

#[derive(Debug, Deserialize, Serialize)]
struct UpdatePlan {
    expected_version: String,
    previous_bundle: PathBuf,
    target_bundle: PathBuf,
    staged_bundle: PathBuf,
    backup_bundle: PathBuf,
    staging_root: PathBuf,
    lock_path: PathBuf,
    relaunch_arguments: Vec<String>,
}

pub(crate) fn prepare(
    archive_path: &Path,
    version: &Version,
) -> Result<PreparedUpdate, UpdateError> {
    let executable = std::env::current_exe()?;
    let previous_bundle = app_bundle_for_executable(&executable).ok_or_else(|| {
        UpdateError::InvalidRelease(
            "Viewr must be running from a Viewr.app bundle to update itself".into(),
        )
    })?;
    let target_bundle = update_target(&previous_bundle)?;
    let target_parent = target_bundle.parent().ok_or_else(|| {
        UpdateError::InvalidRelease("the application update target has no parent".into())
    })?;
    ensure_real_directory(target_parent)?;

    let staging = tempfile::Builder::new()
        .prefix(".Viewr-update-")
        .tempdir_in(target_parent)?;
    std::fs::set_permissions(staging.path(), std::fs::Permissions::from_mode(0o700))?;
    extract_app_archive(archive_path, staging.path())?;
    let staged_bundle = staging.path().join(APP_NAME);
    validate_app_bundle(&staged_bundle, version)?;

    let nonce = format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    let backup_bundle = target_parent.join(format!(".Viewr-backup-{nonce}.app"));
    if backup_bundle.symlink_metadata().is_ok() {
        return Err(UpdateError::InvalidRelease(
            "the application update backup path already exists".into(),
        ));
    }
    let plan_path = staging.path().join("update-plan.json");
    let lock_path = staging.path().join("parent.lock");
    let log_path = staging.path().join("apply.log");
    create_private_file(&lock_path, b"")?;
    let relaunch_arguments = std::env::args_os()
        .skip(1)
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect();
    let plan = UpdatePlan {
        expected_version: version.to_string(),
        previous_bundle,
        target_bundle,
        staged_bundle: staged_bundle.clone(),
        backup_bundle,
        staging_root: staging.path().to_owned(),
        lock_path: lock_path.clone(),
        relaunch_arguments,
    };
    let bytes = serde_json::to_vec(&plan)?;
    create_private_file(&plan_path, &bytes)?;
    sync_directory(staging.path())?;

    let staging_root = staging.keep();
    let helper_path = staged_bundle.join("Contents/MacOS/viewr-bin");
    debug_assert!(helper_path.starts_with(&staging_root));
    Ok(PreparedUpdate {
        helper_path,
        plan_path,
        lock_path,
        log_path,
        staging_root,
    })
}

pub(crate) fn discard(prepared: PreparedUpdate) {
    if prepared
        .staging_root
        .file_name()
        .is_some_and(|name| name.to_string_lossy().starts_with(".Viewr-update-"))
    {
        let _ = std::fs::remove_dir_all(prepared.staging_root);
    }
}

pub(crate) fn spawn(prepared: &PreparedUpdate) -> Result<File, UpdateError> {
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&prepared.lock_path)?;
    lock.lock()?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&prepared.log_path)?;
    let stderr = log.try_clone()?;
    Command::new(&prepared.helper_path)
        .arg("--apply-macos-update")
        .arg(&prepared.plan_path)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(stderr))
        .spawn()?;
    Ok(lock)
}

pub(crate) fn apply(plan_path: &Path) -> Result<(), UpdateError> {
    let plan = read_plan(plan_path)?;
    validate_plan(&plan, plan_path)?;
    let _application_lock = lock_application_updates()?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&plan.lock_path)?;
    lock.lock()?;

    let version = Version::parse(&plan.expected_version).map_err(|_| {
        UpdateError::InvalidRelease("the prepared update version is invalid".into())
    })?;
    validate_app_bundle(&plan.staged_bundle, &version)?;
    if validate_app_bundle(&plan.target_bundle, &version).is_ok() {
        register_application(&plan)?;
        probe_relaunch(&plan)?;
        remove_staging_root(&plan)?;
        return Ok(());
    }
    apply_transaction(&plan, &version)
}

fn lock_application_updates() -> Result<File, UpdateError> {
    let directory = dirs::cache_dir()
        .ok_or_else(|| io::Error::other("the cache directory is unavailable"))?
        .join("viewr")
        .join("updates")
        .join("locks");
    crate::update::ensure_private_directory(&directory)?;
    let lock = crate::update::open_lock_file(&directory.join("apply.lock"))?;
    lock.lock()?;
    Ok(lock)
}

fn apply_transaction(plan: &UpdatePlan, version: &Version) -> Result<(), UpdateError> {
    let had_target = match plan.target_bundle.symlink_metadata() {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => true,
        Ok(_) => {
            return Err(UpdateError::InvalidRelease(
                "the existing Viewr application is not a regular bundle".into(),
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => return Err(error.into()),
    };
    if plan.backup_bundle.symlink_metadata().is_ok() {
        return Err(UpdateError::InvalidRelease(
            "the update backup path is no longer empty".into(),
        ));
    }

    if had_target {
        std::fs::rename(&plan.target_bundle, &plan.backup_bundle)?;
    }
    if let Err(error) = std::fs::rename(&plan.staged_bundle, &plan.target_bundle) {
        if had_target {
            let _ = std::fs::rename(&plan.backup_bundle, &plan.target_bundle);
        }
        return Err(error.into());
    }
    sync_directory(
        plan.target_bundle
            .parent()
            .expect("validated target has a parent"),
    )?;

    let installed =
        validate_app_bundle(&plan.target_bundle, version).and_then(|()| register_application(plan));
    if let Err(error) = installed {
        rollback(plan, had_target);
        return Err(error);
    }

    if let Err(error) = probe_relaunch(plan) {
        rollback(plan, had_target);
        return Err(error);
    }

    if had_target {
        remove_verified_backup(plan)?;
    }
    remove_staging_root(plan)?;
    Ok(())
}

fn rollback(plan: &UpdatePlan, had_target: bool) {
    let failed_bundle = plan.staging_root.join("failed-Viewr.app");
    let _ = std::fs::rename(&plan.target_bundle, &failed_bundle);
    if had_target {
        let _ = std::fs::rename(&plan.backup_bundle, &plan.target_bundle);
        let _ = run_checked(Command::new(LSREGISTER).arg("-f").arg(&plan.target_bundle));
    }
    if plan.previous_bundle != plan.target_bundle {
        let _ = run_checked(
            Command::new(LSREGISTER)
                .arg("-f")
                .arg(&plan.previous_bundle),
        );
    }
    if let Some(parent) = plan.target_bundle.parent() {
        let _ = sync_directory(parent);
    }
}

fn relaunch(plan: &UpdatePlan) -> Result<Child, UpdateError> {
    let executable = plan.target_bundle.join("Contents/MacOS/viewr-bin");
    let mut command = Command::new(executable);
    if plan.relaunch_arguments.is_empty() {
        command.arg("--pick-folder");
    } else {
        command.args(&plan.relaunch_arguments);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(UpdateError::from)
}

fn probe_relaunch(plan: &UpdatePlan) -> Result<(), UpdateError> {
    let mut child = relaunch(plan)?;
    thread::sleep(LAUNCH_PROBE_DELAY);
    match child.try_wait()? {
        None => Ok(()),
        Some(status) => Err(UpdateError::InvalidRelease(format!(
            "the updated Viewr exited during launch ({status})"
        ))),
    }
}

fn register_application(plan: &UpdatePlan) -> Result<(), UpdateError> {
    if plan.previous_bundle != plan.target_bundle {
        let _ = run_checked(
            Command::new(LSREGISTER)
                .arg("-u")
                .arg(&plan.previous_bundle),
        );
    }
    run_checked(Command::new(LSREGISTER).arg("-f").arg(&plan.target_bundle))
}

fn run_checked(command: &mut Command) -> Result<(), UpdateError> {
    let output = command.output()?;
    if output.status.success() {
        Ok(())
    } else {
        let message = String::from_utf8_lossy(&output.stderr);
        Err(UpdateError::InvalidRelease(format!(
            "update helper command failed: {}",
            message.trim()
        )))
    }
}

fn read_plan(path: &Path) -> Result<UpdatePlan, UpdateError> {
    let metadata = path.symlink_metadata()?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > MAX_PLAN_BYTES {
        return Err(UpdateError::InvalidRelease(
            "the prepared update plan is not a bounded regular file".into(),
        ));
    }
    let bytes = std::fs::read(path)?;
    serde_json::from_slice(&bytes).map_err(UpdateError::from)
}

fn validate_plan(plan: &UpdatePlan, plan_path: &Path) -> Result<(), UpdateError> {
    let executable = std::env::current_exe()?;
    let expected_helper = plan.staged_bundle.join("Contents/MacOS/viewr-bin");
    if executable != expected_helper
        || plan_path != plan.staging_root.join("update-plan.json")
        || plan.lock_path != plan.staging_root.join("parent.lock")
        || plan.staged_bundle != plan.staging_root.join(APP_NAME)
        || plan.staging_root.parent() != plan.target_bundle.parent()
        || plan.backup_bundle.parent() != plan.target_bundle.parent()
        || plan.target_bundle.file_name() != Some(APP_NAME.as_ref())
        || !plan
            .backup_bundle
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with(".Viewr-backup-"))
    {
        return Err(UpdateError::InvalidRelease(
            "the prepared application update paths are inconsistent".into(),
        ));
    }
    ensure_real_directory(&plan.staging_root)?;
    ensure_real_directory(
        plan.target_bundle
            .parent()
            .expect("validated target has a parent"),
    )
}

fn update_target(previous_bundle: &Path) -> Result<PathBuf, UpdateError> {
    let parent = previous_bundle.parent().ok_or_else(|| {
        UpdateError::InvalidRelease("the current application bundle has no parent".into())
    })?;
    if directory_is_writable(parent) {
        return Ok(previous_bundle.to_owned());
    }
    let applications = dirs::home_dir()
        .ok_or_else(|| io::Error::other("the home directory is unavailable"))?
        .join("Applications");
    ensure_user_applications_directory(&applications)?;
    Ok(applications.join(APP_NAME))
}

fn directory_is_writable(path: &Path) -> bool {
    tempfile::Builder::new()
        .prefix(".viewr-write-test-")
        .tempdir_in(path)
        .is_ok()
}

fn ensure_user_applications_directory(path: &Path) -> Result<(), UpdateError> {
    match path.symlink_metadata() {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => {
            return Err(UpdateError::InvalidRelease(
                "the user Applications path is not a regular directory".into(),
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            std::fs::create_dir(path)?;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
        }
        Err(error) => return Err(error.into()),
    }
    if !directory_is_writable(path) {
        return Err(UpdateError::InvalidRelease(
            "the user Applications directory is not writable".into(),
        ));
    }
    Ok(())
}

fn app_bundle_for_executable(executable: &Path) -> Option<PathBuf> {
    let macos = executable.parent()?;
    let contents = macos.parent()?;
    let bundle = contents.parent()?;
    (executable.file_name()? == "viewr-bin"
        && macos.file_name()? == "MacOS"
        && contents.file_name()? == "Contents"
        && bundle.file_name()? == APP_NAME)
        .then(|| bundle.to_owned())
}

fn extract_app_archive(archive_path: &Path, destination: &Path) -> Result<(), UpdateError> {
    let metadata = archive_path.symlink_metadata()?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(UpdateError::InvalidRelease(
            "the application update archive is not a regular file".into(),
        ));
    }
    let decoder = GzDecoder::new(File::open(archive_path)?);
    let mut archive = tar::Archive::new(decoder);
    let mut seen = HashSet::new();
    let mut expanded = 0u64;
    let mut count = 0usize;
    for entry in archive.entries()? {
        let mut entry = entry?;
        count = count.saturating_add(1);
        if count > MAX_ARCHIVE_ENTRIES {
            return Err(UpdateError::InvalidRelease(
                "the application update archive contains too many entries".into(),
            ));
        }
        let path = entry.path()?.into_owned();
        if !safe_archive_path(&path) || !seen.insert(path.clone()) {
            return Err(UpdateError::InvalidRelease(format!(
                "the application update archive contains an unsafe or duplicate path: {}",
                path.display()
            )));
        }
        let kind = entry.header().entry_type();
        if !kind.is_file() && !kind.is_dir() {
            return Err(UpdateError::InvalidRelease(format!(
                "the application update archive contains a link or special file: {}",
                path.display()
            )));
        }
        let mode = entry.header().mode()?;
        if mode & 0o7000 != 0 {
            return Err(UpdateError::InvalidRelease(format!(
                "the application update archive contains privileged mode bits: {}",
                path.display()
            )));
        }
        expanded = expanded
            .checked_add(entry.header().size()?)
            .ok_or_else(|| UpdateError::InvalidRelease("expanded update size overflow".into()))?;
        if expanded > MAX_EXPANDED_BYTES {
            return Err(UpdateError::InvalidRelease(
                "the expanded application update exceeds its size limit".into(),
            ));
        }
        if !entry.unpack_in(destination)? {
            return Err(UpdateError::InvalidRelease(format!(
                "the application update entry escaped its destination: {}",
                path.display()
            )));
        }
    }
    if count == 0 || !destination.join(APP_NAME).is_dir() {
        return Err(UpdateError::InvalidRelease(
            "the application update archive does not contain Viewr.app".into(),
        ));
    }
    verify_no_links(&destination.join(APP_NAME), 0)?;
    Ok(())
}

fn safe_archive_path(path: &Path) -> bool {
    let mut components = path.components();
    matches!(components.next(), Some(Component::Normal(name)) if name == APP_NAME)
        && components.all(|component| matches!(component, Component::Normal(_)))
}

fn verify_no_links(path: &Path, visited: usize) -> Result<usize, UpdateError> {
    if visited > MAX_ARCHIVE_ENTRIES {
        return Err(UpdateError::InvalidRelease(
            "the extracted application contains too many files".into(),
        ));
    }
    let metadata = path.symlink_metadata()?;
    if metadata.file_type().is_symlink() {
        return Err(UpdateError::InvalidRelease(format!(
            "the extracted application contains a symbolic link: {}",
            path.display()
        )));
    }
    if !metadata.is_dir() {
        return Ok(visited.saturating_add(1));
    }
    let mut total = visited.saturating_add(1);
    for entry in std::fs::read_dir(path)? {
        total = verify_no_links(&entry?.path(), total)?;
    }
    Ok(total)
}

fn validate_app_bundle(bundle: &Path, version: &Version) -> Result<(), UpdateError> {
    ensure_real_directory(bundle)?;
    let info = bundle.join("Contents/Info.plist");
    let launcher = bundle.join("Contents/MacOS/ViewrLauncher");
    let viewer = bundle.join("Contents/MacOS/viewr-bin");
    ensure_regular_file(&info, false)?;
    ensure_regular_file(&launcher, true)?;
    ensure_regular_file(&viewer, true)?;
    verify_no_links(bundle, 0)?;

    let identifier = plist_value(&info, "CFBundleIdentifier")?;
    let short_version = plist_value(&info, "CFBundleShortVersionString")?;
    let bundle_version = plist_value(&info, "CFBundleVersion")?;
    if identifier != BUNDLE_IDENTIFIER
        || short_version != version.to_string()
        || bundle_version != version.to_string()
    {
        return Err(UpdateError::InvalidRelease(
            "the application update bundle identity or version is incorrect".into(),
        ));
    }

    let output = Command::new(&viewer).arg("--version").output()?;
    if !output.status.success()
        || String::from_utf8_lossy(&output.stdout).trim() != format!("viewr {version}")
    {
        return Err(UpdateError::InvalidRelease(
            "the application update binary reports the wrong version".into(),
        ));
    }
    let architecture = Command::new("/usr/bin/lipo")
        .arg("-archs")
        .arg(&viewer)
        .output()?;
    if !architecture.status.success()
        || String::from_utf8_lossy(&architecture.stdout).trim() != "arm64"
    {
        return Err(UpdateError::InvalidRelease(
            "the application update binary is not arm64".into(),
        ));
    }
    run_checked(
        Command::new("/usr/bin/codesign")
            .arg("--verify")
            .arg("--deep")
            .arg("--strict")
            .arg(bundle),
    )
}

fn plist_value(path: &Path, key: &str) -> Result<String, UpdateError> {
    let output = Command::new("/usr/bin/plutil")
        .arg("-extract")
        .arg(key)
        .arg("raw")
        .arg("-o")
        .arg("-")
        .arg(path)
        .output()?;
    if !output.status.success() {
        return Err(UpdateError::InvalidRelease(format!(
            "the application update Info.plist is missing {key}"
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn ensure_real_directory(path: &Path) -> Result<(), UpdateError> {
    let metadata = path.symlink_metadata()?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(UpdateError::InvalidRelease(format!(
            "expected a regular directory: {}",
            path.display()
        )));
    }
    Ok(())
}

fn ensure_regular_file(path: &Path, executable: bool) -> Result<(), UpdateError> {
    let metadata = path.symlink_metadata()?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || (executable && metadata.permissions().mode() & 0o111 == 0)
    {
        return Err(UpdateError::InvalidRelease(format!(
            "expected a regular{} file: {}",
            if executable { " executable" } else { "" },
            path.display()
        )));
    }
    Ok(())
}

fn create_private_file(path: &Path, bytes: &[u8]) -> Result<(), UpdateError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn remove_verified_backup(plan: &UpdatePlan) -> Result<(), UpdateError> {
    if plan.backup_bundle.parent() != plan.target_bundle.parent()
        || !plan
            .backup_bundle
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with(".Viewr-backup-"))
    {
        return Err(UpdateError::InvalidRelease(
            "refusing to remove an invalid update backup path".into(),
        ));
    }
    std::fs::remove_dir_all(&plan.backup_bundle)?;
    Ok(())
}

fn remove_staging_root(plan: &UpdatePlan) -> Result<(), UpdateError> {
    if plan.staging_root.parent() != plan.target_bundle.parent()
        || !plan
            .staging_root
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with(".Viewr-update-"))
    {
        return Err(UpdateError::InvalidRelease(
            "refusing to remove an invalid update staging path".into(),
        ));
    }
    std::fs::remove_dir_all(&plan.staging_root)?;
    sync_directory(
        plan.target_bundle
            .parent()
            .expect("validated target has a parent"),
    )
    .map_err(UpdateError::from)
}

fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn archive_tree(source_app: &Path, archive_path: &Path) {
        let output = File::create(archive_path).unwrap();
        let encoder = flate2::write::GzEncoder::new(output, flate2::Compression::best());
        let mut archive = tar::Builder::new(encoder);
        archive.follow_symlinks(false);
        archive.append_dir_all(APP_NAME, source_app).unwrap();
        archive.into_inner().unwrap().finish().unwrap();
    }

    #[test]
    fn bundle_detection_requires_the_exact_layout() {
        assert_eq!(
            app_bundle_for_executable(Path::new(
                "/Users/me/Applications/Viewr.app/Contents/MacOS/viewr-bin"
            )),
            Some(PathBuf::from("/Users/me/Applications/Viewr.app"))
        );
        for path in [
            "/Applications/Other.app/Contents/MacOS/viewr-bin",
            "/Applications/Viewr.app/Contents/MacOS/other",
            "/Applications/Viewr.app/MacOS/viewr-bin",
        ] {
            assert_eq!(app_bundle_for_executable(Path::new(path)), None);
        }
    }

    #[test]
    fn archive_paths_are_confined_to_the_expected_bundle() {
        for path in [
            "Viewr.app",
            "Viewr.app/Contents",
            "Viewr.app/Contents/MacOS/viewr-bin",
        ] {
            assert!(safe_archive_path(Path::new(path)), "rejected {path}");
        }
        for path in [
            "other",
            "Other.app/Contents/file",
            "../Viewr.app/Contents/file",
            "Viewr.app/../outside",
            "/Viewr.app/Contents/file",
        ] {
            assert!(!safe_archive_path(Path::new(path)), "accepted {path}");
        }
    }

    #[test]
    fn extracted_tree_rejects_symbolic_links() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let app = root.path().join(APP_NAME);
        std::fs::create_dir(&app).unwrap();
        std::fs::write(app.join("real"), b"ok").unwrap();
        symlink("real", app.join("link")).unwrap();

        assert!(verify_no_links(&app, 0).is_err());
    }

    #[test]
    fn app_archives_extract_with_paths_and_bytes_intact() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source").join(APP_NAME);
        std::fs::create_dir_all(source.join("Contents/MacOS")).unwrap();
        std::fs::write(source.join("Contents/MacOS/viewr-bin"), b"viewer").unwrap();
        let archive = root.path().join("viewr-macos-arm64.tar.gz");
        archive_tree(&source, &archive);

        let destination = root.path().join("destination");
        std::fs::create_dir(&destination).unwrap();
        extract_app_archive(&archive, &destination).unwrap();

        assert_eq!(
            std::fs::read(destination.join("Viewr.app/Contents/MacOS/viewr-bin")).unwrap(),
            b"viewer"
        );
    }

    #[test]
    fn app_archives_reject_links_before_they_are_unpacked() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source").join(APP_NAME);
        std::fs::create_dir_all(source.join("Contents")).unwrap();
        std::fs::write(source.join("Contents/real"), b"viewer").unwrap();
        symlink("real", source.join("Contents/link")).unwrap();
        let archive = root.path().join("viewr-macos-arm64.tar.gz");
        archive_tree(&source, &archive);

        let destination = root.path().join("destination");
        std::fs::create_dir(&destination).unwrap();
        assert!(extract_app_archive(&archive, &destination).is_err());
        assert!(!destination.join("Viewr.app/Contents/link").exists());
    }

    #[test]
    fn update_plan_round_trips_paths_without_shell_interpretation() {
        let root = PathBuf::from("/Users/me/Applications/.Viewr-update-quote'$;[]");
        let plan = UpdatePlan {
            expected_version: "0.5.0".into(),
            previous_bundle: PathBuf::from("/Applications/Viewr.app"),
            target_bundle: PathBuf::from("/Users/me/Applications/Viewr.app"),
            staged_bundle: root.join(APP_NAME),
            backup_bundle: PathBuf::from("/Users/me/Applications/.Viewr-backup-quote'$;[].app"),
            staging_root: root.clone(),
            lock_path: root.join("parent.lock"),
            relaunch_arguments: vec!["/Users/me/photos quote'$;[]".into()],
        };
        let bytes = serde_json::to_vec(&plan).unwrap();
        let decoded: UpdatePlan = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(decoded.target_bundle, plan.target_bundle);
        assert_eq!(decoded.relaunch_arguments, plan.relaunch_arguments);
    }
}
