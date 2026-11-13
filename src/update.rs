//! `truce update` installs a GitHub Release of this binary.
//!
//! The current version is the crate version embedded at build time, which is
//! the same string `truce --version` prints. With no `--version`, the command
//! reads `GET /repos/AJFrio/Truce/releases/latest`. `--version` loads that tag
//! instead.
//!
//! When this binary is already the requested version, the command prints a
//! message and does not download. Otherwise it downloads the archive for this
//! OS and CPU plus `SHA256SUMS`, checks the archive, extracts `truce`, and
//! replaces the running executable. The checksum is verified before extraction
//! and before the binary is replaced.
//!
//! `TRUCE_UPDATE_API` overrides the GitHub API base (tests and proxies).
//! `TRUCE_UPDATE_TARGET`, when set, is the file to replace instead of the
//! running binary.

use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use flate2::read::GzDecoder;
use semver::Version;
use serde::Deserialize;

use crate::cli::UpdateArgs;
use crate::error::{Result, TruceError};
use crate::output::ignore_broken_pipe;

const GITHUB_REPO: &str = "AJFrio/Truce";
const DEFAULT_API: &str = "https://api.github.com";
const MAX_API_BYTES: usize = 2 * 1024 * 1024;
const MAX_SUMS_BYTES: usize = 1024 * 1024;
const MAX_ARCHIVE_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArchiveKind {
    TarGz,
    Zip,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PlatformAsset {
    archive: String,
    binary: String,
    kind: ArchiveKind,
}

#[derive(Debug, PartialEq, Eq)]
enum UpdatePlan {
    UpToDate,
    Available,
    Install,
}

struct Request {
    check: bool,
    version: Option<String>,
    api: String,
    /// File to replace. `None` means the running executable, resolved only
    /// when an install is actually going to happen.
    install_path: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
struct GhRelease {
    tag_name: String,
    assets: Vec<GhAsset>,
}

#[derive(Debug, Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
}

/// Run `truce update`. Prints one status line and returns the process exit code.
pub(crate) async fn run(args: UpdateArgs) -> Result<i32> {
    let message = execute(&Request::from_cli(&args)).await?;
    let mut stdout = io::stdout().lock();
    writeln!(stdout, "{message}").or_else(ignore_broken_pipe)?;
    Ok(0)
}

impl Request {
    fn from_cli(args: &UpdateArgs) -> Self {
        let install_path = std::env::var("TRUCE_UPDATE_TARGET")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        Self {
            check: args.check,
            version: args.version.clone(),
            api: api_base_from_env(),
            install_path,
        }
    }
}

fn api_base_from_env() -> String {
    std::env::var("TRUCE_UPDATE_API")
        .ok()
        .map(|value| value.trim().trim_end_matches('/').to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_API.to_string())
}

async fn execute(req: &Request) -> Result<String> {
    let current = current_version()?;
    let client = http_client()?;
    let release = fetch_release(&client, &req.api, req.version.as_deref()).await?;
    let target = parse_version(&release.tag_name)?;
    match plan(&current, &target, req.version.is_some(), req.check) {
        UpdatePlan::UpToDate => Ok(up_to_date_message(&current, &target, &release.tag_name)),
        UpdatePlan::Available => Ok(available_message(&current, &release.tag_name)),
        UpdatePlan::Install => {
            let dest = match &req.install_path {
                Some(path) => path.clone(),
                None => current_exe()?,
            };
            let asset = asset_for(std::env::consts::OS, std::env::consts::ARCH)?;
            install(&client, &release, &asset, &dest).await?;
            Ok(updated_message(&current, &release.tag_name, &dest))
        }
    }
}

fn current_version() -> Result<Version> {
    parse_version(env!("CARGO_PKG_VERSION"))
}

fn current_exe() -> Result<PathBuf> {
    std::env::current_exe()
        .map_err(|err| TruceError::Update(format!("cannot locate the running truce binary: {err}")))
}

fn up_to_date_message(current: &Version, target: &Version, tag: &str) -> String {
    if current == target {
        format!("truce {current} is up to date")
    } else {
        format!("truce {current} is up to date (latest release is {tag})")
    }
}

fn available_message(current: &Version, tag: &str) -> String {
    format!("truce {current} is installed; {tag} is available")
}

fn updated_message(current: &Version, tag: &str, dest: &Path) -> String {
    format!("updated truce from {current} to {tag} ({})", dest.display())
}

/// Compare the installed version with the release we are considering.
///
/// An implicit "latest" request never downgrades. `--version` installs that
/// tag even when it is older, unless `--check` was also passed.
fn plan(current: &Version, target: &Version, explicit: bool, check_only: bool) -> UpdatePlan {
    let same = current == target;
    if same || (!explicit && current >= target) {
        UpdatePlan::UpToDate
    } else if check_only {
        UpdatePlan::Available
    } else {
        UpdatePlan::Install
    }
}

/// Parse a release tag or a crate version. A single leading `v` or `V` is ignored.
fn parse_version(raw: &str) -> Result<Version> {
    let trimmed = raw.trim();
    let stripped = trimmed
        .strip_prefix('v')
        .or_else(|| trimmed.strip_prefix('V'))
        .unwrap_or(trimmed);
    Version::parse(stripped)
        .map_err(|err| TruceError::Update(format!("{trimmed} is not a release version ({err})")))
}

fn normalize_tag(input: &str) -> Result<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(TruceError::Update("version tag is empty".into()));
    }
    if trimmed.contains(['/', '\\', '?', '#', ' ', '%']) {
        return Err(TruceError::Update(format!(
            "{trimmed} is not a release tag"
        )));
    }
    if let Ok(version) = parse_version(trimmed) {
        return Ok(format!("v{version}"));
    }
    Ok(trimmed.to_string())
}

fn release_url(api: &str, version: Option<&str>) -> Result<String> {
    let api = api.trim_end_matches('/');
    match version {
        None => Ok(format!("{api}/repos/{GITHUB_REPO}/releases/latest")),
        Some(tag) => {
            let tag = normalize_tag(tag)?;
            Ok(format!("{api}/repos/{GITHUB_REPO}/releases/tags/{tag}"))
        }
    }
}

fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .use_rustls_tls()
        .user_agent(format!(
            "truce/{} (https://github.com/{GITHUB_REPO})",
            env!("CARGO_PKG_VERSION")
        ))
        .redirect(reqwest::redirect::Policy::limited(10))
        .timeout(Duration::from_secs(120))
        .build()
        .map_err(|err| TruceError::Update(format!("failed to build HTTP client: {err}")))
}

async fn fetch_release(
    client: &reqwest::Client,
    api: &str,
    version: Option<&str>,
) -> Result<GhRelease> {
    let url = release_url(api, version)?;
    let response = client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .await
        .map_err(|err| TruceError::Update(format!("failed to reach GitHub releases: {err}")))?;
    let status = response.status();
    if let Some(len) = response.content_length() {
        if len > MAX_API_BYTES as u64 {
            return Err(TruceError::Update(format!(
                "GitHub release response is {len} bytes, over the {MAX_API_BYTES} byte limit"
            )));
        }
    }
    let body = response.text().await.map_err(|err| {
        TruceError::Update(format!("failed to read GitHub releases response: {err}"))
    })?;
    if body.len() > MAX_API_BYTES {
        return Err(TruceError::Update(format!(
            "GitHub release response is {} bytes, over the {MAX_API_BYTES} byte limit",
            body.len()
        )));
    }
    if status == reqwest::StatusCode::NOT_FOUND {
        return Err(match version {
            Some(tag) => {
                let shown = normalize_tag(tag).unwrap_or_else(|_| tag.to_string());
                TruceError::Update(format!("release {shown} was not found on {GITHUB_REPO}"))
            }
            None => TruceError::Update(format!("no published release on {GITHUB_REPO}")),
        });
    }
    if !status.is_success() {
        return Err(TruceError::Update(format!(
            "GitHub releases request failed (HTTP {status}){}",
            one_line(&body)
        )));
    }
    serde_json::from_str(&body).map_err(|err| {
        TruceError::Update(format!("GitHub release response was not understood: {err}"))
    })
}

fn one_line(body: &str) -> String {
    let trimmed = body.trim().replace(['\n', '\r'], " ");
    if trimmed.is_empty() {
        String::new()
    } else {
        let short: String = trimmed.chars().take(180).collect();
        format!(": {short}")
    }
}

fn asset_for(os: &str, arch: &str) -> Result<PlatformAsset> {
    let arch = match arch {
        "x86_64" | "amd64" => "x86_64",
        "aarch64" | "arm64" => "aarch64",
        other => other,
    };
    let (target, kind, binary) = match (os, arch) {
        ("linux", "x86_64") => ("x86_64-unknown-linux-gnu", ArchiveKind::TarGz, "truce"),
        ("linux", "aarch64") => ("aarch64-unknown-linux-gnu", ArchiveKind::TarGz, "truce"),
        ("macos" | "darwin", "aarch64") => ("aarch64-apple-darwin", ArchiveKind::TarGz, "truce"),
        ("macos" | "darwin", "x86_64") => ("x86_64-apple-darwin", ArchiveKind::TarGz, "truce"),
        ("windows", "x86_64") => ("x86_64-pc-windows-msvc", ArchiveKind::Zip, "truce.exe"),
        _ => {
            return Err(TruceError::Update(format!(
                "unsupported platform {os}/{arch}; release assets cover linux x64, linux arm64, \
macOS arm64, macOS x64, and windows x64"
            )));
        }
    };
    let ext = match kind {
        ArchiveKind::TarGz => "tar.gz",
        ArchiveKind::Zip => "zip",
    };
    Ok(PlatformAsset {
        archive: format!("truce-{target}.{ext}"),
        binary: binary.to_string(),
        kind,
    })
}

async fn install(
    client: &reqwest::Client,
    release: &GhRelease,
    asset: &PlatformAsset,
    dest: &Path,
) -> Result<()> {
    let archive_meta = find_asset(release, &asset.archive)?;
    let sums_meta = find_asset(release, "SHA256SUMS")?;
    eprintln!("downloading {}", asset.archive);
    let sums = download(client, &sums_meta.browser_download_url, MAX_SUMS_BYTES).await?;
    let sums_text = String::from_utf8(sums)
        .map_err(|_| TruceError::Update("SHA256SUMS is not UTF-8".into()))?;
    let archive = download(
        client,
        &archive_meta.browser_download_url,
        MAX_ARCHIVE_BYTES,
    )
    .await?;
    eprintln!("verifying SHA-256");
    let binary = verified_binary(&archive, &sums_text, asset)?;
    eprintln!("replacing {}", dest.display());
    replace_executable(dest, &binary)
}

fn find_asset<'a>(release: &'a GhRelease, name: &str) -> Result<&'a GhAsset> {
    if let Some(asset) = release.assets.iter().find(|asset| asset.name == name) {
        return Ok(asset);
    }
    if name.eq_ignore_ascii_case("SHA256SUMS") {
        if let Some(asset) = release
            .assets
            .iter()
            .find(|asset| asset.name.eq_ignore_ascii_case(name))
        {
            return Ok(asset);
        }
    }
    Err(TruceError::Update(format!(
        "release {} has no {name} asset",
        release.tag_name
    )))
}

async fn download(client: &reqwest::Client, url: &str, limit: usize) -> Result<Vec<u8>> {
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|err| TruceError::Update(format!("download failed for {url}: {err}")))?;
    let status = response.status();
    if !status.is_success() {
        return Err(TruceError::Update(format!(
            "download failed for {url}: HTTP {status}"
        )));
    }
    if let Some(len) = response.content_length() {
        if len > limit as u64 {
            return Err(TruceError::Update(format!(
                "download for {url} is {len} bytes, over the {limit} byte limit"
            )));
        }
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|err| TruceError::Update(format!("download failed for {url}: {err}")))?;
    if bytes.len() > limit {
        return Err(TruceError::Update(format!(
            "download for {url} is {} bytes, over the {limit} byte limit",
            bytes.len()
        )));
    }
    Ok(bytes.to_vec())
}

/// Check `archive` against `sums` before opening it.
fn verified_binary(archive: &[u8], sums: &str, asset: &PlatformAsset) -> Result<Vec<u8>> {
    let expected = checksum_for(sums, &asset.archive)?;
    let actual = sha256_hex(archive);
    if !actual.eq_ignore_ascii_case(&expected) {
        return Err(TruceError::Update(format!(
            "checksum mismatch for {}: expected {expected}, got {actual}",
            asset.archive
        )));
    }
    extract_binary(archive, asset)
}

fn checksum_for(sums: &str, asset_name: &str) -> Result<String> {
    let mut found: Option<String> = None;
    for raw in sums.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((name, hash)) = parse_sum_line(line) else {
            continue;
        };
        if file_name_of(&name) != asset_name {
            continue;
        }
        if let Some(previous) = &found {
            if previous != &hash {
                return Err(TruceError::Update(format!(
                    "SHA256SUMS has conflicting hashes for {asset_name}"
                )));
            }
            continue;
        }
        found = Some(hash);
    }
    found.ok_or_else(|| TruceError::Update(format!("SHA256SUMS has no entry for {asset_name}")))
}

fn parse_sum_line(line: &str) -> Option<(String, String)> {
    if let Some((left, right)) = line.split_once('=') {
        let hash = right.trim();
        if !is_sha256_hex(hash) {
            return None;
        }
        let name = left.trim();
        let name = name
            .strip_prefix("SHA256")
            .unwrap_or(name)
            .trim()
            .strip_prefix('(')
            .unwrap_or(name.trim())
            .trim()
            .strip_suffix(')')
            .unwrap_or(name.trim())
            .trim();
        return Some((name.to_string(), hash.to_ascii_lowercase()));
    }
    let mut parts = line.split_whitespace();
    let hash = parts.next()?;
    let name = parts.next()?;
    if parts.next().is_some() || !is_sha256_hex(hash) {
        return None;
    }
    let name = name.strip_prefix('*').unwrap_or(name);
    Some((name.to_string(), hash.to_ascii_lowercase()))
}

fn file_name_of(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0xf) as usize] as char);
    }
    out
}

fn extract_binary(archive: &[u8], asset: &PlatformAsset) -> Result<Vec<u8>> {
    let bytes = match asset.kind {
        ArchiveKind::TarGz => extract_tar_gz(archive, asset)?,
        ArchiveKind::Zip => extract_zip(archive, asset)?,
    };
    if bytes.is_empty() {
        return Err(TruceError::Update(format!(
            "archive {} contains an empty {}",
            asset.archive, asset.binary
        )));
    }
    Ok(bytes)
}

fn extract_tar_gz(archive: &[u8], asset: &PlatformAsset) -> Result<Vec<u8>> {
    let decoder = GzDecoder::new(archive);
    let mut tar = tar::Archive::new(decoder);
    let entries = tar.entries().map_err(|err| {
        TruceError::Update(format!(
            "could not read tar archive {}: {err}",
            asset.archive
        ))
    })?;
    let mut found = None;
    for entry in entries {
        let mut entry = entry.map_err(|err| {
            TruceError::Update(format!(
                "could not read tar entry in {}: {err}",
                asset.archive
            ))
        })?;
        let path = entry
            .path()
            .map_err(|err| TruceError::Update(format!("archive entry has a bad path: {err}")))?;
        let path = path.into_owned();
        if path_escapes(&path) {
            return Err(TruceError::Update(format!(
                "archive entry escapes its directory: {}",
                path.display()
            )));
        }
        if !entry.header().entry_type().is_file() {
            continue;
        }
        if !entry_name_is_binary(&path.to_string_lossy(), &asset.binary) {
            continue;
        }
        let bytes = read_entry(&mut entry, asset)?;
        if found.is_some() {
            return Err(TruceError::Update(format!(
                "archive {} contains multiple {} entries",
                asset.archive, asset.binary
            )));
        }
        found = Some(bytes);
    }
    found.ok_or_else(|| {
        TruceError::Update(format!(
            "archive {} does not contain {}",
            asset.archive, asset.binary
        ))
    })
}

fn extract_zip(archive: &[u8], asset: &PlatformAsset) -> Result<Vec<u8>> {
    let cursor = io::Cursor::new(archive);
    let mut zip = zip::ZipArchive::new(cursor).map_err(|err| {
        TruceError::Update(format!(
            "could not read zip archive {}: {err}",
            asset.archive
        ))
    })?;
    let mut found = None;
    for index in 0..zip.len() {
        let mut file = zip.by_index(index).map_err(|err| {
            TruceError::Update(format!(
                "could not read zip entry in {}: {err}",
                asset.archive
            ))
        })?;
        let name = file.name().to_string();
        if file.enclosed_name().is_none() {
            return Err(TruceError::Update(format!(
                "archive entry escapes its directory: {name}"
            )));
        }
        if file.is_dir() || !entry_name_is_binary(&name, &asset.binary) {
            continue;
        }
        let bytes = read_entry(&mut file, asset)?;
        if found.is_some() {
            return Err(TruceError::Update(format!(
                "archive {} contains multiple {} entries",
                asset.archive, asset.binary
            )));
        }
        found = Some(bytes);
    }
    found.ok_or_else(|| {
        TruceError::Update(format!(
            "archive {} does not contain {}",
            asset.archive, asset.binary
        ))
    })
}

fn path_escapes(path: &Path) -> bool {
    path.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    })
}

fn entry_name_is_binary(name: &str, binary: &str) -> bool {
    let normalized = name.replace('\\', "/");
    if normalized.ends_with('/') {
        return false;
    }
    normalized.rsplit('/').next() == Some(binary)
}

fn read_entry(reader: &mut impl Read, asset: &PlatformAsset) -> Result<Vec<u8>> {
    read_limited(reader, MAX_ARCHIVE_BYTES).map_err(|err| {
        TruceError::Update(format!(
            "could not extract {} from {}: {err}",
            asset.binary, asset.archive
        ))
    })
}

fn read_limited(reader: &mut impl Read, limit: usize) -> io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let read = reader.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        if buf.len().saturating_add(read) > limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "archive entry is too large",
            ));
        }
        buf.extend_from_slice(&chunk[..read]);
    }
    Ok(buf)
}

/// Replace `dest` with `bytes`.
///
/// On Unix this is a same-directory write plus `rename`, which replaces a
/// running binary. On Windows a running `.exe` cannot be overwritten, so the
/// current file is renamed aside and the new file takes its place.
fn replace_executable(dest: &Path, bytes: &[u8]) -> Result<()> {
    if bytes.is_empty() {
        return Err(TruceError::Update(
            "refusing to install an empty binary".into(),
        ));
    }
    if dest.file_name().is_none() {
        return Err(TruceError::Update(format!(
            "{} is not a file path",
            dest.display()
        )));
    }
    let parent = dest
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|err| map_replace_error(dest, err))?;
    let tmp = sibling(dest, &format!(".new-{}", std::process::id()));
    if let Err(err) = write_temp(dest, &tmp, bytes) {
        let _ = fs::remove_file(&tmp);
        return Err(err);
    }
    #[cfg(unix)]
    if let Err(err) = make_executable(dest, &tmp) {
        let _ = fs::remove_file(&tmp);
        return Err(map_replace_error(dest, err));
    }
    let swapped = swap_in(&tmp, dest);
    if swapped.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    swapped
}

fn write_temp(dest: &Path, tmp: &Path, bytes: &[u8]) -> Result<()> {
    let result = (|| -> io::Result<()> {
        let mut file = OpenOptions::new().write(true).create_new(true).open(tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(())
    })();
    result.map_err(|err| map_replace_error(dest, err))
}

#[cfg(unix)]
fn make_executable(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = if from.exists() {
        fs::metadata(from)?.permissions().mode() | 0o111
    } else {
        0o755
    };
    let mut perms = fs::metadata(to)?.permissions();
    perms.set_mode(mode);
    fs::set_permissions(to, perms)
}

#[cfg(not(windows))]
fn swap_in(tmp: &Path, dest: &Path) -> Result<()> {
    fs::rename(tmp, dest).map_err(|err| map_replace_error(dest, err))
}

#[cfg(windows)]
fn swap_in(tmp: &Path, dest: &Path) -> Result<()> {
    if !dest.exists() {
        return fs::rename(tmp, dest).map_err(|err| map_replace_error(dest, err));
    }
    let backup = sibling(dest, &format!(".old-{}", std::process::id()));
    fs::rename(dest, &backup).map_err(|err| map_replace_error(dest, err))?;
    if let Err(err) = fs::rename(tmp, dest) {
        let _ = fs::rename(&backup, dest);
        return Err(map_replace_error(dest, err));
    }
    if let Err(err) = fs::remove_file(&backup) {
        eprintln!("note: previous binary left at {} ({err})", backup.display());
    }
    Ok(())
}

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(name),
        _ => PathBuf::from(name),
    }
}

fn map_replace_error(path: &Path, source: io::Error) -> TruceError {
    if source.kind() == io::ErrorKind::PermissionDenied {
        TruceError::Update(format!(
            "cannot replace {}: {source}. {}",
            path.display(),
            permission_hint()
        ))
    } else {
        TruceError::Update(format!("failed to replace {}: {source}", path.display()))
    }
}

fn permission_hint() -> &'static str {
    if cfg!(windows) {
        "Re-run from an elevated terminal."
    } else {
        "Re-run with sudo: sudo truce update"
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Write;
    use std::path::Path;

    use super::*;

    fn version(raw: &str) -> Version {
        parse_version(raw).unwrap()
    }

    #[test]
    fn version_order_ignores_a_leading_v() {
        assert_eq!(version("v1.2.3"), version("1.2.3"));
        assert_eq!(version("V1.2.3"), version("1.2.3"));
        assert!(version("1.2.3") < version("1.2.4"));
        assert!(version("1.2.3") > version("1.2.3-rc.1"));
        assert!(version("1.0.0") < version("v1.0.1"));
        assert!(version("2.0.0") > version("1.9.9"));
        assert!(parse_version("1.2").is_err());
        assert!(parse_version("not-a-version").is_err());
    }

    #[test]
    fn plan_stays_quiet_when_already_current() {
        let current = version("0.1.0");
        let same = version("v0.1.0");
        let newer = version("0.2.0");
        let older = version("0.0.9");
        assert_eq!(plan(&current, &same, false, false), UpdatePlan::UpToDate);
        assert_eq!(plan(&current, &same, true, false), UpdatePlan::UpToDate);
        assert_eq!(plan(&current, &newer, false, true), UpdatePlan::Available);
        assert_eq!(plan(&current, &newer, false, false), UpdatePlan::Install);
        assert_eq!(
            plan(&newer, &current, false, false),
            UpdatePlan::UpToDate,
            "a local build newer than the latest release is not downgraded"
        );
        assert_eq!(plan(&newer, &older, true, false), UpdatePlan::Install);
        assert_eq!(plan(&newer, &older, true, true), UpdatePlan::Available);
    }

    #[test]
    fn tag_normalization_adds_v_and_rejects_paths() {
        assert_eq!(normalize_tag("0.2.0").unwrap(), "v0.2.0");
        assert_eq!(normalize_tag("v0.2.0").unwrap(), "v0.2.0");
        assert_eq!(normalize_tag("V1.2.3-rc.1").unwrap(), "v1.2.3-rc.1");
        assert_eq!(normalize_tag("nightly").unwrap(), "nightly");
        assert!(normalize_tag("v1.2.3/../evil").is_err());
        assert!(normalize_tag("").is_err());
        assert!(release_url("https://api.github.com/", None)
            .unwrap()
            .ends_with("/repos/AJFrio/Truce/releases/latest"));
        assert!(release_url("https://api.github.com", Some("0.2.0"))
            .unwrap()
            .ends_with("/repos/AJFrio/Truce/releases/tags/v0.2.0"));
    }

    #[test]
    fn maps_release_assets_for_supported_targets() {
        let cases = [
            (
                "linux",
                "x86_64",
                "truce-x86_64-unknown-linux-gnu.tar.gz",
                "truce",
                ArchiveKind::TarGz,
            ),
            (
                "linux",
                "amd64",
                "truce-x86_64-unknown-linux-gnu.tar.gz",
                "truce",
                ArchiveKind::TarGz,
            ),
            (
                "linux",
                "aarch64",
                "truce-aarch64-unknown-linux-gnu.tar.gz",
                "truce",
                ArchiveKind::TarGz,
            ),
            (
                "linux",
                "arm64",
                "truce-aarch64-unknown-linux-gnu.tar.gz",
                "truce",
                ArchiveKind::TarGz,
            ),
            (
                "macos",
                "aarch64",
                "truce-aarch64-apple-darwin.tar.gz",
                "truce",
                ArchiveKind::TarGz,
            ),
            (
                "darwin",
                "arm64",
                "truce-aarch64-apple-darwin.tar.gz",
                "truce",
                ArchiveKind::TarGz,
            ),
            (
                "macos",
                "x86_64",
                "truce-x86_64-apple-darwin.tar.gz",
                "truce",
                ArchiveKind::TarGz,
            ),
            (
                "windows",
                "x86_64",
                "truce-x86_64-pc-windows-msvc.zip",
                "truce.exe",
                ArchiveKind::Zip,
            ),
            (
                "windows",
                "amd64",
                "truce-x86_64-pc-windows-msvc.zip",
                "truce.exe",
                ArchiveKind::Zip,
            ),
        ];
        for (os, arch, archive, binary, kind) in cases {
            let asset = asset_for(os, arch).unwrap();
            assert_eq!(asset.archive, archive, "{os}/{arch}");
            assert_eq!(asset.binary, binary);
            assert_eq!(asset.kind, kind);
        }
        assert!(asset_for("windows", "aarch64").is_err());
        assert!(asset_for("linux", "x86").is_err());
        let err = asset_for("freebsd", "x86_64").unwrap_err().to_string();
        assert!(err.contains("unsupported platform"), "{err}");
    }

    #[test]
    fn permission_hint_matches_the_operating_system() {
        let hint = permission_hint();
        if cfg!(windows) {
            assert!(hint.to_ascii_lowercase().contains("elevated"), "{hint}");
        } else {
            assert!(hint.contains("sudo truce update"), "{hint}");
        }
    }

    #[test]
    fn checksum_accepts_gnu_bsd_and_binary_mode_lines() {
        let asset = asset_for("linux", "aarch64").unwrap();
        let bytes = b"archive-bytes";
        let hash = sha256_hex(bytes);
        let upper = hash.to_ascii_uppercase();
        let sums = [
            format!("{hash}  {}\n", asset.archive),
            format!("{hash} {}\n", asset.archive),
            format!("{hash} *{}\n", asset.archive),
            format!("{hash}  ./{}\r\n", asset.archive),
            format!("SHA256 ({}) = {upper}\n", asset.archive),
            format!("# comment\n{hash}  other.txt\n{hash}  {}\n", asset.archive),
        ];
        for sums in sums {
            let err = verified_binary(bytes, &sums, &asset)
                .unwrap_err()
                .to_string();
            assert!(
                !err.contains("checksum mismatch") && !err.contains("no entry"),
                "{sums}\n{err}"
            );
        }
    }

    #[test]
    fn checksum_mismatch_is_reported_before_the_archive_is_opened() {
        let asset = asset_for("linux", "x86_64").unwrap();
        let zeros = "0".repeat(64);
        let err = verified_binary(
            b"this is not a gzip archive",
            &format!("{zeros}  {}\n", asset.archive),
            &asset,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("checksum mismatch"), "{err}");
        assert!(!err.contains("could not read"), "{err}");
    }

    #[test]
    fn missing_checksum_entry_names_the_asset() {
        let asset = asset_for("macos", "aarch64").unwrap();
        let err = verified_binary(
            b"bytes",
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef  other.txt\n",
            &asset,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("no entry"), "{err}");
        assert!(err.contains(&asset.archive), "{err}");
    }

    #[test]
    fn verified_archive_returns_the_binary_and_ignores_the_readme() {
        let asset = asset_for(std::env::consts::OS, std::env::consts::ARCH)
            .expect("this test runner has a published release target");
        let packed = pack(&asset, b"payload-bytes");
        let hash = sha256_hex(&packed);
        let got =
            verified_binary(&packed, &format!("{hash}  {}\n", asset.archive), &asset).unwrap();
        assert_eq!(got, b"payload-bytes");
    }

    #[test]
    fn zip_accepts_backslash_entry_names() {
        let asset = asset_for("windows", "x86_64").unwrap();
        let stem = archive_stem(&asset.archive);
        let archive = zip_with(&[(&format!("{stem}\\{}", asset.binary), &b"MZ-binary"[..])]);
        let got = extract_binary(&archive, &asset).unwrap();
        assert_eq!(got, b"MZ-binary");
    }

    #[test]
    fn zip_rejects_parent_directory_entries() {
        let asset = asset_for("windows", "x86_64").unwrap();
        let archive = zip_with(&[("../truce.exe", &b"evil"[..])]);
        let err = extract_binary(&archive, &asset).unwrap_err().to_string();
        assert!(err.contains("escapes"), "{err}");
    }

    #[test]
    fn tar_rejects_parent_directory_entries() {
        let asset = asset_for("linux", "x86_64").unwrap();
        // The tar writer refuses `..` paths. A hostile archive can still
        // carry one, so the header name is written directly.
        let archive = tar_gz_unchecked_name("../truce", b"evil");
        let err = extract_binary(&archive, &asset).unwrap_err().to_string();
        assert!(err.contains("escapes"), "{err}");
    }

    #[test]
    fn replace_executable_swaps_the_file_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let name = if cfg!(windows) { "truce.exe" } else { "truce" };
        let bin = dir.path().join(name);
        fs::write(&bin, b"old-binary").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&bin).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&bin, perms).unwrap();
        }
        replace_executable(&bin, b"new-binary").unwrap();
        assert_eq!(fs::read(&bin).unwrap(), b"new-binary");
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 1, "{names:?}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&bin).unwrap().permissions().mode();
            assert_ne!(mode & 0o111, 0, "mode {mode:o}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn unwritable_directory_mentions_sudo_and_keeps_the_binary() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("truce");
        fs::write(&bin, b"old").unwrap();
        let mode = fs::metadata(dir.path()).unwrap().permissions().mode();
        struct Restore<'a> {
            path: &'a Path,
            mode: u32,
        }
        impl Drop for Restore<'_> {
            fn drop(&mut self) {
                let mut perms = fs::metadata(self.path).unwrap().permissions();
                perms.set_mode(self.mode);
                fs::set_permissions(self.path, perms).unwrap();
            }
        }
        let _restore = Restore {
            path: dir.path(),
            mode,
        };
        let mut locked = fs::metadata(dir.path()).unwrap().permissions();
        locked.set_mode(0o555);
        fs::set_permissions(dir.path(), locked).unwrap();

        match replace_executable(&bin, b"new") {
            Ok(()) => assert_eq!(fs::read(&bin).unwrap(), b"new"),
            Err(err) => {
                let msg = err.to_string();
                assert!(msg.contains("sudo truce update"), "{msg}");
                assert!(msg.contains("cannot replace"), "{msg}");
                assert_eq!(fs::read(&bin).unwrap(), b"old");
            }
        }
    }

    fn archive_stem(name: &str) -> &str {
        name.strip_suffix(".tar.gz")
            .or_else(|| name.strip_suffix(".zip"))
            .unwrap_or(name)
    }

    fn pack(asset: &PlatformAsset, binary: &[u8]) -> Vec<u8> {
        let stem = archive_stem(&asset.archive);
        let path = format!("{stem}/{}", asset.binary);
        let readme = format!("{stem}/README.md");
        match asset.kind {
            ArchiveKind::TarGz => tar_gz_with(&[(&readme, &b"readme"[..]), (&path, binary)]),
            ArchiveKind::Zip => zip_with(&[(&readme, &b"readme"[..]), (&path, binary)]),
        }
    }

    fn tar_gz_unchecked_name(name: &str, bytes: &[u8]) -> Vec<u8> {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::file());
        header.set_size(bytes.len() as u64);
        header.set_mode(0o755);
        let name_bytes = name.as_bytes();
        assert!(name_bytes.len() < 100);
        header.as_mut_bytes()[..name_bytes.len()].copy_from_slice(name_bytes);
        header.set_cksum();
        let mut raw = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut raw);
            builder.append(&header, bytes).unwrap();
            builder.finish().unwrap();
        }
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(&raw).unwrap();
        enc.finish().unwrap()
    }

    fn tar_gz_with(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut raw = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut raw);
            for (name, bytes) in entries {
                let mut header = tar::Header::new_gnu();
                header.set_path(*name).unwrap();
                header.set_size(bytes.len() as u64);
                header.set_mode(0o755);
                header.set_cksum();
                builder.append(&header, *bytes).unwrap();
            }
            builder.finish().unwrap();
        }
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(&raw).unwrap();
        enc.finish().unwrap()
    }

    fn zip_with(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut cursor = std::io::Cursor::new(Vec::new());
        {
            let mut zip = zip::ZipWriter::new(&mut cursor);
            let options = zip::write::FileOptions::default();
            for (name, bytes) in entries {
                zip.start_file(*name, options).unwrap();
                zip.write_all(bytes).unwrap();
            }
            zip.finish().unwrap();
        }
        cursor.into_inner()
    }

    fn temp_request(
        server: &str,
        check: bool,
        version: Option<&str>,
    ) -> (Request, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir
            .path()
            .join(if cfg!(windows) { "truce.exe" } else { "truce" });
        fs::write(&bin, b"installed-binary").unwrap();
        let req = Request {
            check,
            version: version.map(str::to_string),
            api: server.to_string(),
            install_path: Some(bin),
        };
        (req, dir)
    }

    fn release_json(tag: &str, server: &str, asset: &PlatformAsset) -> serde_json::Value {
        serde_json::json!({
            "tag_name": tag,
            "assets": [
                {
                    "name": asset.archive,
                    "browser_download_url": format!("{server}/assets/{}", asset.archive)
                },
                {
                    "name": "SHA256SUMS",
                    "browser_download_url": format!("{server}/assets/SHA256SUMS")
                }
            ]
        })
    }

    async fn mount_release(server: &wiremock::MockServer, path: &str, body: &serde_json::Value) {
        use wiremock::matchers::{method, path as path_matcher};
        use wiremock::{Mock, ResponseTemplate};
        Mock::given(method("GET"))
            .and(path_matcher(path))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .expect(1)
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn already_up_to_date_does_not_download() {
        use wiremock::MockServer;
        let server = MockServer::start().await;
        let current = current_version().unwrap();
        let tag = format!("v{current}");
        let asset = asset_for(std::env::consts::OS, std::env::consts::ARCH).unwrap();
        mount_release(
            &server,
            "/repos/AJFrio/Truce/releases/latest",
            &release_json(&tag, &server.uri(), &asset),
        )
        .await;
        let (req, dir) = temp_request(&server.uri(), false, None);
        let message = execute(&req).await.unwrap();
        assert_eq!(message, format!("truce {current} is up to date"));
        assert_eq!(
            fs::read(req.install_path.unwrap()).unwrap(),
            b"installed-binary"
        );
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
        drop(dir);
    }

    #[tokio::test]
    async fn check_reports_a_newer_release_without_downloading() {
        use wiremock::MockServer;
        let server = MockServer::start().await;
        let current = current_version().unwrap();
        let asset = asset_for(std::env::consts::OS, std::env::consts::ARCH).unwrap();
        mount_release(
            &server,
            "/repos/AJFrio/Truce/releases/latest",
            &release_json("v99.0.0", &server.uri(), &asset),
        )
        .await;
        let (req, _dir) = temp_request(&server.uri(), true, None);
        let message = execute(&req).await.unwrap();
        assert_eq!(
            message,
            format!("truce {current} is installed; v99.0.0 is available")
        );
        assert_eq!(
            fs::read(req.install_path.unwrap()).unwrap(),
            b"installed-binary"
        );
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn install_replaces_the_target_only_after_the_checksum_matches() {
        use wiremock::matchers::{method, path as path_matcher};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        let asset = asset_for(std::env::consts::OS, std::env::consts::ARCH).unwrap();
        let packed = pack(&asset, b"updated-binary");
        let hash = sha256_hex(&packed);
        let sums = format!("{hash}  {}\n", asset.archive);
        mount_release(
            &server,
            "/repos/AJFrio/Truce/releases/latest",
            &release_json("v99.0.0", &server.uri(), &asset),
        )
        .await;
        Mock::given(method("GET"))
            .and(path_matcher(format!("/assets/{}", asset.archive)))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(packed))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path_matcher("/assets/SHA256SUMS"))
            .respond_with(ResponseTemplate::new(200).set_body_string(sums))
            .expect(1)
            .mount(&server)
            .await;

        let (req, _dir) = temp_request(&server.uri(), false, None);
        let dest = req.install_path.clone().unwrap();
        let message = execute(&req).await.unwrap();
        assert!(message.contains("updated truce from"), "{message}");
        assert!(message.contains("v99.0.0"), "{message}");
        assert_eq!(fs::read(&dest).unwrap(), b"updated-binary");
    }

    #[tokio::test]
    async fn checksum_mismatch_leaves_the_installed_binary_in_place() {
        use wiremock::matchers::{method, path as path_matcher};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        let asset = asset_for(std::env::consts::OS, std::env::consts::ARCH).unwrap();
        let packed = pack(&asset, b"updated-binary");
        let zeros = "0".repeat(64);
        let sums = format!("{zeros}  {}\n", asset.archive);
        mount_release(
            &server,
            "/repos/AJFrio/Truce/releases/latest",
            &release_json("v99.0.0", &server.uri(), &asset),
        )
        .await;
        Mock::given(method("GET"))
            .and(path_matcher(format!("/assets/{}", asset.archive)))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(packed))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path_matcher("/assets/SHA256SUMS"))
            .respond_with(ResponseTemplate::new(200).set_body_string(sums))
            .mount(&server)
            .await;

        let (req, _dir) = temp_request(&server.uri(), false, None);
        let dest = req.install_path.clone().unwrap();
        let err = execute(&req).await.unwrap_err().to_string();
        assert!(err.contains("checksum mismatch"), "{err}");
        assert_eq!(fs::read(dest).unwrap(), b"installed-binary");
    }

    #[tokio::test]
    async fn explicit_version_uses_the_tag_endpoint() {
        use wiremock::MockServer;
        let server = MockServer::start().await;
        let current = current_version().unwrap();
        let asset = asset_for(std::env::consts::OS, std::env::consts::ARCH).unwrap();
        let path = format!("/repos/AJFrio/Truce/releases/tags/v{current}");
        mount_release(
            &server,
            &path,
            &release_json(&format!("v{current}"), &server.uri(), &asset),
        )
        .await;
        let (req, _dir) = temp_request(&server.uri(), false, Some(&current.to_string()));
        let message = execute(&req).await.unwrap();
        assert_eq!(message, format!("truce {current} is up to date"));
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].url.path(), path);
    }
}
