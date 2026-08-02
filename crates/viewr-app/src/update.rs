//! Stable-release discovery, verified downloads, and native installer handoff.
//!
//! Update work is deliberately separate from the image engine. Network and
//! hashing operations run on background threads; this module's UI controller
//! consumes their bounded completion messages on the eframe thread.

use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use eframe::egui;
use semver::Version;
use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use thiserror::Error;
use url::Url;

const RELEASE_API_URL: &str = "https://api.github.com/repos/hunterchen7/viewr/releases/latest";
const GITHUB_API_VERSION: &str = "2026-03-10";
const USER_AGENT: &str = concat!("viewr/", env!("CARGO_PKG_VERSION"), " updater");
const MAX_RELEASE_JSON_BYTES: usize = 1024 * 1024;
const MAX_RELEASE_NOTES_BYTES: usize = 64 * 1024;
const MAX_RELEASE_NOTE_BLOCKS: usize = 128;
const MAX_RELEASE_NOTE_SPANS: usize = 16;
const MAX_RELEASE_ASSETS: usize = 128;
const MAX_PACKAGE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_REDIRECTS: usize = 5;
const MAX_DOWNLOAD_DURATION: Duration = Duration::from_secs(20 * 60);
const AUTO_CHECK_INTERVAL_SECS: u64 = 24 * 60 * 60;
const MANUAL_CHECK_INTERVAL_SECS: u64 = 5;
const MAX_UPDATE_STATE_BYTES: u64 = 4096;
const UPDATE_CACHE_RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);
#[cfg(target_os = "macos")]
static QUARANTINE_EVENT_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Error)]
pub(crate) enum UpdateError {
    #[error("{0}")]
    InvalidRelease(String),
    #[error("GitHub returned HTTP {0}")]
    HttpStatus(u16),
    #[error("update request failed: {0}")]
    Network(String),
    #[error("update data exceeded its {0} byte limit")]
    BodyTooLarge(usize),
    #[error("update data was not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("update storage failed: {0}")]
    Io(#[from] io::Error),
    #[error("another Viewr window is already {0}")]
    Busy(&'static str),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CheckSource {
    Automatic,
    Manual,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PackageKind {
    Application,
    Installer,
    Portable,
}

impl PackageKind {
    pub(crate) fn noun(self) -> &'static str {
        match self {
            Self::Application => "application update",
            Self::Installer => "installer",
            Self::Portable => "portable update",
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ReleaseAsset {
    pub(crate) name: &'static str,
    pub(crate) url: String,
    pub(crate) size: u64,
    pub(crate) sha256: [u8; 32],
    pub(crate) kind: PackageKind,
}

#[derive(Clone, Debug)]
pub(crate) enum Delivery {
    Download(ReleaseAsset),
    ReleasePageOnly { reason: String },
}

#[derive(Clone, Debug)]
pub(crate) struct Release {
    pub(crate) version: Version,
    notes: Arc<[ReleaseNoteBlock]>,
    pub(crate) delivery: Delivery,
}

impl Release {
    pub(crate) fn page_url(&self) -> String {
        format!(
            "https://github.com/hunterchen7/viewr/releases/tag/v{}",
            self.version
        )
    }
}

#[derive(Debug)]
pub(crate) enum CheckOutcome {
    Current(Version),
    Available(Release),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PlatformAssets {
    application: Option<&'static str>,
    installer: &'static str,
    portable: &'static str,
}

fn platform_assets(target_os: &str, target_arch: &str) -> Option<PlatformAssets> {
    match (target_os, target_arch) {
        ("macos", "aarch64") => Some(PlatformAssets {
            application: Some("viewr-macos-arm64.tar.gz"),
            installer: "viewr-macos-arm64.pkg",
            portable: "viewr-macos-arm64.tar.gz",
        }),
        ("windows", "x86_64") => Some(PlatformAssets {
            application: None,
            installer: "viewr-windows-x64.msi",
            portable: "viewr-windows-x64.zip",
        }),
        ("linux", "x86_64") => Some(PlatformAssets {
            application: None,
            installer: "viewr-linux-x64.deb",
            portable: "viewr-linux-x64.tar.gz",
        }),
        _ => None,
    }
}

fn current_delivery_spec() -> Option<(&'static str, PackageKind)> {
    delivery_spec_for(
        std::env::consts::OS,
        std::env::consts::ARCH,
        native_install_detected(),
    )
}

fn delivery_spec_for(
    target_os: &str,
    target_arch: &str,
    native_install: bool,
) -> Option<(&'static str, PackageKind)> {
    let assets = platform_assets(target_os, target_arch)?;
    if let Some(application) = assets.application {
        return Some((application, PackageKind::Application));
    }
    let kind = if native_install {
        PackageKind::Installer
    } else {
        PackageKind::Portable
    };
    Some((
        match kind {
            PackageKind::Application => unreachable!("handled above"),
            PackageKind::Installer => assets.installer,
            PackageKind::Portable => assets.portable,
        },
        kind,
    ))
}

fn native_install_detected() -> bool {
    std::env::current_exe()
        .ok()
        .is_some_and(|path| native_install_path(std::env::consts::OS, &path))
}

fn native_install_path(target_os: &str, executable: &Path) -> bool {
    match target_os {
        "macos" => executable == Path::new("/Applications/Viewr.app/Contents/MacOS/viewr-bin"),
        "linux" => executable == Path::new("/usr/bin/viewr"),
        "windows" => {
            let normalized = executable.to_string_lossy().replace('/', "\\");
            let normalized = normalized.to_ascii_lowercase();
            let normalized = normalized.strip_prefix(r"\\?\").unwrap_or(&normalized);
            let Some(relative) = normalized.get(2..) else {
                return false;
            };
            normalized.as_bytes()[0].is_ascii_alphabetic()
                && normalized.as_bytes()[1] == b':'
                && matches!(
                    relative,
                    r"\program files\viewr\viewr.exe" | r"\program files (x86)\viewr\viewr.exe"
                )
        }
        _ => false,
    }
}

#[derive(Deserialize)]
struct GithubRelease {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    body: Option<String>,
    assets: Vec<GithubAsset>,
}

#[derive(Deserialize)]
struct GithubAsset {
    name: String,
    state: String,
    size: u64,
    digest: Option<String>,
}

pub(crate) fn current_version() -> Version {
    let raw = current_version_text();
    let version = Version::parse(raw).expect("the workspace package version must be valid SemVer");
    assert!(
        version.pre.is_empty() && version.build.is_empty() && version.to_string() == raw,
        "the workspace package version must be canonical stable SemVer"
    );
    version
}

fn current_version_text() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

fn parse_stable_tag(tag: &str) -> Result<Version, UpdateError> {
    let Some(raw) = tag.strip_prefix('v') else {
        return Err(UpdateError::InvalidRelease(
            "the latest release tag is missing its v prefix".into(),
        ));
    };
    if raw.len() > 64 {
        return Err(UpdateError::InvalidRelease(
            "the latest release version is too long".into(),
        ));
    }
    let version = Version::parse(raw).map_err(|_| {
        UpdateError::InvalidRelease("the latest release tag is not stable SemVer".into())
    })?;
    if !version.pre.is_empty() || !version.build.is_empty() || version.to_string() != raw {
        return Err(UpdateError::InvalidRelease(
            "the latest release tag is not canonical stable SemVer".into(),
        ));
    }
    Ok(version)
}

fn parse_digest(value: &str) -> Result<[u8; 32], UpdateError> {
    let Some(hex) = value.strip_prefix("sha256:") else {
        return Err(UpdateError::InvalidRelease(
            "the update asset has no SHA-256 digest".into(),
        ));
    };
    if hex.len() != 64 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(UpdateError::InvalidRelease(
            "the update asset has an invalid SHA-256 digest".into(),
        ));
    }
    let mut digest = [0u8; 32];
    for (index, pair) in hex.as_bytes().chunks_exact(2).enumerate() {
        let pair = std::str::from_utf8(pair).expect("ASCII hex is valid UTF-8");
        digest[index] = u8::from_str_radix(pair, 16).expect("validated hex pair");
    }
    Ok(digest)
}

fn sanitized_notes(body: Option<&str>) -> String {
    let mut output = String::new();
    let mut last_was_cr = false;
    for character in body.unwrap_or_default().chars() {
        if output.len() >= MAX_RELEASE_NOTES_BYTES {
            break;
        }
        let normalized = match character {
            '\r' => {
                last_was_cr = true;
                '\n'
            }
            '\n' if last_was_cr => {
                last_was_cr = false;
                continue;
            }
            '\n' | '\t' => {
                last_was_cr = false;
                character
            }
            control if control.is_control() => {
                last_was_cr = false;
                '�'
            }
            _ => {
                last_was_cr = false;
                character
            }
        };
        if output.len() + normalized.len_utf8() > MAX_RELEASE_NOTES_BYTES {
            break;
        }
        output.push(normalized);
    }
    let trimmed = output.trim();
    if trimmed.is_empty() {
        "See the release page for details.".to_owned()
    } else {
        trimmed.to_owned()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ReleaseNoteSpan {
    Text(String),
    Strong(String),
    Link { label: String, url: String },
}

impl ReleaseNoteSpan {
    #[cfg(test)]
    fn label(&self) -> &str {
        match self {
            Self::Text(text) | Self::Strong(text) => text,
            Self::Link { label, .. } => label,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ReleaseNoteBlock {
    Heading {
        level: u8,
        spans: Vec<ReleaseNoteSpan>,
    },
    Bullet(Vec<ReleaseNoteSpan>),
    Paragraph(Vec<ReleaseNoteSpan>),
}

impl ReleaseNoteBlock {
    #[cfg(test)]
    fn spans(&self) -> std::slice::Iter<'_, ReleaseNoteSpan> {
        match self {
            Self::Heading { spans, .. } | Self::Bullet(spans) | Self::Paragraph(spans) => {
                spans.iter()
            }
        }
    }
}

fn parse_release_notes(notes: &str) -> Vec<ReleaseNoteBlock> {
    let mut blocks = Vec::new();
    for line in notes.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if blocks.len() == MAX_RELEASE_NOTE_BLOCKS - 1 {
            blocks.push(ReleaseNoteBlock::Paragraph(vec![ReleaseNoteSpan::Text(
                "More details are available on the release page.".into(),
            )]));
            break;
        }
        let heading_marks = line.bytes().take_while(|byte| *byte == b'#').count();
        if (1..=6).contains(&heading_marks) && line.as_bytes().get(heading_marks) == Some(&b' ') {
            blocks.push(ReleaseNoteBlock::Heading {
                level: heading_marks as u8,
                spans: parse_release_note_spans(line[heading_marks + 1..].trim()),
            });
            continue;
        }
        if let Some(bullet) = line.strip_prefix("* ").or_else(|| line.strip_prefix("- ")) {
            blocks.push(ReleaseNoteBlock::Bullet(parse_release_note_spans(
                bullet.trim(),
            )));
            continue;
        }
        blocks.push(ReleaseNoteBlock::Paragraph(parse_release_note_spans(line)));
    }
    blocks
}

fn parse_release_note_spans(line: &str) -> Vec<ReleaseNoteSpan> {
    let mut spans = Vec::new();
    let mut emitted = 0;
    let mut scan = 0;
    while scan < line.len() {
        let Some(open_offset) = line[scan..].find('[') else {
            break;
        };
        let open = scan + open_offset;
        let Some(close_offset) = line[open + 1..].find(']') else {
            break;
        };
        let close = open + 1 + close_offset;
        let url_start = close + 1;
        if line.as_bytes().get(url_start) != Some(&b'(') {
            scan = close + 1;
            continue;
        }
        let url_start = url_start + 1;
        let Some(url_end_offset) = line[url_start..].find(')') else {
            break;
        };
        let url_end = url_start + url_end_offset;
        let label = &line[open + 1..close];
        let url = &line[url_start..url_end];
        let trusted = !label.is_empty()
            && Url::parse(url).is_ok_and(|url| {
                url.scheme() == "https"
                    && url.port_or_known_default() == Some(443)
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.host_str() == Some("github.com")
                    && url.path().starts_with("/hunterchen7/viewr/")
            });
        if !trusted {
            scan = url_end + 1;
            continue;
        }
        if spans.len() >= MAX_RELEASE_NOTE_SPANS - 2 {
            break;
        }
        if emitted < open {
            spans.push(ReleaseNoteSpan::Text(line[emitted..open].to_owned()));
        }
        spans.push(ReleaseNoteSpan::Link {
            label: label.to_owned(),
            url: url.to_owned(),
        });
        emitted = url_end + 1;
        scan = emitted;
    }
    if emitted < line.len() {
        spans.push(ReleaseNoteSpan::Text(line[emitted..].to_owned()));
    }
    parse_release_note_emphasis(spans)
}

fn parse_release_note_emphasis(spans: Vec<ReleaseNoteSpan>) -> Vec<ReleaseNoteSpan> {
    let mut output = Vec::with_capacity(spans.len());
    for span in spans {
        let text = match span {
            ReleaseNoteSpan::Text(text) => text,
            other => {
                if !push_release_note_span(&mut output, other) {
                    return output;
                }
                continue;
            }
        };
        let mut emitted = 0;
        let mut scan = 0;
        while scan < text.len() {
            let Some(open_offset) = text[scan..].find("**") else {
                break;
            };
            let open = scan + open_offset;
            let content_start = open + 2;
            let Some(close_offset) = text[content_start..].find("**") else {
                break;
            };
            let close = content_start + close_offset;
            if close == content_start {
                scan = content_start;
                continue;
            }
            if emitted < open
                && !push_release_note_span(
                    &mut output,
                    ReleaseNoteSpan::Text(text[emitted..open].to_owned()),
                )
            {
                return output;
            }
            if !push_release_note_span(
                &mut output,
                ReleaseNoteSpan::Strong(text[content_start..close].to_owned()),
            ) {
                return output;
            }
            emitted = close + 2;
            scan = emitted;
        }
        if emitted < text.len()
            && !push_release_note_span(
                &mut output,
                ReleaseNoteSpan::Text(text[emitted..].to_owned()),
            )
        {
            return output;
        }
    }
    if output.is_empty() {
        output.push(ReleaseNoteSpan::Text(String::new()));
    }
    output
}

fn push_release_note_span(output: &mut Vec<ReleaseNoteSpan>, span: ReleaseNoteSpan) -> bool {
    if output.len() < MAX_RELEASE_NOTE_SPANS - 1 {
        output.push(span);
        true
    } else {
        output.push(ReleaseNoteSpan::Text("…".into()));
        false
    }
}

#[derive(Clone, Copy)]
enum ReleaseNoteStyle {
    Heading(u8),
    Body,
}

fn release_note_text(text: &str, style: ReleaseNoteStyle, strong: bool) -> egui::RichText {
    let text = egui::RichText::new(text);
    let text = match style {
        ReleaseNoteStyle::Heading(1 | 2) => text.strong().size(15.0),
        ReleaseNoteStyle::Heading(_) => text.strong().size(13.0),
        ReleaseNoteStyle::Body => text,
    };
    if strong { text.strong() } else { text }
}

fn release_note_job(
    ui: &egui::Ui,
    text: &str,
    style: ReleaseNoteStyle,
    strong: bool,
) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::default();
    release_note_text(text, style, strong).append_to(
        &mut job,
        ui.style(),
        egui::FontSelection::Default,
        egui::Align::Center,
    );
    job
}

fn show_release_note_inline(ui: &mut egui::Ui, spans: &[ReleaseNoteSpan], style: ReleaseNoteStyle) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        for span in spans {
            match span {
                ReleaseNoteSpan::Text(text) | ReleaseNoteSpan::Strong(text) => {
                    let strong = matches!(span, ReleaseNoteSpan::Strong(_));
                    ui.add(egui::Label::new(release_note_job(ui, text, style, strong)).wrap());
                }
                ReleaseNoteSpan::Link { label, url } => {
                    ui.hyperlink_to(release_note_job(ui, label, style, false), url);
                }
            }
        }
    });
}

fn show_release_note_spans(
    ui: &mut egui::Ui,
    spans: &[ReleaseNoteSpan],
    style: ReleaseNoteStyle,
    bullet: bool,
) {
    if bullet {
        ui.horizontal_top(|ui| {
            ui.label(release_note_text("•", style, false));
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                show_release_note_inline(ui, spans, style);
            });
        });
    } else {
        show_release_note_inline(ui, spans, style);
    }
}

fn show_release_notes(ui: &mut egui::Ui, notes: &[ReleaseNoteBlock]) {
    for (index, block) in notes.iter().enumerate() {
        if index > 0 {
            ui.add_space(match block {
                ReleaseNoteBlock::Heading { .. } => 6.0,
                ReleaseNoteBlock::Bullet(_) | ReleaseNoteBlock::Paragraph(_) => 2.0,
            });
        }
        match block {
            ReleaseNoteBlock::Heading { level, spans } => {
                show_release_note_spans(ui, spans, ReleaseNoteStyle::Heading(*level), false);
            }
            ReleaseNoteBlock::Bullet(spans) => {
                show_release_note_spans(ui, spans, ReleaseNoteStyle::Body, true);
            }
            ReleaseNoteBlock::Paragraph(spans) => {
                show_release_note_spans(ui, spans, ReleaseNoteStyle::Body, false);
            }
        }
    }
}

fn parse_release(
    bytes: &[u8],
    running: &Version,
    delivery_spec: Option<(&'static str, PackageKind)>,
) -> Result<CheckOutcome, UpdateError> {
    if bytes.len() > MAX_RELEASE_JSON_BYTES {
        return Err(UpdateError::BodyTooLarge(MAX_RELEASE_JSON_BYTES));
    }
    let raw: GithubRelease = serde_json::from_slice(bytes)?;
    if raw.draft || raw.prerelease {
        return Err(UpdateError::InvalidRelease(
            "GitHub returned a draft or prerelease as the latest stable release".into(),
        ));
    }
    if raw.assets.len() > MAX_RELEASE_ASSETS {
        return Err(UpdateError::InvalidRelease(
            "the latest release contains too many assets".into(),
        ));
    }
    let version = parse_stable_tag(&raw.tag_name)?;
    if version <= *running {
        return Ok(CheckOutcome::Current(version));
    }

    let notes: Arc<[ReleaseNoteBlock]> =
        Arc::from(parse_release_notes(&sanitized_notes(raw.body.as_deref())));
    let delivery = match delivery_spec {
        None => Delivery::ReleasePageOnly {
            reason: format!(
                "Downloads are not packaged for {} {}.",
                std::env::consts::OS,
                std::env::consts::ARCH
            ),
        },
        Some((expected_name, kind)) => {
            let matches: Vec<&GithubAsset> = raw
                .assets
                .iter()
                .filter(|asset| asset.name == expected_name)
                .collect();
            match matches.as_slice() {
                [] => Delivery::ReleasePageOnly {
                    reason: format!("This release does not include {expected_name}."),
                },
                [asset] => {
                    if asset.state != "uploaded" {
                        return Err(UpdateError::InvalidRelease(format!(
                            "{expected_name} is not fully uploaded"
                        )));
                    }
                    if asset.size == 0 || asset.size > MAX_PACKAGE_BYTES {
                        return Err(UpdateError::InvalidRelease(format!(
                            "{expected_name} has an invalid size"
                        )));
                    }
                    let Some(digest) = asset.digest.as_deref() else {
                        return Ok(CheckOutcome::Available(Release {
                            version,
                            notes,
                            delivery: Delivery::ReleasePageOnly {
                                reason: format!(
                                    "GitHub did not publish a digest for {expected_name}."
                                ),
                            },
                        }));
                    };
                    let sha256 = parse_digest(digest)?;
                    let url = format!(
                        "https://github.com/hunterchen7/viewr/releases/download/v{version}/{expected_name}"
                    );
                    Delivery::Download(ReleaseAsset {
                        name: expected_name,
                        url,
                        size: asset.size,
                        sha256,
                        kind,
                    })
                }
                _ => {
                    return Err(UpdateError::InvalidRelease(format!(
                        "the release contains duplicate {expected_name} assets"
                    )));
                }
            }
        }
    };
    Ok(CheckOutcome::Available(Release {
        version,
        notes,
        delivery,
    }))
}

#[derive(Clone, Copy)]
enum RequestPurpose {
    ReleaseApi,
    ReleaseAsset,
}

struct HttpClient {
    agent: ureq::Agent,
}

impl HttpClient {
    fn new() -> Self {
        Self {
            agent: ureq::AgentBuilder::new()
                .redirects(0)
                .timeout_connect(Duration::from_secs(5))
                .timeout_read(Duration::from_secs(30))
                .timeout_write(Duration::from_secs(30))
                .user_agent(USER_AGENT)
                .build(),
        }
    }

    fn get(
        &self,
        raw_url: &str,
        purpose: RequestPurpose,
        timeout: Duration,
    ) -> Result<ureq::Response, UpdateError> {
        let mut url = Url::parse(raw_url)
            .map_err(|_| UpdateError::InvalidRelease("the update URL is not valid".into()))?;
        validate_request_url(&url, purpose)?;
        let started = Instant::now();
        for redirect_count in 0..=MAX_REDIRECTS {
            if started.elapsed() > timeout {
                return Err(UpdateError::Network("the update request timed out".into()));
            }
            let response = self
                .agent
                .get(url.as_str())
                .set("Accept-Encoding", "identity")
                .set(
                    "Accept",
                    match purpose {
                        RequestPurpose::ReleaseApi => "application/vnd.github+json",
                        RequestPurpose::ReleaseAsset => "application/octet-stream",
                    },
                )
                .set("X-GitHub-Api-Version", GITHUB_API_VERSION)
                .call()
                .map_err(map_ureq_error)?;
            match response.status() {
                200 => return Ok(response),
                301 | 302 | 303 | 307 | 308 => {
                    url = redirect_url(&url, response.header("Location"), redirect_count, purpose)?;
                }
                status => return Err(UpdateError::HttpStatus(status)),
            }
        }
        unreachable!("the redirect loop returns at its configured bound")
    }
}

fn redirect_url(
    current: &Url,
    location: Option<&str>,
    redirect_count: usize,
    purpose: RequestPurpose,
) -> Result<Url, UpdateError> {
    if redirect_count == MAX_REDIRECTS {
        return Err(UpdateError::Network(
            "the update download redirected too many times".into(),
        ));
    }
    let location = location.ok_or_else(|| {
        UpdateError::Network("the update download redirected without a location".into())
    })?;
    let next = current.join(location).map_err(|_| {
        UpdateError::Network("the update download returned an invalid redirect".into())
    })?;
    validate_request_url(&next, purpose)?;
    Ok(next)
}

fn map_ureq_error(error: ureq::Error) -> UpdateError {
    match error {
        ureq::Error::Status(status, _) => UpdateError::HttpStatus(status),
        ureq::Error::Transport(error) => UpdateError::Network(error.to_string()),
    }
}

fn validate_request_url(url: &Url, purpose: RequestPurpose) -> Result<(), UpdateError> {
    if url.scheme() != "https"
        || url.port_or_known_default() != Some(443)
        || url.username() != ""
        || url.password().is_some()
    {
        return Err(UpdateError::InvalidRelease(
            "update requests require an HTTPS URL without credentials".into(),
        ));
    }
    let host = url.host_str().unwrap_or_default();
    let allowed = match purpose {
        RequestPurpose::ReleaseApi => host == "api.github.com",
        RequestPurpose::ReleaseAsset => matches!(
            host,
            "github.com"
                | "release-assets.githubusercontent.com"
                | "objects.githubusercontent.com"
                | "github-releases.githubusercontent.com"
        ),
    };
    if !allowed {
        return Err(UpdateError::InvalidRelease(format!(
            "the update request redirected to an untrusted host: {host}"
        )));
    }
    Ok(())
}

fn read_bounded(mut reader: impl io::Read, maximum: usize) -> Result<Vec<u8>, UpdateError> {
    let started = Instant::now();
    let mut bytes = Vec::with_capacity(maximum.min(64 * 1024));
    let mut buffer = [0u8; 16 * 1024];
    loop {
        if started.elapsed() > Duration::from_secs(20) {
            return Err(UpdateError::Network("the update response timed out".into()));
        }
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        if bytes.len().saturating_add(count) > maximum {
            return Err(UpdateError::BodyTooLarge(maximum));
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
    Ok(bytes)
}

pub(crate) fn check_latest_release() -> Result<CheckOutcome, UpdateError> {
    let client = HttpClient::new();
    let response = client.get(
        RELEASE_API_URL,
        RequestPurpose::ReleaseApi,
        Duration::from_secs(20),
    )?;
    if let Some(length) = response.header("Content-Length") {
        let length = length.parse::<usize>().map_err(|_| {
            UpdateError::Network("GitHub returned an invalid Content-Length".into())
        })?;
        if length > MAX_RELEASE_JSON_BYTES {
            return Err(UpdateError::BodyTooLarge(MAX_RELEASE_JSON_BYTES));
        }
    }
    let bytes = read_bounded(response.into_reader(), MAX_RELEASE_JSON_BYTES)?;
    parse_release(&bytes, &current_version(), current_delivery_spec())
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct PersistedUpdateState {
    automatic_checks_enabled: Option<bool>,
    skipped_version: Option<String>,
    last_automatic_attempt_unix: Option<u64>,
    last_manual_attempt_unix: Option<u64>,
}

#[derive(Clone, Debug)]
pub(crate) struct UpdateStore {
    state_path: PathBuf,
    state_lock_path: PathBuf,
    operation_directory: PathBuf,
    download_directory: PathBuf,
}

impl UpdateStore {
    pub(crate) fn from_system() -> Result<Self, UpdateError> {
        let config_directory = dirs::config_dir()
            .ok_or_else(|| io::Error::other("configuration directory is unavailable"))?
            .join("viewr");
        let cache_directory = dirs::cache_dir()
            .ok_or_else(|| io::Error::other("cache directory is unavailable"))?
            .join("viewr")
            .join("updates");
        Self::new(config_directory, cache_directory)
    }

    fn new(config_directory: PathBuf, cache_directory: PathBuf) -> Result<Self, UpdateError> {
        ensure_private_directory(&config_directory)?;
        ensure_private_directory(&cache_directory)?;
        Ok(Self {
            state_path: config_directory.join("update-state.toml"),
            state_lock_path: config_directory.join("update-state.lock"),
            operation_directory: cache_directory.join("locks"),
            download_directory: cache_directory.join("downloads"),
        })
    }

    fn with_state<T>(
        &self,
        mutate: impl FnOnce(&mut PersistedUpdateState) -> T,
    ) -> Result<T, UpdateError> {
        ensure_private_directory(
            self.state_lock_path
                .parent()
                .expect("state lock has a parent"),
        )?;
        let lock = open_lock_file(&self.state_lock_path)?;
        lock.lock()?;
        let mut state = self.read_state_unlocked()?;
        let result = mutate(&mut state);
        self.write_state_unlocked(&state)?;
        lock.unlock()?;
        Ok(result)
    }

    fn read_state_unlocked(&self) -> Result<PersistedUpdateState, UpdateError> {
        let metadata = match self.state_path.symlink_metadata() {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "update state cannot be a symbolic link",
                )
                .into());
            }
            Ok(metadata) if metadata.is_file() => metadata,
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "update state is not a regular file",
                )
                .into());
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(PersistedUpdateState::default());
            }
            Err(error) => return Err(error.into()),
        };
        if metadata.len() > MAX_UPDATE_STATE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "update state exceeds its size limit",
            )
            .into());
        }
        let text = std::fs::read_to_string(&self.state_path)?;
        toml::from_str(&text).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("update state is not valid TOML: {error}"),
            )
            .into()
        })
    }

    fn write_state_unlocked(&self, state: &PersistedUpdateState) -> Result<(), UpdateError> {
        let skipped = state
            .skipped_version
            .as_deref()
            .and_then(valid_stored_version);
        let mut output =
            String::from("# Viewr update preferences and cross-process coordination state.\n");
        if let Some(enabled) = state.automatic_checks_enabled {
            output.push_str(&format!("automatic_checks_enabled = {enabled}\n"));
        }
        if let Some(skipped) = skipped {
            output.push_str(&format!("skipped_version = \"{skipped}\"\n"));
        }
        if let Some(timestamp) = state.last_automatic_attempt_unix {
            output.push_str(&format!("last_automatic_attempt_unix = {timestamp}\n"));
        }
        if let Some(timestamp) = state.last_manual_attempt_unix {
            output.push_str(&format!("last_manual_attempt_unix = {timestamp}\n"));
        }
        replace_file_durable(&self.state_path, output.as_bytes())?;
        Ok(())
    }

    pub(crate) fn skipped_version(&self) -> Result<Option<Version>, UpdateError> {
        Ok(self
            .read_state_unlocked()?
            .skipped_version
            .as_deref()
            .and_then(valid_stored_version))
    }

    fn automatic_checks_enabled(&self) -> Result<bool, UpdateError> {
        // State publication is an atomic same-directory rename, so a reader
        // sees either the previous complete file or the next complete file.
        // Do not wait on the writer lock from the UI thread.
        Ok(self
            .read_state_unlocked()?
            .automatic_checks_enabled
            .unwrap_or(true))
    }

    fn set_automatic_checks_enabled(&self, enabled: bool) -> Result<(), UpdateError> {
        ensure_private_directory(
            self.state_lock_path
                .parent()
                .expect("state lock has a parent"),
        )?;
        let lock = open_lock_file(&self.state_lock_path)?;
        lock.lock()?;
        let mut state = match self.read_state_unlocked() {
            Ok(state) => state,
            Err(UpdateError::Io(error)) if error.kind() == io::ErrorKind::InvalidData => {
                PersistedUpdateState::default()
            }
            Err(error) => return Err(error),
        };
        state.automatic_checks_enabled = Some(enabled);
        self.write_state_unlocked(&state)?;
        lock.unlock()?;
        Ok(())
    }

    pub(crate) fn skip(&self, version: &Version) -> Result<(), UpdateError> {
        let version = version.clone();
        self.with_state(move |state| {
            state.skipped_version = Some(version.to_string());
        })
    }

    fn initialize(&self, running: &Version) -> Result<bool, UpdateError> {
        ensure_private_directory(
            self.state_lock_path
                .parent()
                .expect("state lock has a parent"),
        )?;
        let lock = open_lock_file(&self.state_lock_path)?;
        lock.lock()?;
        let mut state = self.read_state_unlocked()?;
        let enabled = state.automatic_checks_enabled.unwrap_or(true);
        let original_skip = state.skipped_version.clone();
        {
            let clear = state
                .skipped_version
                .as_deref()
                .and_then(valid_stored_version)
                .is_some_and(|skipped| skipped <= *running);
            if clear
                || state
                    .skipped_version
                    .as_deref()
                    .is_some_and(|value| valid_stored_version(value).is_none())
            {
                state.skipped_version = None;
            }
        }
        if state.skipped_version != original_skip {
            self.write_state_unlocked(&state)?;
        }
        lock.unlock()?;
        Ok(enabled)
    }

    pub(crate) fn claim_check(
        &self,
        source: CheckSource,
        now_unix: u64,
    ) -> Result<Option<File>, UpdateError> {
        ensure_private_directory(&self.operation_directory)?;
        let check_lock = open_lock_file(&self.operation_directory.join("check.lock"))?;
        match check_lock.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(UpdateError::Busy("checking for updates"));
            }
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(error.into());
            }
        }

        let claimed = self.with_state(|state| {
            if source == CheckSource::Automatic && state.automatic_checks_enabled == Some(false) {
                return false;
            }
            let previous = match source {
                CheckSource::Automatic => state.last_automatic_attempt_unix,
                CheckSource::Manual => match (
                    state.last_automatic_attempt_unix,
                    state.last_manual_attempt_unix,
                ) {
                    (Some(automatic), Some(manual)) => Some(automatic.max(manual)),
                    (automatic, manual) => automatic.or(manual),
                },
            };
            let minimum_age = match source {
                CheckSource::Automatic => AUTO_CHECK_INTERVAL_SECS,
                CheckSource::Manual => MANUAL_CHECK_INTERVAL_SECS,
            };
            if !check_due(previous, now_unix, minimum_age) {
                return false;
            }
            match source {
                CheckSource::Automatic => state.last_automatic_attempt_unix = Some(now_unix),
                CheckSource::Manual => state.last_manual_attempt_unix = Some(now_unix),
            }
            true
        })?;
        if claimed {
            Ok(Some(check_lock))
        } else {
            check_lock.unlock()?;
            Ok(None)
        }
    }

    fn claim_download(&self) -> Result<File, UpdateError> {
        ensure_private_directory(&self.operation_directory)?;
        let lock = open_lock_file(&self.operation_directory.join("download.lock"))?;
        match lock.try_lock() {
            Ok(()) => Ok(lock),
            Err(std::fs::TryLockError::WouldBlock) => {
                Err(UpdateError::Busy("downloading this update"))
            }
            Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
        }
    }

    fn destination(&self, release: &Release, asset: &ReleaseAsset) -> PathBuf {
        self.download_directory
            .join(release.version.to_string())
            .join(asset.name)
    }
}

fn prune_update_cache(
    download_directory: &Path,
    keep_directory: &Path,
    obsolete_before: SystemTime,
) -> io::Result<()> {
    let entries = match std::fs::read_dir(download_directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let entry = entry?;
        let version_path = entry.path();
        let metadata = version_path.symlink_metadata()?;
        if !metadata.is_dir() {
            continue;
        }
        let obsolete = metadata
            .modified()
            .is_ok_and(|modified| modified <= obsolete_before);
        for candidate in std::fs::read_dir(&version_path)? {
            let candidate = candidate?;
            let name = candidate.file_name();
            let name = name.to_string_lossy();
            if name.starts_with(".viewr-download-") && name.ends_with(".tmp") {
                let metadata = candidate.path().symlink_metadata()?;
                if metadata.is_file() || metadata.file_type().is_symlink() {
                    std::fs::remove_file(candidate.path())?;
                }
            }
        }
        if version_path != keep_directory && obsolete {
            std::fs::remove_dir_all(version_path)?;
        }
    }
    Ok(())
}

fn valid_stored_version(value: &str) -> Option<Version> {
    if value.len() > 64 {
        return None;
    }
    let version = Version::parse(value).ok()?;
    if version.pre.is_empty() && version.build.is_empty() && version.to_string() == value {
        Some(version)
    } else {
        None
    }
}

fn check_due(previous: Option<u64>, now: u64, minimum_age: u64) -> bool {
    previous.is_none_or(|previous| {
        now.checked_sub(previous)
            .is_none_or(|elapsed| elapsed >= minimum_age)
    })
}

pub(crate) fn open_lock_file(path: &Path) -> io::Result<File> {
    if path
        .symlink_metadata()
        .is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "update lock cannot be a symbolic link",
        ));
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

pub(crate) fn ensure_private_directory(path: &Path) -> io::Result<()> {
    match path.symlink_metadata() {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "update directory cannot be a symbolic link",
            ));
        }
        Ok(metadata) if !metadata.is_dir() => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "update directory path is not a directory",
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            std::fs::create_dir_all(path)?;
        }
        Err(error) => return Err(error),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn replace_file_durable(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "update state path has no parent",
        )
    })?;
    let permissions = match path.symlink_metadata() {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "refusing to replace a symbolic link",
            ));
        }
        Ok(metadata) if metadata.is_file() => Some(metadata.permissions()),
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "update state path is not a regular file",
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    let mut temporary = tempfile::Builder::new()
        .prefix(".viewr-update-state-")
        .suffix(".tmp")
        .tempfile_in(parent)?;
    temporary.write_all(bytes)?;
    if let Some(permissions) = permissions {
        temporary.as_file().set_permissions(permissions)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        temporary
            .as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    sync_parent(parent)
}

fn sync_parent(parent: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(parent)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = parent;
        Ok(())
    }
}

fn verify_file(path: &Path, asset: &ReleaseAsset) -> Result<(), UpdateError> {
    let metadata = path.symlink_metadata()?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(UpdateError::InvalidRelease(
            "the cached update is not a regular file".into(),
        ));
    }
    if metadata.len() != asset.size {
        return Err(UpdateError::InvalidRelease(
            "the cached update has the wrong size".into(),
        ));
    }
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    io::copy(&mut file, &mut hasher)?;
    let digest: [u8; 32] = hasher.finalize().into();
    if digest != asset.sha256 {
        return Err(UpdateError::InvalidRelease(
            "the cached update failed SHA-256 verification".into(),
        ));
    }
    Ok(())
}

#[cfg(any(target_os = "macos", test))]
fn quarantine_value(timestamp: u64, event_id: &str) -> String {
    format!("0083;{timestamp:x};Viewr;{event_id}")
}

#[cfg(target_os = "macos")]
fn quarantine_event_id(path: &Path, asset: &ReleaseAsset) -> String {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let sequence = QUARANTINE_EVENT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let mut hasher = Sha256::new();
    hasher.update(timestamp.to_le_bytes());
    hasher.update(std::process::id().to_le_bytes());
    hasher.update(sequence.to_le_bytes());
    hasher.update(path.as_os_str().as_encoded_bytes());
    hasher.update(asset.url.as_bytes());
    let digest = hasher.finalize();
    let mut id: [u8; 16] = digest[..16]
        .try_into()
        .expect("a SHA-256 digest contains 16 bytes");
    id[6] = (id[6] & 0x0f) | 0x40;
    id[8] = (id[8] & 0x3f) | 0x80;
    format!(
        "{:02X}{:02X}{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
        id[0],
        id[1],
        id[2],
        id[3],
        id[4],
        id[5],
        id[6],
        id[7],
        id[8],
        id[9],
        id[10],
        id[11],
        id[12],
        id[13],
        id[14],
        id[15]
    )
}

#[cfg(any(target_os = "macos", test))]
fn valid_quarantine_event_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 36
        && [8, 13, 18, 23]
            .into_iter()
            .all(|index| bytes[index] == b'-')
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| [8, 13, 18, 23].contains(&index) || byte.is_ascii_hexdigit())
        && bytes[14] == b'4'
        && matches!(bytes[19].to_ascii_uppercase(), b'8' | b'9' | b'A' | b'B')
}

#[cfg(any(target_os = "windows", test))]
fn zone_identifier(source_url: &str) -> String {
    format!("[ZoneTransfer]\r\nZoneId=3\r\nHostUrl={source_url}\r\n")
}

#[cfg(target_os = "windows")]
fn zone_identifier_path(path: &Path) -> PathBuf {
    let mut stream = path.as_os_str().to_owned();
    stream.push(":Zone.Identifier");
    PathBuf::from(stream)
}

fn apply_download_provenance(path: &Path, asset: &ReleaseAsset) -> Result<(), UpdateError> {
    #[cfg(target_os = "macos")]
    {
        let value = quarantine_value(unix_time(), &quarantine_event_id(path, asset));
        let output = Command::new("/usr/bin/xattr")
            .args(["-w", "com.apple.quarantine", &value])
            .arg(path)
            .output()?;
        if !output.status.success() {
            return Err(UpdateError::Io(io::Error::other(format!(
                "could not apply macOS download quarantine: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ))));
        }
    }
    #[cfg(target_os = "windows")]
    {
        let mut marker = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(zone_identifier_path(path))?;
        marker.write_all(zone_identifier(&asset.url).as_bytes())?;
        marker.sync_all()?;
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = (path, asset);
    }
    Ok(())
}

fn verify_download_provenance(path: &Path, _asset: &ReleaseAsset) -> Result<(), UpdateError> {
    #[cfg(target_os = "macos")]
    {
        let output = Command::new("/usr/bin/xattr")
            .args(["-p", "com.apple.quarantine"])
            .arg(path)
            .output()?;
        if !output.status.success() {
            return Err(UpdateError::InvalidRelease(
                "the cached update is missing macOS download quarantine".into(),
            ));
        }
        let value = String::from_utf8_lossy(&output.stdout);
        let fields: Vec<&str> = value.trim_end().splitn(4, ';').collect();
        if fields.len() != 4
            || fields[0] != "0083"
            || fields[2] != "Viewr"
            || !valid_quarantine_event_id(fields[3])
        {
            return Err(UpdateError::InvalidRelease(
                "the cached update has invalid macOS download quarantine".into(),
            ));
        }
    }
    #[cfg(target_os = "windows")]
    {
        let marker = std::fs::read_to_string(zone_identifier_path(path))?;
        if marker != zone_identifier(&_asset.url) {
            return Err(UpdateError::InvalidRelease(
                "the cached update has invalid Windows Internet-zone metadata".into(),
            ));
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = (path, _asset);
    }
    Ok(())
}

fn prepare_download_for_handoff(path: &Path, asset: &ReleaseAsset) -> Result<(), UpdateError> {
    verify_file(path, asset)?;
    apply_download_provenance(path, asset)?;
    verify_download_provenance(path, asset)
}

pub(crate) fn download_release(
    release: &Release,
    asset: &ReleaseAsset,
    store: &UpdateStore,
    progress: &AtomicU64,
) -> Result<PathBuf, UpdateError> {
    let _download_lock = store.claim_download()?;
    let destination = store.destination(release, asset);
    let directory = destination
        .parent()
        .expect("download destination has a parent");
    ensure_private_directory(&store.download_directory)?;
    ensure_private_directory(directory)?;
    let obsolete_before = SystemTime::now()
        .checked_sub(UPDATE_CACHE_RETENTION)
        .unwrap_or(UNIX_EPOCH);
    if let Err(error) = prune_update_cache(&store.download_directory, directory, obsolete_before) {
        eprintln!("could not prune the update cache: {error}");
    }

    match destination.symlink_metadata() {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(UpdateError::InvalidRelease(
                "the cached update path is not a regular file".into(),
            ));
        }
        Ok(_) if verify_file(&destination, asset).is_ok() => {
            match prepare_download_for_handoff(&destination, asset) {
                Ok(()) => {
                    progress.store(asset.size, Ordering::Relaxed);
                    return Ok(destination);
                }
                Err(error) => {
                    let _ = std::fs::remove_file(&destination);
                    return Err(error);
                }
            }
        }
        Ok(_) => std::fs::remove_file(&destination)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }

    let client = HttpClient::new();
    let response = client.get(
        &asset.url,
        RequestPurpose::ReleaseAsset,
        MAX_DOWNLOAD_DURATION,
    )?;
    if let Some(length) = response.header("Content-Length") {
        let length = length.parse::<u64>().map_err(|_| {
            UpdateError::Network("the update download has an invalid Content-Length".into())
        })?;
        if length != asset.size {
            return Err(UpdateError::InvalidRelease(
                "the update download size disagrees with GitHub metadata".into(),
            ));
        }
    }

    persist_download_stream(response.into_reader(), &destination, asset, progress)?;
    Ok(destination)
}

fn persist_download_stream(
    mut reader: impl io::Read,
    destination: &Path,
    asset: &ReleaseAsset,
    progress: &AtomicU64,
) -> Result<(), UpdateError> {
    let directory = destination.parent().ok_or_else(|| {
        UpdateError::InvalidRelease("the update download path has no parent directory".into())
    })?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".viewr-download-")
        .suffix(".tmp")
        .tempfile_in(directory)?;
    let mut hasher = Sha256::new();
    let mut received = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    let started = Instant::now();
    loop {
        if started.elapsed() > MAX_DOWNLOAD_DURATION {
            return Err(UpdateError::Network("the update download timed out".into()));
        }
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        received = received
            .checked_add(count as u64)
            .ok_or_else(|| UpdateError::InvalidRelease("update size overflow".into()))?;
        if received > asset.size || received > MAX_PACKAGE_BYTES {
            return Err(UpdateError::InvalidRelease(
                "the update download exceeded its declared size".into(),
            ));
        }
        temporary.write_all(&buffer[..count])?;
        hasher.update(&buffer[..count]);
        progress.store(received, Ordering::Relaxed);
    }
    if received != asset.size {
        return Err(UpdateError::InvalidRelease(format!(
            "the update download ended early ({received} of {} bytes)",
            asset.size
        )));
    }
    let digest: [u8; 32] = hasher.finalize().into();
    if digest != asset.sha256 {
        return Err(UpdateError::InvalidRelease(
            "the update download failed SHA-256 verification".into(),
        ));
    }
    temporary.as_file().sync_all()?;
    if destination
        .symlink_metadata()
        .is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        return Err(UpdateError::InvalidRelease(
            "refusing to replace a symbolic-link update path".into(),
        ));
    }
    temporary
        .persist(destination)
        .map_err(|error| error.error)?;
    let finalized = sync_parent(directory)
        .map_err(UpdateError::from)
        .and_then(|()| prepare_download_for_handoff(destination, asset));
    if let Err(error) = finalized {
        let _ = std::fs::remove_file(destination);
        let _ = sync_parent(directory);
        return Err(error);
    }
    Ok(())
}

pub(crate) fn verify_before_open(path: &Path, asset: &ReleaseAsset) -> Result<(), UpdateError> {
    verify_file(path, asset)?;
    verify_download_provenance(path, asset)
}

#[derive(Clone, Debug)]
enum UpdateState {
    Idle,
    Checking {
        id: u64,
        source: CheckSource,
    },
    UpToDate {
        latest: Version,
    },
    Available {
        release: Release,
    },
    Deferred {
        release: Release,
    },
    Downloading {
        id: u64,
        release: Release,
        asset: ReleaseAsset,
        progress: Arc<AtomicU64>,
    },
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    PreparingApplication {
        id: u64,
        release: Release,
    },
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    StartingApplication {
        id: u64,
        release: Release,
    },
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    ApplyingApplication {
        release: Release,
    },
    Ready {
        release: Release,
        asset: ReleaseAsset,
        path: PathBuf,
    },
    VerifyingInstaller {
        id: u64,
        release: Release,
        path: PathBuf,
    },
    InstallerOpened {
        release: Release,
        path: PathBuf,
    },
    Failed {
        message: String,
        retry: Retry,
    },
}

#[derive(Clone, Debug)]
enum Retry {
    None,
    Check,
    Download(Box<DownloadRetry>),
}

#[derive(Clone, Debug)]
struct DownloadRetry {
    release: Release,
    asset: ReleaseAsset,
}

enum WorkerEvent {
    CheckFinished {
        id: u64,
        claimed_source: CheckSource,
        result: Result<Option<CheckOutcome>, UpdateError>,
    },
    DownloadFinished {
        id: u64,
        release: Release,
        asset: ReleaseAsset,
        result: Result<PathBuf, UpdateError>,
    },
    #[cfg(target_os = "macos")]
    ApplicationPrepared {
        id: u64,
        release: Release,
        asset: ReleaseAsset,
        result: Result<crate::macos_update::PreparedUpdate, UpdateError>,
    },
    #[cfg(target_os = "macos")]
    ApplicationStarted {
        id: u64,
        release: Release,
        asset: ReleaseAsset,
        result: Result<(), UpdateError>,
    },
    InstallerVerified {
        id: u64,
        release: Release,
        asset: ReleaseAsset,
        path: PathBuf,
        result: Result<(), UpdateError>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CommandSpec {
    program: OsString,
    arguments: Vec<OsString>,
}

fn installer_command_for(
    target_os: &str,
    path: &Path,
    windows_system_directory: Option<&Path>,
) -> Option<CommandSpec> {
    match target_os {
        "macos" => Some(CommandSpec {
            program: "/usr/bin/open".into(),
            arguments: vec![path.as_os_str().to_owned()],
        }),
        "windows" => Some(CommandSpec {
            program: windows_system_directory?
                .join("msiexec.exe")
                .into_os_string(),
            arguments: vec!["/i".into(), path.as_os_str().to_owned()],
        }),
        "linux" => Some(CommandSpec {
            program: "/usr/bin/xdg-open".into(),
            arguments: vec![path.as_os_str().to_owned()],
        }),
        _ => None,
    }
}

fn reveal_command_for(
    target_os: &str,
    path: &Path,
    windows_directory: Option<&Path>,
) -> Option<CommandSpec> {
    match target_os {
        "macos" => Some(CommandSpec {
            program: "/usr/bin/open".into(),
            arguments: vec!["-R".into(), path.as_os_str().to_owned()],
        }),
        "windows" => Some(CommandSpec {
            program: windows_directory?.join("explorer.exe").into_os_string(),
            arguments: vec![format!("/select,{}", path.display()).into()],
        }),
        "linux" => Some(CommandSpec {
            program: "/usr/bin/xdg-open".into(),
            arguments: vec![path.parent()?.as_os_str().to_owned()],
        }),
        _ => None,
    }
}

#[cfg(target_os = "windows")]
fn windows_known_directory(
    getter: unsafe extern "system" fn(*mut u16, u32) -> u32,
) -> io::Result<PathBuf> {
    use std::os::windows::ffi::OsStringExt as _;

    let mut buffer = vec![0u16; 260];
    loop {
        // SAFETY: `buffer` is writable for the advertised length. The Win32
        // functions write at most that many UTF-16 code units and return the
        // required length when the buffer is too small.
        let length = unsafe { getter(buffer.as_mut_ptr(), buffer.len() as u32) };
        if length == 0 {
            return Err(io::Error::last_os_error());
        }
        let length = length as usize;
        if length < buffer.len() {
            return Ok(PathBuf::from(OsString::from_wide(&buffer[..length])));
        }
        buffer.resize(length.saturating_add(1), 0);
    }
}

fn installer_command(path: &Path) -> Result<CommandSpec, UpdateError> {
    #[cfg(target_os = "windows")]
    let windows_system_directory = Some(windows_known_directory(
        windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW,
    )?);
    #[cfg(not(target_os = "windows"))]
    let windows_system_directory: Option<PathBuf> = None;

    installer_command_for(
        std::env::consts::OS,
        path,
        windows_system_directory.as_deref(),
    )
    .ok_or_else(|| {
        UpdateError::InvalidRelease("opening installers is unsupported on this platform".into())
    })
}

fn reveal_command(path: &Path) -> Result<CommandSpec, UpdateError> {
    #[cfg(target_os = "windows")]
    let windows_directory = Some(windows_known_directory(
        windows_sys::Win32::System::SystemInformation::GetWindowsDirectoryW,
    )?);
    #[cfg(not(target_os = "windows"))]
    let windows_directory: Option<PathBuf> = None;

    reveal_command_for(std::env::consts::OS, path, windows_directory.as_deref()).ok_or_else(|| {
        UpdateError::InvalidRelease(
            "showing downloaded files is unsupported on this platform".into(),
        )
    })
}

fn spawn_command(spec: &CommandSpec) -> io::Result<()> {
    Command::new(&spec.program)
        .args(&spec.arguments)
        .spawn()
        .map(|_| ())
}

fn unix_time() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Eframe-thread update controller. Worker threads only send terminal events;
/// progress lives in an atomic counter so slow frames cannot accumulate an
/// unbounded queue.
pub(crate) struct UpdateManager {
    ctx: egui::Context,
    state: UpdateState,
    store: Option<UpdateStore>,
    store_error: Option<String>,
    sender: mpsc::Sender<WorkerEvent>,
    receiver: mpsc::Receiver<WorkerEvent>,
    next_id: u64,
    automatic_checks_enabled: bool,
    automatic_due_at: Option<Instant>,
    preference_refresh_due_at: Instant,
    dialog_open: bool,
}

impl UpdateManager {
    pub(crate) fn new(ctx: egui::Context) -> Self {
        Self::with_store(ctx, UpdateStore::from_system())
    }

    fn with_store(ctx: egui::Context, store_result: Result<UpdateStore, UpdateError>) -> Self {
        let (sender, receiver) = mpsc::channel();
        let (store, store_error, automatic_checks_enabled) = match store_result {
            Ok(store) => match store.initialize(&current_version()) {
                Ok(enabled) => (Some(store), None, enabled),
                Err(error) => (Some(store), Some(error.to_string()), false),
            },
            Err(error) => (None, Some(error.to_string()), false),
        };
        let automatic_due_at =
            automatic_checks_enabled.then(|| Instant::now() + Duration::from_secs(4));
        if let Some(deadline) = automatic_due_at {
            ctx.request_repaint_after(deadline.saturating_duration_since(Instant::now()));
        }
        Self {
            ctx,
            state: UpdateState::Idle,
            store,
            store_error,
            sender,
            receiver,
            next_id: 1,
            automatic_checks_enabled,
            automatic_due_at,
            preference_refresh_due_at: Instant::now(),
            dialog_open: false,
        }
    }

    pub(crate) fn poll(&mut self) {
        if let Some(deadline) = self.automatic_due_at {
            let now = Instant::now();
            if now >= deadline {
                self.automatic_due_at = None;
                if matches!(self.state, UpdateState::Idle) {
                    self.start_check(CheckSource::Automatic);
                }
            } else {
                self.ctx.request_repaint_after(deadline.duration_since(now));
            }
        }
        while let Ok(event) = self.receiver.try_recv() {
            self.handle_event(event);
        }
        if matches!(self.state, UpdateState::Downloading { .. }) {
            self.ctx.request_repaint_after(Duration::from_millis(100));
        }
    }

    pub(crate) fn set_automatic_checks(&mut self, enabled: bool) {
        let result = self
            .store
            .as_ref()
            .ok_or_else(|| {
                self.store_error
                    .clone()
                    .unwrap_or_else(|| "update storage is unavailable".into())
            })
            .and_then(|store| {
                store
                    .set_automatic_checks_enabled(enabled)
                    .map_err(|error| error.to_string())
            });
        if let Err(message) = result {
            self.state = UpdateState::Failed {
                message,
                retry: Retry::None,
            };
            self.dialog_open = true;
            return;
        }
        self.automatic_checks_enabled = enabled;
        self.store_error = None;
        if enabled && matches!(self.state, UpdateState::Idle) {
            let delay = Duration::from_secs(1);
            self.automatic_due_at = Some(Instant::now() + delay);
            self.ctx.request_repaint_after(delay);
        } else if !enabled {
            self.automatic_due_at = None;
        }
    }

    pub(crate) fn automatic_checks_enabled(&self) -> bool {
        self.automatic_checks_enabled
    }

    pub(crate) fn refresh_automatic_preference(&mut self) {
        let now = Instant::now();
        if now < self.preference_refresh_due_at {
            self.ctx
                .request_repaint_after(self.preference_refresh_due_at.duration_since(now));
            return;
        }
        self.preference_refresh_due_at = now + Duration::from_millis(250);
        self.ctx.request_repaint_after(Duration::from_millis(250));
        let Some(store) = self.store.as_ref() else {
            return;
        };
        match store.automatic_checks_enabled() {
            Ok(enabled) if enabled != self.automatic_checks_enabled => {
                self.store_error = None;
                self.automatic_checks_enabled = enabled;
                if enabled && matches!(self.state, UpdateState::Idle) {
                    let delay = Duration::from_secs(1);
                    self.automatic_due_at = Some(now + delay);
                    self.ctx.request_repaint_after(delay);
                } else if !enabled {
                    self.automatic_due_at = None;
                }
            }
            Ok(_) => {
                self.store_error = None;
            }
            Err(error) => {
                let message = error.to_string();
                if self.store_error.as_deref() != Some(&message) {
                    eprintln!("could not refresh update preference: {message}");
                }
                self.store_error = Some(message);
                self.automatic_checks_enabled = false;
                self.automatic_due_at = None;
            }
        }
    }

    pub(crate) fn check_now(&mut self) {
        self.dialog_open = true;
        if let UpdateState::Checking { source, .. } = &mut self.state {
            *source = CheckSource::Manual;
            return;
        }
        self.start_check(CheckSource::Manual);
    }

    pub(crate) fn open_status(&mut self) {
        if let UpdateState::Deferred { release } = &self.state {
            self.state = UpdateState::Available {
                release: release.clone(),
            };
        }
        self.dialog_open = true;
    }

    pub(crate) fn has_available_update(&self) -> bool {
        matches!(
            self.state,
            UpdateState::Available { .. }
                | UpdateState::Deferred { .. }
                | UpdateState::Downloading { .. }
                | UpdateState::PreparingApplication { .. }
                | UpdateState::StartingApplication { .. }
                | UpdateState::ApplyingApplication { .. }
                | UpdateState::Ready { .. }
                | UpdateState::VerifyingInstaller { .. }
                | UpdateState::InstallerOpened { .. }
        )
    }

    pub(crate) fn has_status_details(&self) -> bool {
        !matches!(self.state, UpdateState::Idle)
    }

    pub(crate) fn blocks_app_input(&self) -> bool {
        self.dialog_open
    }

    pub(crate) fn status_text(&self) -> String {
        match &self.state {
            UpdateState::Idle => {
                format!("Viewr {} is installed", current_version_text())
            }
            UpdateState::Checking { .. } => "Checking for updates…".into(),
            UpdateState::UpToDate { latest } => {
                format!("Viewr is up to date ({latest})")
            }
            UpdateState::Available { release } | UpdateState::Deferred { release } => {
                format!("Viewr {} is available", release.version)
            }
            UpdateState::Downloading {
                asset, progress, ..
            } => format!(
                "Downloading {} ({:.0}%)",
                asset.kind.noun(),
                download_percent(progress.load(Ordering::Relaxed), asset.size)
            ),
            UpdateState::PreparingApplication { release, .. } => {
                format!("Preparing Viewr {}", release.version)
            }
            UpdateState::StartingApplication { release, .. } => {
                format!("Starting Viewr {} update", release.version)
            }
            UpdateState::ApplyingApplication { release } => {
                format!("Restarting into Viewr {}", release.version)
            }
            UpdateState::Ready { release, .. } => {
                format!("Viewr {} is ready", release.version)
            }
            UpdateState::VerifyingInstaller { release, .. } => {
                format!("Verifying Viewr {} installer", release.version)
            }
            UpdateState::InstallerOpened { release, .. } => {
                format!("Viewr {} installer opened", release.version)
            }
            UpdateState::Failed { message, .. } => {
                format!("Update error: {message}")
            }
        }
    }

    fn next_request_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        id
    }

    fn start_check(&mut self, source: CheckSource) {
        if matches!(
            self.state,
            UpdateState::Checking { .. }
                | UpdateState::Downloading { .. }
                | UpdateState::PreparingApplication { .. }
                | UpdateState::StartingApplication { .. }
                | UpdateState::ApplyingApplication { .. }
                | UpdateState::VerifyingInstaller { .. }
        ) {
            return;
        }
        let Some(store) = self.store.clone() else {
            let message = self
                .store_error
                .clone()
                .unwrap_or_else(|| "update storage is unavailable".into());
            if source == CheckSource::Automatic {
                eprintln!("automatic update check skipped: {message}");
                self.state = UpdateState::Idle;
            } else {
                self.state = UpdateState::Failed {
                    message,
                    retry: Retry::Check,
                };
            }
            return;
        };
        let id = self.next_request_id();
        self.state = UpdateState::Checking { id, source };
        let sender = self.sender.clone();
        let ctx = self.ctx.clone();
        let spawn = std::thread::Builder::new()
            .name("viewr-update-check".into())
            .spawn(move || {
                let result = match store.claim_check(source, unix_time()) {
                    Ok(Some(_operation_lock)) => check_latest_release().map(Some),
                    Ok(None) => Ok(None),
                    Err(error) => Err(error),
                };
                let _ = sender.send(WorkerEvent::CheckFinished {
                    id,
                    claimed_source: source,
                    result,
                });
                ctx.request_repaint();
            });
        if let Err(error) = spawn {
            self.state = UpdateState::Failed {
                message: format!("could not start the update check: {error}"),
                retry: Retry::Check,
            };
        }
    }

    fn start_download(&mut self, release: Release, asset: ReleaseAsset) {
        let Some(store) = self.store.clone() else {
            self.state = UpdateState::Failed {
                message: self
                    .store_error
                    .clone()
                    .unwrap_or_else(|| "update storage is unavailable".into()),
                retry: Retry::Download(Box::new(DownloadRetry { release, asset })),
            };
            return;
        };
        let id = self.next_request_id();
        let progress = Arc::new(AtomicU64::new(0));
        self.state = UpdateState::Downloading {
            id,
            release: release.clone(),
            asset: asset.clone(),
            progress: progress.clone(),
        };
        self.dialog_open = true;
        let sender = self.sender.clone();
        let ctx = self.ctx.clone();
        let worker_release = release.clone();
        let worker_asset = asset.clone();
        let spawn = std::thread::Builder::new()
            .name("viewr-update-download".into())
            .spawn(move || {
                let result = download_release(&worker_release, &worker_asset, &store, &progress);
                let _ = sender.send(WorkerEvent::DownloadFinished {
                    id,
                    release: worker_release,
                    asset: worker_asset,
                    result,
                });
                ctx.request_repaint();
            });
        if let Err(error) = spawn {
            self.state = UpdateState::Failed {
                message: format!("could not start the update download: {error}"),
                retry: Retry::Download(Box::new(DownloadRetry { release, asset })),
            };
        }
    }

    fn start_installer_verification(
        &mut self,
        release: Release,
        asset: ReleaseAsset,
        path: PathBuf,
    ) {
        let id = self.next_request_id();
        self.state = UpdateState::VerifyingInstaller {
            id,
            release: release.clone(),
            path: path.clone(),
        };
        self.dialog_open = true;
        let sender = self.sender.clone();
        let ctx = self.ctx.clone();
        let worker_release = release.clone();
        let worker_asset = asset.clone();
        let worker_path = path.clone();
        let spawn = std::thread::Builder::new()
            .name("viewr-update-verify".into())
            .spawn(move || {
                let result = verify_before_open(&worker_path, &worker_asset);
                let _ = sender.send(WorkerEvent::InstallerVerified {
                    id,
                    release: worker_release,
                    asset: worker_asset,
                    path: worker_path,
                    result,
                });
                ctx.request_repaint();
            });
        if let Err(error) = spawn {
            self.state = UpdateState::Failed {
                message: format!("could not start installer verification: {error}"),
                retry: Retry::Download(Box::new(DownloadRetry { release, asset })),
            };
        }
    }

    #[cfg(target_os = "macos")]
    fn start_application_preparation(
        &mut self,
        release: Release,
        asset: ReleaseAsset,
        path: PathBuf,
    ) {
        let id = self.next_request_id();
        self.state = UpdateState::PreparingApplication {
            id,
            release: release.clone(),
        };
        self.dialog_open = true;
        let sender = self.sender.clone();
        let ctx = self.ctx.clone();
        let worker_release = release.clone();
        let worker_asset = asset.clone();
        let worker_path = path.clone();
        let version = release.version.clone();
        let spawn = std::thread::Builder::new()
            .name("viewr-update-prepare".into())
            .spawn(move || {
                let result = verify_before_open(&worker_path, &worker_asset)
                    .and_then(|()| crate::macos_update::prepare(&worker_path, &version));
                let event = WorkerEvent::ApplicationPrepared {
                    id,
                    release: worker_release,
                    asset: worker_asset,
                    result,
                };
                if let Err(error) = sender.send(event)
                    && let WorkerEvent::ApplicationPrepared {
                        result: Ok(prepared),
                        ..
                    } = error.0
                {
                    crate::macos_update::discard(prepared);
                }
                ctx.request_repaint();
            });
        if let Err(error) = spawn {
            self.state = UpdateState::Failed {
                message: format!("could not start application preparation: {error}"),
                retry: Retry::Download(Box::new(DownloadRetry { release, asset })),
            };
        }
    }

    #[cfg(target_os = "macos")]
    fn start_application_helper(
        &mut self,
        release: Release,
        asset: ReleaseAsset,
        mut prepared: crate::macos_update::PreparedUpdate,
    ) {
        let id = self.next_request_id();
        self.state = UpdateState::StartingApplication {
            id,
            release: release.clone(),
        };
        let sender = self.sender.clone();
        let ctx = self.ctx.clone();
        let spawn = std::thread::Builder::new()
            .name("viewr-update-start-helper".into())
            .spawn(move || {
                let result = crate::macos_update::spawn(&mut prepared);
                if result.is_err() {
                    crate::macos_update::discard(prepared);
                }
                let _ = sender.send(WorkerEvent::ApplicationStarted {
                    id,
                    release,
                    asset,
                    result,
                });
                ctx.request_repaint();
            });
        if let Err(error) = spawn {
            self.state = UpdateState::Failed {
                message: format!("could not start the application update helper: {error}"),
                retry: Retry::None,
            };
        }
    }

    fn handle_event(&mut self, event: WorkerEvent) {
        match event {
            WorkerEvent::CheckFinished {
                id,
                claimed_source,
                result,
            } => {
                let source = match &self.state {
                    UpdateState::Checking { id: active, source } if *active == id => *source,
                    _ => return,
                };
                match result {
                    Ok(None) => {
                        if source == CheckSource::Manual && claimed_source == CheckSource::Automatic
                        {
                            self.state = UpdateState::Idle;
                            self.start_check(CheckSource::Manual);
                        } else if source == CheckSource::Manual {
                            self.state = UpdateState::Failed {
                                message: "Another update check ran moments ago. Try again in a few seconds.".into(),
                                retry: Retry::Check,
                            };
                            self.dialog_open = true;
                        } else {
                            self.state = UpdateState::Idle;
                        }
                    }
                    Ok(Some(CheckOutcome::Current(latest))) => {
                        self.state = if source == CheckSource::Manual {
                            self.dialog_open = true;
                            UpdateState::UpToDate { latest }
                        } else {
                            UpdateState::Idle
                        };
                    }
                    Ok(Some(CheckOutcome::Available(release))) => {
                        let skipped = self
                            .store
                            .as_ref()
                            .and_then(|store| store.skipped_version().ok())
                            .flatten();
                        if source == CheckSource::Automatic
                            && skipped.as_ref() == Some(&release.version)
                        {
                            self.state = UpdateState::Idle;
                        } else {
                            self.state = UpdateState::Available { release };
                            self.dialog_open = true;
                        }
                    }
                    Err(error) if source == CheckSource::Automatic => {
                        eprintln!("automatic update check failed: {error}");
                        self.state = UpdateState::Idle;
                    }
                    Err(error) => {
                        self.state = UpdateState::Failed {
                            message: error.to_string(),
                            retry: Retry::Check,
                        };
                        self.dialog_open = true;
                    }
                }
            }
            WorkerEvent::DownloadFinished {
                id,
                release,
                asset,
                result,
            } => {
                if !matches!(
                    self.state,
                    UpdateState::Downloading {
                        id: active,
                        ..
                    } if active == id
                ) {
                    return;
                }
                match result {
                    Ok(path) if asset.kind == PackageKind::Application => {
                        #[cfg(target_os = "macos")]
                        self.start_application_preparation(release, asset, path);
                        #[cfg(not(target_os = "macos"))]
                        {
                            let _ = path;
                            self.state = UpdateState::Failed {
                                message:
                                    "application self-updates are unsupported on this platform"
                                        .into(),
                                retry: Retry::Download(Box::new(DownloadRetry { release, asset })),
                            };
                        }
                    }
                    Ok(path) => {
                        self.state = UpdateState::Ready {
                            release,
                            asset,
                            path,
                        };
                    }
                    Err(error) => {
                        self.state = UpdateState::Failed {
                            message: error.to_string(),
                            retry: Retry::Download(Box::new(DownloadRetry { release, asset })),
                        };
                    }
                }
                self.dialog_open = true;
            }
            #[cfg(target_os = "macos")]
            WorkerEvent::ApplicationPrepared {
                id,
                release,
                asset,
                result,
            } => {
                if !matches!(
                    self.state,
                    UpdateState::PreparingApplication {
                        id: active,
                        ..
                    } if active == id
                ) {
                    if let Ok(prepared) = result {
                        crate::macos_update::discard(prepared);
                    }
                    return;
                }
                match result {
                    Ok(prepared) => self.start_application_helper(release, asset, prepared),
                    Err(error) => {
                        self.state = UpdateState::Failed {
                            message: error.to_string(),
                            retry: Retry::Download(Box::new(DownloadRetry { release, asset })),
                        };
                        self.dialog_open = true;
                    }
                }
            }
            #[cfg(target_os = "macos")]
            WorkerEvent::ApplicationStarted {
                id,
                release,
                asset,
                result,
            } => {
                if !matches!(
                    self.state,
                    UpdateState::StartingApplication { id: active, .. } if active == id
                ) {
                    if result.is_ok() {
                        self.ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                    return;
                }
                match result {
                    Ok(()) => {
                        self.state = UpdateState::ApplyingApplication { release };
                        self.dialog_open = true;
                        self.ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                    Err(error) => {
                        self.state = UpdateState::Failed {
                            message: error.to_string(),
                            retry: Retry::Download(Box::new(DownloadRetry { release, asset })),
                        };
                        self.dialog_open = true;
                    }
                }
            }
            WorkerEvent::InstallerVerified {
                id,
                release,
                asset,
                path,
                result,
            } => {
                if !matches!(
                    self.state,
                    UpdateState::VerifyingInstaller {
                        id: active,
                        ..
                    } if active == id
                ) {
                    return;
                }
                let result = result
                    .and_then(|()| installer_command(&path))
                    .and_then(|spec| spawn_command(&spec).map_err(UpdateError::from));
                match result {
                    Ok(()) => {
                        self.state = UpdateState::InstallerOpened { release, path };
                    }
                    Err(error) => {
                        self.state = UpdateState::Failed {
                            message: error.to_string(),
                            retry: Retry::Download(Box::new(DownloadRetry { release, asset })),
                        };
                    }
                }
                self.dialog_open = true;
            }
        }
    }

    pub(crate) fn show(&mut self) {
        if !self.dialog_open {
            return;
        }
        let snapshot = self.state.clone();
        let response =
            egui::Modal::new(egui::Id::new("viewr-update-modal")).show(&self.ctx, |ui| {
                ui.set_min_width(460.0);
                ui.set_max_width(620.0);
                self.modal_contents(ui, &snapshot)
            });
        let mut action = response.inner;
        if matches!(action, UpdateAction::None) && response.should_close() {
            action = match snapshot {
                UpdateState::Available { .. } => UpdateAction::Later,
                UpdateState::UpToDate { .. }
                | UpdateState::Failed { .. }
                | UpdateState::InstallerOpened { .. } => UpdateAction::Close,
                _ => UpdateAction::None,
            };
        }
        self.apply_action(action);
    }

    fn modal_contents(&self, ui: &mut egui::Ui, state: &UpdateState) -> UpdateAction {
        match state {
            UpdateState::Idle | UpdateState::Deferred { .. } => UpdateAction::Close,
            UpdateState::Checking { source, .. } => {
                ui.heading("Checking for updates");
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(match source {
                        CheckSource::Automatic => "Looking for the newest stable release…",
                        CheckSource::Manual => "Checking GitHub Releases…",
                    });
                });
                UpdateAction::None
            }
            UpdateState::UpToDate { latest } => {
                ui.heading("Viewr is up to date");
                ui.label(format!(
                    "You are running {}. The latest stable release is {latest}.",
                    current_version_text()
                ));
                ui.add_space(12.0);
                if ui.button("Close").clicked() {
                    UpdateAction::Close
                } else {
                    UpdateAction::None
                }
            }
            UpdateState::Available { release } => {
                ui.heading(format!("Viewr {} is available", release.version));
                ui.label(format!(
                    "You are currently running Viewr {}.",
                    current_version_text()
                ));
                ui.add_space(8.0);
                ui.label(egui::RichText::new("What’s new").strong());
                egui::ScrollArea::vertical()
                    .id_salt("viewr-update-notes")
                    .max_height(260.0)
                    .show(ui, |ui| {
                        show_release_notes(ui, &release.notes);
                    });
                ui.add_space(10.0);
                if let Delivery::ReleasePageOnly { reason } = &release.delivery {
                    ui.label(egui::RichText::new(reason).color(egui::Color32::YELLOW));
                }
                let mut action = UpdateAction::None;
                ui.horizontal_wrapped(|ui| {
                    if let Delivery::Download(asset) = &release.delivery {
                        let label = match asset.kind {
                            PackageKind::Application => "Update and restart",
                            PackageKind::Installer | PackageKind::Portable => "Download now",
                        };
                        if ui.button(label).clicked() {
                            action = UpdateAction::Download;
                        }
                    }
                    if ui.button("View release").clicked() {
                        action = UpdateAction::ViewRelease;
                    }
                    if ui.button("Later").clicked() {
                        action = UpdateAction::Later;
                    }
                    if ui.button("Skip this version").clicked() {
                        action = UpdateAction::Skip;
                    }
                });
                action
            }
            UpdateState::Downloading {
                asset, progress, ..
            } => {
                let received = progress.load(Ordering::Relaxed);
                ui.heading(format!("Downloading {}", asset.kind.noun()));
                ui.add(
                    egui::ProgressBar::new(
                        (received as f64 / asset.size as f64).clamp(0.0, 1.0) as f32
                    )
                    .show_percentage(),
                );
                ui.label(format!(
                    "{} of {}",
                    human_bytes(received),
                    human_bytes(asset.size)
                ));
                ui.label(
                    egui::RichText::new(
                        "Viewr verifies the exact file size and SHA-256 digest before making the download available.",
                    )
                    .weak()
                    .size(11.0),
                );
                UpdateAction::None
            }
            UpdateState::PreparingApplication { .. } => {
                ui.heading("Preparing update");
                ui.spinner();
                ui.label("Viewr is verifying and staging the new application before restarting.");
                UpdateAction::None
            }
            UpdateState::StartingApplication { .. } => {
                ui.heading("Starting update");
                ui.spinner();
                ui.label("Viewr is starting its verified update helper.");
                UpdateAction::None
            }
            UpdateState::ApplyingApplication { release } => {
                ui.heading("Restarting Viewr");
                ui.spinner();
                ui.label(format!(
                    "Viewr {} will open as soon as the application update is complete.",
                    release.version
                ));
                UpdateAction::None
            }
            UpdateState::Ready {
                release,
                asset,
                path,
            } => {
                ui.heading(format!("Viewr {} is ready", release.version));
                ui.label(format!(
                    "{} was downloaded and SHA-256 verified.",
                    asset.name
                ));
                ui.add_space(8.0);
                match asset.kind {
                    PackageKind::Application => {
                        ui.label("The application update is ready to be installed and restarted.");
                    }
                    PackageKind::Installer => {
                        ui.label(
                            egui::RichText::new(
                                "Preview warning: this installer is not yet code-signed. Opening it hands control to your operating system; installation still requires your confirmation.",
                            )
                            .color(egui::Color32::YELLOW),
                        );
                        ui.label(
                            "Close every Viewr window before completing the upgrade, then reopen Viewr.",
                        );
                    }
                    PackageKind::Portable => {
                        ui.label(
                            "This copy of Viewr is portable. The downloaded archive will not replace the running app automatically.",
                        );
                    }
                }
                ui.add_space(10.0);
                let mut action = UpdateAction::None;
                ui.horizontal_wrapped(|ui| {
                    if asset.kind == PackageKind::Installer && ui.button("Open installer").clicked()
                    {
                        action = UpdateAction::OpenInstaller;
                    }
                    if asset.kind != PackageKind::Application && ui.button("Show file").clicked() {
                        action = UpdateAction::RevealFile;
                    }
                    if ui.button("View release").clicked() {
                        action = UpdateAction::ViewRelease;
                    }
                    if ui.button("Later").clicked() {
                        action = UpdateAction::Close;
                    }
                });
                ui.label(
                    egui::RichText::new(path.display().to_string())
                        .weak()
                        .monospace()
                        .size(10.0),
                );
                action
            }
            UpdateState::VerifyingInstaller { .. } => {
                ui.heading("Verifying installer");
                ui.spinner();
                ui.label(
                    "Viewr is checking the downloaded file again before it asks the operating system to open it.",
                );
                UpdateAction::None
            }
            UpdateState::InstallerOpened { release, path } => {
                ui.heading("Installer opened");
                ui.label(format!(
                    "The Viewr {} installer was handed to your operating system.",
                    release.version
                ));
                ui.label("Complete the installer, close all old Viewr windows, and reopen Viewr.");
                ui.label(
                    egui::RichText::new(
                        "Viewr cannot safely restart itself until the system installer has finished.",
                    )
                    .weak(),
                );
                ui.add_space(10.0);
                let mut action = UpdateAction::None;
                ui.horizontal(|ui| {
                    if ui.button("Quit this Viewr window").clicked() {
                        action = UpdateAction::Quit;
                    }
                    if ui.button("Show installer").clicked() {
                        action = UpdateAction::RevealFile;
                    }
                    if ui.button("Keep viewing").clicked() {
                        action = UpdateAction::Close;
                    }
                });
                ui.label(
                    egui::RichText::new(path.display().to_string())
                        .weak()
                        .monospace()
                        .size(10.0),
                );
                action
            }
            UpdateState::Failed { message, retry } => {
                ui.heading("Update could not be completed");
                ui.label(egui::RichText::new(message).color(egui::Color32::LIGHT_RED));
                ui.add_space(10.0);
                let mut action = UpdateAction::None;
                ui.horizontal_wrapped(|ui| {
                    if !matches!(retry, Retry::None) && ui.button("Retry").clicked() {
                        action = UpdateAction::Retry;
                    }
                    if ui.button("View releases").clicked() {
                        action = UpdateAction::ViewReleases;
                    }
                    if ui.button("Close").clicked() {
                        action = UpdateAction::Close;
                    }
                });
                action
            }
        }
    }

    fn apply_action(&mut self, action: UpdateAction) {
        match action {
            UpdateAction::None => {}
            UpdateAction::Close => {
                self.dialog_open = false;
            }
            UpdateAction::Later => {
                if let UpdateState::Available { release } = &self.state {
                    self.state = UpdateState::Deferred {
                        release: release.clone(),
                    };
                }
                self.dialog_open = false;
            }
            UpdateAction::Skip => {
                let Some(release) = (match &self.state {
                    UpdateState::Available { release } => Some(release.clone()),
                    _ => None,
                }) else {
                    return;
                };
                match self
                    .store
                    .as_ref()
                    .ok_or_else(|| {
                        self.store_error
                            .clone()
                            .unwrap_or_else(|| "update storage is unavailable".into())
                    })
                    .and_then(|store| {
                        store
                            .skip(&release.version)
                            .map_err(|error| error.to_string())
                    }) {
                    Ok(()) => {
                        self.state = UpdateState::Idle;
                        self.dialog_open = false;
                    }
                    Err(message) => {
                        self.state = UpdateState::Failed {
                            message,
                            retry: Retry::None,
                        };
                    }
                }
            }
            UpdateAction::Download => {
                let Some((release, asset)) = (match &self.state {
                    UpdateState::Available { release } => match &release.delivery {
                        Delivery::Download(asset) => Some((release.clone(), asset.clone())),
                        Delivery::ReleasePageOnly { .. } => None,
                    },
                    _ => None,
                }) else {
                    return;
                };
                self.start_download(release, asset);
            }
            UpdateAction::Retry => {
                let retry = match &self.state {
                    UpdateState::Failed { retry, .. } => retry.clone(),
                    _ => Retry::None,
                };
                match retry {
                    Retry::None => {}
                    Retry::Check => self.start_check(CheckSource::Manual),
                    Retry::Download(retry) => {
                        self.start_download(retry.release, retry.asset);
                    }
                }
            }
            UpdateAction::ViewRelease => {
                if let Some(release) = release_in_state(&self.state) {
                    self.ctx
                        .open_url(egui::OpenUrl::new_tab(release.page_url()));
                }
            }
            UpdateAction::ViewReleases => {
                self.ctx.open_url(egui::OpenUrl::new_tab(
                    "https://github.com/hunterchen7/viewr/releases",
                ));
            }
            UpdateAction::RevealFile => {
                let Some(path) = path_in_state(&self.state) else {
                    return;
                };
                let result = reveal_command(&path)
                    .and_then(|spec| spawn_command(&spec).map_err(UpdateError::from));
                if let Err(error) = result {
                    self.state = UpdateState::Failed {
                        message: format!("could not show the update file: {error}"),
                        retry: Retry::None,
                    };
                }
            }
            UpdateAction::OpenInstaller => {
                let Some((release, asset, path)) = (match &self.state {
                    UpdateState::Ready {
                        release,
                        asset,
                        path,
                    } => Some((release.clone(), asset.clone(), path.clone())),
                    _ => None,
                }) else {
                    return;
                };
                self.start_installer_verification(release, asset, path);
            }
            UpdateAction::Quit => {
                self.ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }
}

fn release_in_state(state: &UpdateState) -> Option<&Release> {
    match state {
        UpdateState::Available { release }
        | UpdateState::Deferred { release }
        | UpdateState::Downloading { release, .. }
        | UpdateState::PreparingApplication { release, .. }
        | UpdateState::StartingApplication { release, .. }
        | UpdateState::ApplyingApplication { release }
        | UpdateState::Ready { release, .. }
        | UpdateState::VerifyingInstaller { release, .. }
        | UpdateState::InstallerOpened { release, .. } => Some(release),
        UpdateState::Idle
        | UpdateState::Checking { .. }
        | UpdateState::UpToDate { .. }
        | UpdateState::Failed { .. } => None,
    }
}

fn path_in_state(state: &UpdateState) -> Option<PathBuf> {
    match state {
        UpdateState::Ready { path, .. }
        | UpdateState::VerifyingInstaller { path, .. }
        | UpdateState::InstallerOpened { path, .. } => Some(path.clone()),
        _ => None,
    }
}

fn human_bytes(bytes: u64) -> String {
    const MIB: f64 = 1024.0 * 1024.0;
    if bytes >= 1024 * 1024 {
        format!("{:.1} MiB", bytes as f64 / MIB)
    } else if bytes >= 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

fn download_percent(received: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        (received as f64 / total as f64 * 100.0).clamp(0.0, 100.0)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UpdateAction {
    None,
    Close,
    Later,
    Skip,
    Download,
    Retry,
    ViewRelease,
    ViewReleases,
    RevealFile,
    OpenInstaller,
    Quit,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn digest(byte: u8) -> String {
        format!("sha256:{}", format!("{byte:02x}").repeat(32))
    }

    fn digest_for(bytes: &[u8]) -> String {
        use std::fmt::Write as _;

        let digest = Sha256::digest(bytes);
        let mut output = String::from("sha256:");
        for byte in digest {
            write!(&mut output, "{byte:02x}").unwrap();
        }
        output
    }

    fn release_json(tag: &str, name: &str, size: u64, digest: Option<String>) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "tag_name": tag,
            "draft": false,
            "prerelease": false,
            "body": "## Changes\r\n\n* Faster\u{0} viewer",
            "assets": [{
                "name": name,
                "state": "uploaded",
                "size": size,
                "digest": digest,
            }]
        }))
        .unwrap()
    }

    #[test]
    fn platform_asset_contract_is_exact() {
        assert_eq!(
            platform_assets("macos", "aarch64"),
            Some(PlatformAssets {
                application: Some("viewr-macos-arm64.tar.gz"),
                installer: "viewr-macos-arm64.pkg",
                portable: "viewr-macos-arm64.tar.gz",
            })
        );
        assert_eq!(
            platform_assets("windows", "x86_64"),
            Some(PlatformAssets {
                application: None,
                installer: "viewr-windows-x64.msi",
                portable: "viewr-windows-x64.zip",
            })
        );
        assert_eq!(
            platform_assets("linux", "x86_64"),
            Some(PlatformAssets {
                application: None,
                installer: "viewr-linux-x64.deb",
                portable: "viewr-linux-x64.tar.gz",
            })
        );
        assert_eq!(platform_assets("macos", "x86_64"), None);
        assert_eq!(platform_assets("freebsd", "x86_64"), None);
    }

    #[test]
    fn macos_always_selects_the_self_updating_application_archive() {
        for native_install in [false, true] {
            assert_eq!(
                delivery_spec_for("macos", "aarch64", native_install),
                Some(("viewr-macos-arm64.tar.gz", PackageKind::Application))
            );
        }
    }

    #[test]
    fn other_platforms_preserve_installer_and_portable_delivery() {
        assert_eq!(
            delivery_spec_for("windows", "x86_64", true),
            Some(("viewr-windows-x64.msi", PackageKind::Installer))
        );
        assert_eq!(
            delivery_spec_for("windows", "x86_64", false),
            Some(("viewr-windows-x64.zip", PackageKind::Portable))
        );
        assert_eq!(
            delivery_spec_for("linux", "x86_64", true),
            Some(("viewr-linux-x64.deb", PackageKind::Installer))
        );
        assert_eq!(
            delivery_spec_for("linux", "x86_64", false),
            Some(("viewr-linux-x64.tar.gz", PackageKind::Portable))
        );
    }

    #[test]
    fn native_package_detection_is_conservative() {
        assert!(native_install_path(
            "macos",
            Path::new("/Applications/Viewr.app/Contents/MacOS/viewr-bin")
        ));
        assert!(!native_install_path(
            "macos",
            Path::new("/tmp/Viewr.app/Contents/MacOS/viewr-bin")
        ));
        assert!(native_install_path(
            "windows",
            Path::new(r"C:\Program Files\Viewr\viewr.exe")
        ));
        assert!(native_install_path(
            "windows",
            Path::new(r"\\?\C:\Program Files\Viewr\viewr.exe")
        ));
        assert!(!native_install_path(
            "windows",
            Path::new(r"C:\Downloads\Viewr\viewr.exe")
        ));
        assert!(!native_install_path(
            "windows",
            Path::new(r"C:\Downloads\Program Files\Viewr\viewr.exe")
        ));
        assert!(native_install_path("linux", Path::new("/usr/bin/viewr")));
        assert!(!native_install_path(
            "linux",
            Path::new("/home/me/bin/viewr")
        ));
    }

    #[test]
    fn semantic_version_order_is_numeric() {
        let bytes = release_json("v0.10.0", "viewr-linux-x64.deb", 42, Some(digest(1)));
        let outcome = parse_release(
            &bytes,
            &Version::parse("0.9.9").unwrap(),
            Some(("viewr-linux-x64.deb", PackageKind::Installer)),
        )
        .unwrap();
        assert!(matches!(outcome, CheckOutcome::Available(_)));

        let outcome = parse_release(
            &bytes,
            &Version::parse("0.10.0").unwrap(),
            Some(("viewr-linux-x64.deb", PackageKind::Installer)),
        )
        .unwrap();
        assert!(matches!(outcome, CheckOutcome::Current(_)));
    }

    #[test]
    fn available_release_uses_only_the_canonical_asset_contract() {
        let bytes = release_json("v0.2.0", "viewr-linux-x64.deb", 42, Some(digest(0xab)));
        let CheckOutcome::Available(release) = parse_release(
            &bytes,
            &Version::parse("0.1.0").unwrap(),
            Some(("viewr-linux-x64.deb", PackageKind::Installer)),
        )
        .unwrap() else {
            panic!("expected an available release");
        };
        let Delivery::Download(asset) = release.delivery else {
            panic!("expected a downloadable asset");
        };
        assert_eq!(asset.name, "viewr-linux-x64.deb");
        assert_eq!(
            asset.url,
            "https://github.com/hunterchen7/viewr/releases/download/v0.2.0/viewr-linux-x64.deb"
        );
        assert_eq!(asset.sha256, [0xab; 32]);
        assert_eq!(asset.kind, PackageKind::Installer);
    }

    #[test]
    fn malformed_or_nonstable_tags_are_rejected() {
        for tag in [
            "0.2.0",
            "v0.2",
            "v0.2.0-alpha.1",
            "v0.2.0+build",
            "v00.2.0",
            "v18446744073709551616.0.0",
        ] {
            let bytes = release_json(tag, "viewr-linux-x64.deb", 42, Some(digest(1)));
            assert!(
                parse_release(
                    &bytes,
                    &Version::parse("0.1.0").unwrap(),
                    Some(("viewr-linux-x64.deb", PackageKind::Installer)),
                )
                .is_err(),
                "accepted {tag}"
            );
        }
    }

    #[test]
    fn drafts_and_prereleases_are_rejected_independently() {
        for field in ["draft", "prerelease"] {
            let mut value: serde_json::Value = serde_json::from_slice(&release_json(
                "v0.2.0",
                "viewr-linux-x64.deb",
                42,
                Some(digest(1)),
            ))
            .unwrap();
            value[field] = true.into();
            let bytes = serde_json::to_vec(&value).unwrap();
            assert!(
                parse_release(
                    &bytes,
                    &Version::parse("0.1.0").unwrap(),
                    Some(("viewr-linux-x64.deb", PackageKind::Installer)),
                )
                .is_err()
            );
        }
    }

    #[test]
    fn asset_must_be_unique_uploaded_bounded_and_hashed() {
        let running = Version::parse("0.1.0").unwrap();
        let expected = Some(("viewr-linux-x64.deb", PackageKind::Installer));

        let missing = release_json("v0.2.0", "wrong.deb", 42, Some(digest(1)));
        let CheckOutcome::Available(release) = parse_release(&missing, &running, expected).unwrap()
        else {
            panic!("expected an available release");
        };
        assert!(matches!(release.delivery, Delivery::ReleasePageOnly { .. }));

        for (size, digest_value) in [
            (0, Some(digest(1))),
            (MAX_PACKAGE_BYTES + 1, Some(digest(1))),
            (42, Some("sha256:short".into())),
        ] {
            let bytes = release_json("v0.2.0", "viewr-linux-x64.deb", size, digest_value);
            assert!(parse_release(&bytes, &running, expected).is_err());
        }

        let mut duplicate: serde_json::Value = serde_json::from_slice(&release_json(
            "v0.2.0",
            "viewr-linux-x64.deb",
            42,
            Some(digest(1)),
        ))
        .unwrap();
        let duplicate_asset = duplicate["assets"][0].clone();
        duplicate["assets"]
            .as_array_mut()
            .unwrap()
            .push(duplicate_asset);
        assert!(
            parse_release(&serde_json::to_vec(&duplicate).unwrap(), &running, expected,).is_err()
        );
    }

    #[test]
    fn notes_are_bounded_plain_text_and_controls_are_replaced() {
        let notes = sanitized_notes(Some("line 1\r\nline\u{0} 2\tok"));
        assert_eq!(notes, "line 1\nline� 2\tok");
        assert_eq!(
            sanitized_notes(Some(" \n\t ")),
            "See the release page for details."
        );
        assert!(
            sanitized_notes(Some(&"x".repeat(MAX_RELEASE_NOTES_BYTES + 10))).len()
                <= MAX_RELEASE_NOTES_BYTES
        );
    }

    #[test]
    fn release_notes_parse_release_please_markdown_without_exposing_urls() {
        let notes = "## [0.4.0](https://github.com/hunterchen7/viewr/compare/v0.3.0...v0.4.0) (2026-08-02)\n\n### Features\n\n* add an information strip ([#24](https://github.com/hunterchen7/viewr/pull/24))\n* fill the cache adaptively ([#26](https://github.com/hunterchen7/viewr/pull/26))\n\n### Bug Fixes\n\n* keep overflow draggable ([650b8f6](https://github.com/hunterchen7/viewr/commit/650b8f6))";

        let blocks = parse_release_notes(notes);

        assert_eq!(blocks.len(), 6);
        assert!(matches!(
            &blocks[0],
            ReleaseNoteBlock::Heading { level: 2, spans }
                if spans == &vec![
                    ReleaseNoteSpan::Link {
                        label: "0.4.0".into(),
                        url: "https://github.com/hunterchen7/viewr/compare/v0.3.0...v0.4.0".into(),
                    },
                    ReleaseNoteSpan::Text(" (2026-08-02)".into()),
                ]
        ));
        assert!(matches!(
            &blocks[1],
            ReleaseNoteBlock::Heading { level: 3, spans }
                if spans == &vec![ReleaseNoteSpan::Text("Features".into())]
        ));
        assert!(matches!(&blocks[2], ReleaseNoteBlock::Bullet(_)));
        assert!(matches!(&blocks[3], ReleaseNoteBlock::Bullet(_)));
        assert!(matches!(
            &blocks[4],
            ReleaseNoteBlock::Heading { level: 3, spans }
                if spans == &vec![ReleaseNoteSpan::Text("Bug Fixes".into())]
        ));
        assert!(matches!(&blocks[5], ReleaseNoteBlock::Bullet(_)));

        let visible = blocks
            .iter()
            .flat_map(ReleaseNoteBlock::spans)
            .map(ReleaseNoteSpan::label)
            .collect::<String>();
        assert!(visible.contains("0.4.0 (2026-08-02)"));
        assert!(visible.contains("add an information strip (#24)"));
        assert!(!visible.contains("https://"));
        assert!(!visible.contains("##"));
        assert!(!visible.contains("* "));
    }

    #[test]
    fn release_notes_keep_malformed_or_untrusted_links_as_plain_text() {
        let blocks = parse_release_notes(
            "A [broken](not a url), [unsafe](javascript:alert(1)), and [external](https://example.com/path) link.\n\nPlain paragraph.",
        );

        assert_eq!(blocks.len(), 2);
        assert!(blocks.iter().all(|block| {
            block
                .spans()
                .all(|span| matches!(span, ReleaseNoteSpan::Text(_)))
        }));
        let visible = blocks
            .iter()
            .flat_map(ReleaseNoteBlock::spans)
            .map(ReleaseNoteSpan::label)
            .collect::<String>();
        assert_eq!(
            visible,
            "A [broken](not a url), [unsafe](javascript:alert(1)), and [external](https://example.com/path) link.Plain paragraph."
        );
    }

    #[test]
    fn release_notes_render_release_please_strong_scopes_without_markers() {
        let blocks = parse_release_notes(
            "* **ci:** keep update archives verified ([#42](https://github.com/hunterchen7/viewr/pull/42))",
        );

        assert!(matches!(
            &blocks[0],
            ReleaseNoteBlock::Bullet(spans)
                if spans == &vec![
                    ReleaseNoteSpan::Strong("ci:".into()),
                    ReleaseNoteSpan::Text(" keep update archives verified (".into()),
                    ReleaseNoteSpan::Link {
                        label: "#42".into(),
                        url: "https://github.com/hunterchen7/viewr/pull/42".into(),
                    },
                    ReleaseNoteSpan::Text(")".into()),
                ]
        ));
    }

    #[test]
    fn release_note_layout_breaks_long_tokens_inside_the_available_width() {
        let ctx = egui::Context::default();
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(320.0, 1200.0));
        let input = egui::RawInput {
            screen_rect: Some(screen),
            ..Default::default()
        };
        let notes = format!(
            "### Fixes\n\n* {} ([very-long-link-label-{}](https://github.com/hunterchen7/viewr/pull/42))",
            "unbroken".repeat(128),
            "x".repeat(128),
        );
        let mut rendered = egui::Rect::NOTHING;
        let blocks = parse_release_notes(&notes);

        let _ = ctx.run_ui(input, |ui| {
            ui.set_max_width(300.0);
            rendered = ui.scope(|ui| show_release_notes(ui, &blocks)).response.rect;
        });

        assert!(rendered.is_finite());
        assert!(rendered.right() <= 300.5, "rendered rect was {rendered:?}");
        assert!(rendered.height() > 40.0, "long tokens did not wrap");
    }

    #[test]
    fn release_note_parser_bounds_adversarial_brackets_lines_and_spans() {
        let malformed = format!("{}]", "[".repeat(MAX_RELEASE_NOTES_BYTES - 1));
        let spans = parse_release_note_spans(&malformed);
        assert!(spans.len() <= MAX_RELEASE_NOTE_SPANS);
        let emphasized = parse_release_note_spans(&"**x** ".repeat(MAX_RELEASE_NOTE_SPANS * 100));
        assert!(emphasized.len() <= MAX_RELEASE_NOTE_SPANS);

        let many_lines = "line\n".repeat(MAX_RELEASE_NOTE_BLOCKS * 100);
        let blocks = parse_release_notes(&many_lines);
        assert_eq!(blocks.len(), MAX_RELEASE_NOTE_BLOCKS);
        assert!(matches!(
            blocks.last(),
            Some(ReleaseNoteBlock::Paragraph(spans))
                if spans.iter().any(|span| span.label().contains("release page"))
        ));
    }

    #[test]
    fn release_note_wrapping_prefers_word_boundaries() {
        let ctx = egui::Context::default();
        let _ = ctx.run_ui(Default::default(), |ui| {
            let job = release_note_job(
                ui,
                "ordinary words should wrap at spaces",
                ReleaseNoteStyle::Body,
                false,
            );
            assert!(!job.wrap.break_anywhere);
        });
    }

    #[test]
    fn request_urls_reject_downgrades_credentials_and_unknown_hosts() {
        for url in [
            "http://github.com/file",
            "https://user@github.com/file",
            "https://example.com/file",
        ] {
            assert!(
                validate_request_url(&Url::parse(url).unwrap(), RequestPurpose::ReleaseAsset,)
                    .is_err(),
                "accepted {url}"
            );
        }
        assert!(
            validate_request_url(
                &Url::parse("https://release-assets.githubusercontent.com/file").unwrap(),
                RequestPurpose::ReleaseAsset,
            )
            .is_ok()
        );
    }

    #[test]
    fn redirects_remain_https_and_inside_the_purpose_allowlist() {
        let asset_url =
            Url::parse("https://github.com/hunterchen7/viewr/releases/download/v0.2.0/update.pkg")
                .unwrap();
        assert_eq!(
            redirect_url(
                &asset_url,
                Some("https://release-assets.githubusercontent.com/token"),
                0,
                RequestPurpose::ReleaseAsset,
            )
            .unwrap()
            .host_str(),
            Some("release-assets.githubusercontent.com")
        );
        assert!(
            redirect_url(
                &asset_url,
                Some("http://release-assets.githubusercontent.com/token"),
                0,
                RequestPurpose::ReleaseAsset,
            )
            .is_err()
        );
        assert!(
            redirect_url(
                &asset_url,
                Some("https://example.com/token"),
                0,
                RequestPurpose::ReleaseAsset,
            )
            .is_err()
        );
        assert!(redirect_url(&asset_url, None, 0, RequestPurpose::ReleaseAsset).is_err());
        assert!(
            redirect_url(
                &asset_url,
                Some("/another"),
                MAX_REDIRECTS,
                RequestPurpose::ReleaseAsset,
            )
            .is_err()
        );

        let api_url = Url::parse(RELEASE_API_URL).unwrap();
        assert!(
            redirect_url(
                &api_url,
                Some("https://github.com/hunterchen7/viewr/releases/latest"),
                0,
                RequestPurpose::ReleaseApi,
            )
            .is_err()
        );
    }

    #[test]
    fn bounded_reader_rejects_one_extra_byte() {
        assert_eq!(read_bounded(&b"abcd"[..], 4).unwrap(), b"abcd");
        assert!(read_bounded(&b"abcde"[..], 4).is_err());
    }

    #[test]
    #[ignore = "requires the public GitHub API"]
    fn live_release_endpoint_satisfies_the_update_contract() {
        check_latest_release().unwrap();
    }

    #[test]
    fn state_skip_is_exact_and_throttles_are_clock_safe() {
        let directory = tempfile::tempdir().unwrap();
        let store = UpdateStore::new(
            directory.path().join("config"),
            directory.path().join("cache"),
        )
        .unwrap();
        let skipped = Version::parse("0.2.0").unwrap();
        store.skip(&skipped).unwrap();
        assert_eq!(store.skipped_version().unwrap(), Some(skipped.clone()));

        store.initialize(&Version::parse("0.1.9").unwrap()).unwrap();
        assert_eq!(store.skipped_version().unwrap(), Some(skipped.clone()));
        store.initialize(&skipped).unwrap();
        assert_eq!(store.skipped_version().unwrap(), None);

        assert!(check_due(None, 100, 10));
        assert!(!check_due(Some(95), 100, 10));
        assert!(check_due(Some(90), 100, 10));
        assert!(check_due(Some(101), 100, 10));
    }

    #[test]
    fn check_claim_is_cross_process_exclusive_and_persistently_throttled() {
        let directory = tempfile::tempdir().unwrap();
        let store = UpdateStore::new(
            directory.path().join("config"),
            directory.path().join("cache"),
        )
        .unwrap();

        let first = store
            .claim_check(CheckSource::Automatic, 100)
            .unwrap()
            .unwrap();
        assert!(matches!(
            store.claim_check(CheckSource::Manual, 100),
            Err(UpdateError::Busy(_))
        ));
        drop(first);

        assert!(
            store
                .claim_check(CheckSource::Automatic, 100)
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .claim_check(CheckSource::Manual, 100)
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .claim_check(CheckSource::Manual, 100 + MANUAL_CHECK_INTERVAL_SECS,)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn automatic_opt_out_is_authoritative_across_existing_processes() {
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("config");
        let cache = directory.path().join("cache");
        let first = UpdateStore::new(config.clone(), cache.clone()).unwrap();
        let sibling = UpdateStore::new(config, cache).unwrap();

        assert!(first.automatic_checks_enabled().unwrap());
        first.set_automatic_checks_enabled(false).unwrap();
        assert!(!sibling.automatic_checks_enabled().unwrap());
        assert!(
            sibling
                .claim_check(CheckSource::Automatic, 100)
                .unwrap()
                .is_none()
        );
        assert!(
            sibling
                .claim_check(CheckSource::Manual, 100)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn corrupt_existing_update_state_fails_closed_for_automatic_network_access() {
        let cases = [
            b"automatic_checks_enabled = maybe".to_vec(),
            vec![b'x'; MAX_UPDATE_STATE_BYTES as usize + 1],
        ];
        for contents in cases {
            let directory = tempfile::tempdir().unwrap();
            let store = UpdateStore::new(
                directory.path().join("config"),
                directory.path().join("cache"),
            )
            .unwrap();
            std::fs::write(&store.state_path, &contents).unwrap();

            assert!(store.automatic_checks_enabled().is_err());
            let mut manager = UpdateManager::with_store(egui::Context::default(), Ok(store));
            assert!(!manager.automatic_checks_enabled());
            assert!(manager.automatic_due_at.is_none());
            manager.set_automatic_checks(true);
            assert!(manager.automatic_checks_enabled());
            assert!(manager.store_error.is_none());
        }
    }

    #[test]
    fn manager_schedules_and_cancels_automatic_checks_with_the_persisted_setting() {
        let directory = tempfile::tempdir().unwrap();
        let store = UpdateStore::new(
            directory.path().join("config"),
            directory.path().join("cache"),
        )
        .unwrap();
        let mut manager = UpdateManager::with_store(egui::Context::default(), Ok(store.clone()));
        let mut sibling = UpdateManager::with_store(egui::Context::default(), Ok(store.clone()));
        assert!(manager.automatic_checks_enabled());
        assert!(sibling.automatic_checks_enabled());
        assert!(manager.automatic_due_at.is_some());

        manager.set_automatic_checks(false);
        assert!(!manager.automatic_checks_enabled());
        assert!(manager.automatic_due_at.is_none());

        sibling.preference_refresh_due_at = Instant::now();
        sibling.refresh_automatic_preference();
        assert!(!sibling.automatic_checks_enabled());
        assert!(sibling.automatic_due_at.is_none());

        let restarted = UpdateManager::with_store(egui::Context::default(), Ok(store));
        assert!(!restarted.automatic_checks_enabled());
        assert!(restarted.automatic_due_at.is_none());
    }

    #[test]
    fn manual_check_promotes_an_in_flight_automatic_check() {
        let directory = tempfile::tempdir().unwrap();
        let store = UpdateStore::new(
            directory.path().join("config"),
            directory.path().join("cache"),
        )
        .unwrap();
        let mut manager = UpdateManager::with_store(egui::Context::default(), Ok(store));
        manager.state = UpdateState::Checking {
            id: 9,
            source: CheckSource::Automatic,
        };

        manager.check_now();

        assert!(manager.dialog_open);
        assert!(matches!(
            manager.state,
            UpdateState::Checking {
                id: 9,
                source: CheckSource::Manual
            }
        ));
    }

    #[test]
    fn promoted_manual_check_retries_when_the_automatic_claim_was_suppressed() {
        let directory = tempfile::tempdir().unwrap();
        let store = UpdateStore::new(
            directory.path().join("config"),
            directory.path().join("cache"),
        )
        .unwrap();
        drop(
            store
                .claim_check(CheckSource::Manual, unix_time())
                .unwrap()
                .unwrap(),
        );
        let mut manager = UpdateManager::with_store(egui::Context::default(), Ok(store));
        manager.state = UpdateState::Checking {
            id: 9,
            source: CheckSource::Manual,
        };

        manager.handle_event(WorkerEvent::CheckFinished {
            id: 9,
            claimed_source: CheckSource::Automatic,
            result: Ok(None),
        });

        assert!(matches!(
            manager.state,
            UpdateState::Checking {
                id,
                source: CheckSource::Manual
            } if id != 9
        ));
    }

    #[test]
    fn installer_and_reveal_commands_preserve_paths_as_single_arguments() {
        let path = Path::new("/tmp/update with spaces;$(nope).pkg");
        let mac = installer_command_for("macos", path, None).unwrap();
        assert_eq!(mac.program, OsString::from("/usr/bin/open"));
        assert_eq!(mac.arguments, vec![path.as_os_str().to_owned()]);

        let system = Path::new(r"C:\Windows\System32");
        let windows =
            installer_command_for("windows", Path::new(r"C:\update a.msi"), Some(system)).unwrap();
        assert_eq!(windows.program, system.join("msiexec.exe").into_os_string());
        assert_eq!(
            windows.arguments,
            vec![OsString::from("/i"), OsString::from(r"C:\update a.msi")]
        );

        let windows = reveal_command_for(
            "windows",
            Path::new(r"C:\update a.msi"),
            Some(Path::new(r"C:\Windows")),
        )
        .unwrap();
        assert_eq!(
            windows.program,
            Path::new(r"C:\Windows")
                .join("explorer.exe")
                .into_os_string()
        );

        let linux =
            reveal_command_for("linux", Path::new("/tmp/version/update.deb"), None).unwrap();
        assert_eq!(linux.program, OsString::from("/usr/bin/xdg-open"));
        assert_eq!(linux.arguments, vec![OsString::from("/tmp/version")]);
        assert!(installer_command_for("freebsd", path, None).is_none());
        assert!(installer_command_for("windows", path, None).is_none());
    }

    #[test]
    fn provenance_records_the_download_source_without_shell_interpolation() {
        let url = "https://github.com/hunterchen7/viewr/releases/download/v0.2.0/viewr.pkg?x=1&y=2";
        let event_id = "550E8400-E29B-41D4-A716-446655440000";
        assert_eq!(
            quarantine_value(0x1234, event_id),
            format!("0083;1234;Viewr;{event_id}")
        );
        assert!(valid_quarantine_event_id(event_id));
        assert!(!valid_quarantine_event_id(url));
        assert_eq!(
            zone_identifier(url),
            format!("[ZoneTransfer]\r\nZoneId=3\r\nHostUrl={url}\r\n")
        );
    }

    #[test]
    fn streamed_download_publishes_only_exact_verified_content() {
        let directory = tempfile::tempdir().unwrap();
        let store = UpdateStore::new(
            directory.path().join("config"),
            directory.path().join("cache"),
        )
        .unwrap();
        let bytes = b"verified update";
        let asset = ReleaseAsset {
            name: "viewr-linux-x64.deb",
            url:
                "https://github.com/hunterchen7/viewr/releases/download/v0.2.0/viewr-linux-x64.deb"
                    .into(),
            size: bytes.len() as u64,
            sha256: Sha256::digest(bytes).into(),
            kind: PackageKind::Installer,
        };
        let release = Release {
            version: Version::parse("0.2.0").unwrap(),
            notes: Arc::from([]),
            delivery: Delivery::Download(asset.clone()),
        };
        let destination = store.destination(&release, &asset);
        ensure_private_directory(destination.parent().unwrap()).unwrap();

        let progress = AtomicU64::new(0);
        persist_download_stream(&bytes[..], &destination, &asset, &progress).unwrap();
        assert_eq!(std::fs::read(&destination).unwrap(), bytes);
        assert_eq!(progress.load(Ordering::Relaxed), bytes.len() as u64);
        verify_file(&destination, &asset).unwrap();

        for (name, candidate) in [
            ("short.deb", &bytes[..bytes.len() - 1]),
            ("long.deb", b"verified update!" as &[u8]),
            ("wrong-hash.deb", b"VERIFIED UPDATE" as &[u8]),
        ] {
            let failed_destination = destination.with_file_name(name);
            assert!(
                persist_download_stream(
                    candidate,
                    &failed_destination,
                    &asset,
                    &AtomicU64::new(0),
                )
                .is_err(),
                "accepted {name}"
            );
            assert!(
                !failed_destination.exists(),
                "{name} left a launchable file"
            );
        }
    }

    #[test]
    fn local_release_to_verified_handoff_flow_is_end_to_end() {
        let bytes = b"a complete release package";
        let platform = platform_assets(std::env::consts::OS, std::env::consts::ARCH)
            .expect("CI runs on a supported release target");
        let json = release_json(
            "v9.8.7",
            platform.portable,
            bytes.len() as u64,
            Some(digest_for(bytes)),
        );
        let CheckOutcome::Available(release) = parse_release(
            &json,
            &Version::parse("1.0.0").unwrap(),
            Some((platform.portable, PackageKind::Portable)),
        )
        .unwrap() else {
            panic!("expected a newer release");
        };
        let Delivery::Download(asset) = &release.delivery else {
            panic!("expected the exact platform asset");
        };
        let directory = tempfile::tempdir().unwrap();
        let store = UpdateStore::new(
            directory.path().join("config"),
            directory.path().join("cache"),
        )
        .unwrap();
        let destination = store.destination(&release, asset);
        ensure_private_directory(destination.parent().unwrap()).unwrap();

        persist_download_stream(&bytes[..], &destination, asset, &AtomicU64::new(0)).unwrap();

        verify_before_open(&destination, asset).unwrap();
        assert_eq!(std::fs::read(destination).unwrap(), bytes);
    }

    #[test]
    fn pre_open_verification_rejects_same_size_tampering() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("viewr-update.pkg");
        let original = b"verified update";
        let asset = ReleaseAsset {
            name: "viewr-macos-arm64.pkg",
            url:
                "https://github.com/hunterchen7/viewr/releases/download/v0.2.0/viewr-macos-arm64.pkg"
                    .into(),
            size: original.len() as u64,
            sha256: Sha256::digest(original).into(),
            kind: PackageKind::Installer,
        };
        std::fs::write(&path, original).unwrap();
        apply_download_provenance(&path, &asset).unwrap();
        std::fs::write(&path, b"tampered update").unwrap();

        assert!(verify_before_open(&path, &asset).is_err());
    }

    #[test]
    fn stale_installer_verification_cannot_open_an_installer() {
        let directory = tempfile::tempdir().unwrap();
        let store = UpdateStore::new(
            directory.path().join("config"),
            directory.path().join("cache"),
        )
        .unwrap();
        let mut manager = UpdateManager::with_store(egui::Context::default(), Ok(store));
        let asset = ReleaseAsset {
            name: "viewr-macos-arm64.pkg",
            url:
                "https://github.com/hunterchen7/viewr/releases/download/v0.2.0/viewr-macos-arm64.pkg"
                    .into(),
            size: 1,
            sha256: [0; 32],
            kind: PackageKind::Installer,
        };
        let release = Release {
            version: Version::parse("0.2.0").unwrap(),
            notes: Arc::from([]),
            delivery: Delivery::Download(asset.clone()),
        };
        manager.state = UpdateState::VerifyingInstaller {
            id: 7,
            release: release.clone(),
            path: PathBuf::from("/tmp/update.pkg"),
        };
        manager.handle_event(WorkerEvent::InstallerVerified {
            id: 6,
            release,
            asset,
            path: PathBuf::from("/tmp/update.pkg"),
            result: Ok(()),
        });
        assert!(matches!(
            manager.state,
            UpdateState::VerifyingInstaller { id: 7, .. }
        ));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn stale_application_preparation_cannot_disrupt_the_active_update() {
        let directory = tempfile::tempdir().unwrap();
        let store = UpdateStore::new(
            directory.path().join("config"),
            directory.path().join("cache"),
        )
        .unwrap();
        let mut manager = UpdateManager::with_store(egui::Context::default(), Ok(store));
        let asset = ReleaseAsset {
            name: "viewr-macos-arm64.tar.gz",
            url: "https://github.com/hunterchen7/viewr/releases/download/v0.2.0/viewr-macos-arm64.tar.gz"
                .into(),
            size: 1,
            sha256: [0; 32],
            kind: PackageKind::Application,
        };
        let release = Release {
            version: Version::parse("0.2.0").unwrap(),
            notes: Arc::from([]),
            delivery: Delivery::Download(asset.clone()),
        };
        manager.state = UpdateState::PreparingApplication {
            id: 7,
            release: release.clone(),
        };
        manager.handle_event(WorkerEvent::ApplicationPrepared {
            id: 6,
            release,
            asset,
            result: Err(UpdateError::InvalidRelease("stale failure".into())),
        });
        assert!(matches!(
            manager.state,
            UpdateState::PreparingApplication { id: 7, .. }
        ));
    }

    #[test]
    fn cache_pruning_removes_orphan_temps_and_obsolete_versions() {
        let directory = tempfile::tempdir().unwrap();
        let downloads = directory.path().join("downloads");
        let keep = downloads.join("0.3.0");
        let obsolete = downloads.join("0.2.0");
        std::fs::create_dir_all(&keep).unwrap();
        std::fs::create_dir_all(&obsolete).unwrap();
        std::fs::write(keep.join(".viewr-download-one.tmp"), b"partial").unwrap();
        std::fs::write(keep.join("viewr.pkg"), b"current").unwrap();
        std::fs::write(obsolete.join("viewr.pkg"), b"old").unwrap();

        prune_update_cache(
            &downloads,
            &keep,
            SystemTime::now() + Duration::from_secs(1),
        )
        .unwrap();

        assert!(keep.join("viewr.pkg").is_file());
        assert!(!keep.join(".viewr-download-one.tmp").exists());
        assert!(!obsolete.exists());
    }

    #[cfg(unix)]
    #[test]
    fn update_state_refuses_symbolic_link_targets() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let store = UpdateStore::new(
            directory.path().join("config"),
            directory.path().join("cache"),
        )
        .unwrap();
        let target = directory.path().join("outside");
        std::fs::write(&target, b"untouched").unwrap();
        symlink(&target, &store.state_path).unwrap();

        assert!(store.skip(&Version::parse("0.2.0").unwrap()).is_err());
        assert_eq!(std::fs::read(target).unwrap(), b"untouched");
    }

    #[cfg(unix)]
    #[test]
    fn updater_storage_is_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir().unwrap();
        let store = UpdateStore::new(
            directory.path().join("config"),
            directory.path().join("cache"),
        )
        .unwrap();
        store.skip(&Version::parse("0.2.0").unwrap()).unwrap();
        let check_lock = store
            .claim_check(CheckSource::Manual, 100)
            .unwrap()
            .unwrap();

        assert_eq!(
            store
                .state_path
                .parent()
                .unwrap()
                .metadata()
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            store.state_path.metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            check_lock.metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn progress_counter_type_is_lock_free_enough_for_worker_updates() {
        let progress = Arc::new(AtomicU64::new(0));
        progress.store(42, Ordering::Relaxed);
        assert_eq!(progress.load(Ordering::Relaxed), 42);
    }
}
