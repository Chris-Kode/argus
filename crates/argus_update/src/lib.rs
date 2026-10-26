//! Self-update of the macOS app from GitHub Releases.
//!
//! [`check`] asks GitHub for the latest release and returns it when it is
//! newer than this build. [`install`] downloads its `.zip`, checks it (the
//! release's SHA-256 digest, the bundle identifier, the version, the code
//! signature) and swaps it in place of the running `Argus.app`; the caller
//! then restarts the app.
//!
//! Everything goes through system tools (`curl`, `shasum`, `ditto`,
//! `codesign`, `PlistBuddy`) so no HTTP or archive stack is linked in. Every
//! child process is killed when its future is dropped, except the final
//! swap of the two bundles, which always runs to completion.

use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde::Deserialize;
use tokio::process::Command;

/// The latest published (non-draft, non-pre-release) release.
const LATEST_RELEASE_URL: &str = "https://api.github.com/repos/Chris-Kode/argus/releases/latest";
/// Overrides [`LATEST_RELEASE_URL`]; any URL `curl` reads, `file://`
/// included, so the flow can be tried against a local feed.
const FEED_ENV: &str = "ARGUS_UPDATE_FEED";
/// `CFBundleIdentifier` a downloaded app must carry.
const BUNDLE_ID: &str = "io.github.chris-kode.argus";
/// This build's version.
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
/// Name prefix of the new bundle while it is copied next to the old one.
const INCOMING: &str = ".Argus.app.incoming";
/// Name prefix of the old bundle between the swap and its deletion.
const OUTGOING: &str = ".Argus.app.outgoing";

/// Result alias used across `argus_update`.
pub type Result<T, E = UpdateError> = std::result::Result<T, E>;

/// Everything that can go wrong while checking for or installing an update.
///
/// Messages are meant to be shown to the user as-is.
#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    /// A system tool could not be started.
    #[error("failed to run `{program}`: {source}")]
    Spawn {
        /// The tool.
        program: &'static str,
        /// Why it did not start.
        source: io::Error,
    },

    /// A system tool exited with an error.
    #[error("`{program}` failed: {message}")]
    Command {
        /// The tool.
        program: &'static str,
        /// Its stderr, or the exit status when it printed nothing.
        message: String,
    },

    /// The release feed is not what GitHub returns.
    #[error("unexpected release information: {0}")]
    Feed(String),

    /// The release has no `.zip` for this Mac.
    #[error("release {0} has no macOS download")]
    NoAsset(String),

    /// The download does not match the release's digest.
    #[error("the download is corrupt (its checksum does not match the release)")]
    Checksum,

    /// The downloaded bundle is not the expected Argus.
    #[error("the downloaded app is not a valid Argus {version}: {reason}")]
    InvalidApp {
        /// The release's version.
        version: String,
        /// What did not match.
        reason: String,
    },

    /// A file operation failed.
    #[error("{action} {}: {source}", path.display())]
    Io {
        /// What was being done, e.g. "cannot replace".
        action: &'static str,
        /// The path it was done to.
        path: PathBuf,
        /// The underlying error.
        source: io::Error,
    },
}

/// A version as Argus tags it: `MAJOR.MINOR.PATCH`, optionally with a
/// `-pre-release` suffix, which sorts before the same version without one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    major: u64,
    minor: u64,
    patch: u64,
    /// `false` for a pre-release, so that `1.0.0-beta < 1.0.0`.
    stable: bool,
}

impl Version {
    /// Parses `1.2.3` or `1.2.3-beta.1`; a leading `v` is accepted.
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.strip_prefix('v').unwrap_or(text);
        let (core, pre) = match text.split_once('-') {
            Some((core, pre)) => (core, Some(pre)),
            None => (text, None),
        };
        let mut parts = core.split('.').map(str::parse::<u64>);
        let version = Self {
            major: parts.next()?.ok()?,
            minor: parts.next()?.ok()?,
            patch: parts.next()?.ok()?,
            stable: pre.is_none(),
        };
        parts.next().is_none().then_some(version)
    }

    /// This build's version.
    pub fn current() -> Self {
        Self::parse(CURRENT_VERSION).unwrap_or(Self {
            major: 0,
            minor: 0,
            patch: 0,
            stable: false,
        })
    }
}

/// A release newer than this build, with the download for this Mac.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    /// The version as tagged, without the `v`.
    pub version: String,
    /// The release page on GitHub.
    pub page_url: String,
    /// The `.zip` to download.
    asset_url: String,
    /// The asset's `sha256:<hex>` digest, when GitHub reports one.
    digest: Option<String>,
}

/// GitHub's release JSON, the fields Argus reads.
#[derive(Deserialize)]
struct FeedRelease {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    assets: Vec<FeedAsset>,
}

#[derive(Deserialize)]
struct FeedAsset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    digest: Option<String>,
}

/// This build's version, for messages.
pub fn current_version() -> &'static str {
    CURRENT_VERSION
}

/// Asks GitHub for the latest release; `Some` when it is newer than this
/// build.
pub async fn check() -> Result<Option<Release>> {
    let feed = std::env::var(FEED_ENV)
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| LATEST_RELEASE_URL.to_owned());
    let user_agent = format!("Argus/{CURRENT_VERSION}");
    let body = match run(
        "curl",
        Command::new("curl")
            .args(["--fail", "--silent", "--show-error", "--location"])
            .args(["--max-time", "30", "--user-agent", &user_agent])
            .args(["--header", "Accept: application/vnd.github+json"])
            .arg(&feed),
    )
    .await
    {
        Ok(body) => body,
        // GitHub answers 404 while no release has been published.
        Err(UpdateError::Command { message, .. }) if message.contains("error: 404") => {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    let release = parse_release(&body)?;
    let newer = Version::parse(&release.version).is_some_and(|v| v > Version::current());
    Ok(newer.then_some(release))
}

/// Reads the release JSON and picks the `.zip` for this Mac: the universal
/// build, else the one for this architecture.
fn parse_release(body: &str) -> Result<Release> {
    let feed: FeedRelease =
        serde_json::from_str(body).map_err(|error| UpdateError::Feed(error.to_string()))?;
    let version = feed
        .tag_name
        .strip_prefix('v')
        .unwrap_or(&feed.tag_name)
        .to_owned();
    if Version::parse(&version).is_none() {
        return Err(UpdateError::Feed(format!(
            "`{}` is not a version",
            feed.tag_name
        )));
    }
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        arch => arch,
    };
    let wanted = [
        format!("Argus-{version}-macos-universal.zip"),
        format!("Argus-{version}-macos-{arch}.zip"),
    ];
    let asset = wanted
        .iter()
        .find_map(|name| feed.assets.iter().find(|asset| &asset.name == name))
        .ok_or_else(|| UpdateError::NoAsset(version.clone()))?;
    Ok(Release {
        page_url: feed.html_url,
        asset_url: asset.browser_download_url.clone(),
        digest: asset.digest.clone(),
        version,
    })
}

/// Why the app at `app` cannot be updated in place, if it cannot: it runs
/// translocated (straight from the download, or from the disk image) or its
/// folder is not writable.
pub async fn install_blocker(app: &Path) -> Option<String> {
    let in_place_hint = "Move Argus to the Applications folder to update it.";
    if app
        .components()
        .any(|part| part.as_os_str() == "AppTranslocation")
    {
        return Some(in_place_hint.to_owned());
    }
    let parent = app.parent()?;
    let probe = parent.join(format!(".argus-update-probe-{}", std::process::id()));
    match tokio::fs::write(&probe, b"").await {
        Ok(()) => {
            let _ = tokio::fs::remove_file(&probe).await;
            None
        }
        Err(_) if parent.starts_with("/Volumes") => Some(in_place_hint.to_owned()),
        Err(_) => Some(format!(
            "Argus cannot write to {}, so it cannot update itself.",
            parent.display()
        )),
    }
}

/// Downloads `release` into `updates_dir`, checks it and puts it in place of
/// the bundle at `app`. Restart the app afterwards to run the new version.
///
/// Dropping the future cancels the download and the checks; once the
/// bundles start to swap, the swap finishes regardless.
pub async fn install(release: &Release, app: &Path, updates_dir: &Path) -> Result<()> {
    let staging = updates_dir.join(&release.version);
    remove_path(&staging).await?;
    tokio::fs::create_dir_all(&staging)
        .await
        .map_err(io_error("cannot create", &staging))?;

    let zip = staging.join("Argus.zip");
    tracing::info!(version = %release.version, url = %release.asset_url, "downloading update");
    run(
        "curl",
        Command::new("curl")
            .args(["--fail", "--silent", "--show-error", "--location"])
            .args(["--retry", "2", "--max-time", "900", "--output"])
            .arg(&zip)
            .arg(&release.asset_url),
    )
    .await?;
    if let Some(expected) = release
        .digest
        .as_deref()
        .and_then(|digest| digest.strip_prefix("sha256:"))
    {
        let output = run(
            "shasum",
            Command::new("shasum").args(["-a", "256"]).arg(&zip),
        )
        .await?;
        let actual = output.split_whitespace().next().unwrap_or_default();
        if !actual.eq_ignore_ascii_case(expected) {
            return Err(UpdateError::Checksum);
        }
    }
    run(
        "ditto",
        Command::new("ditto")
            .args(["-x", "-k"])
            .arg(&zip)
            .arg(&staging),
    )
    .await?;
    let downloaded = staging.join("Argus.app");
    verify(&downloaded, &release.version, app).await?;

    // Copied next to the old bundle first, so the swap is two renames on one
    // volume.
    let parent = app.parent().ok_or_else(|| UpdateError::Io {
        action: "cannot replace",
        path: app.to_owned(),
        source: io::Error::from(io::ErrorKind::InvalidInput),
    })?;
    sweep_leftovers(parent).await;
    // Fresh names per attempt, so a leftover that cannot be deleted never
    // blocks the next update.
    let suffix = unique_suffix();
    let incoming = Leftover::new(parent.join(format!("{INCOMING}{suffix}")));
    run(
        "ditto",
        Command::new("ditto").arg(&downloaded).arg(incoming.path()),
    )
    .await?;

    let outgoing = parent.join(format!("{OUTGOING}{suffix}"));
    let current = app.to_owned();
    // `spawn_blocking` work is never aborted: the bundle is never left half
    // swapped, even if the caller goes away. `incoming` moves in with it so
    // its cleanup cannot race the swap.
    tokio::task::spawn_blocking(move || {
        let result = swap(&current, incoming.path(), &outgoing);
        if result.is_ok() {
            incoming.keep();
        }
        result
    })
    .await
    .map_err(|error| UpdateError::Io {
        action: "cannot replace",
        path: app.to_owned(),
        source: io::Error::other(error),
    })??;
    if let Err(error) = remove_path(&staging).await {
        tracing::warn!(%error, "cannot clean up the update download");
    }
    tracing::info!(version = %release.version, "update installed");
    Ok(())
}

/// Checks that `downloaded` is Argus `version`, validly signed, and signed by
/// the same team as `current` when `current` has a Developer ID signature.
async fn verify(downloaded: &Path, version: &str, current: &Path) -> Result<()> {
    let invalid = |reason: String| UpdateError::InvalidApp {
        version: version.to_owned(),
        reason,
    };
    let plist = downloaded.join("Contents").join("Info.plist");
    if !tokio::fs::try_exists(&plist).await.unwrap_or(false) {
        return Err(invalid("there is no Argus.app in the download".to_owned()));
    }
    let identifier = plist_value(&plist, "CFBundleIdentifier").await?;
    if identifier != BUNDLE_ID {
        return Err(invalid(format!("its identifier is `{identifier}`")));
    }
    let bundle_version = plist_value(&plist, "CFBundleShortVersionString").await?;
    if bundle_version != version {
        return Err(invalid(format!("it is version {bundle_version}")));
    }
    run(
        "codesign",
        Command::new("codesign")
            .args(["--verify", "--strict"])
            .arg(downloaded),
    )
    .await
    .map_err(|error| invalid(error.to_string()))?;
    if let Some(team) = team_id(current).await {
        match team_id(downloaded).await {
            Some(new_team) if new_team == team => {}
            _ => return Err(invalid("it is not signed by the same developer".to_owned())),
        }
    }
    Ok(())
}

/// One top-level string of an `Info.plist`.
async fn plist_value(plist: &Path, key: &str) -> Result<String> {
    let output = run(
        "PlistBuddy",
        Command::new("/usr/libexec/PlistBuddy")
            .arg("-c")
            .arg(format!("Print :{key}"))
            .arg(plist),
    )
    .await?;
    Ok(output.trim().to_owned())
}

/// The Developer ID team that signed `app`; `None` when it is signed ad hoc
/// or not at all.
async fn team_id(app: &Path) -> Option<String> {
    let output = Command::new("codesign")
        .args(["--display", "--verbose=2"])
        .arg(app)
        .kill_on_drop(true)
        .output()
        .await
        .ok()?;
    // `codesign --display` writes its report to stderr.
    String::from_utf8_lossy(&output.stderr)
        .lines()
        .find_map(|line| line.strip_prefix("TeamIdentifier="))
        .map(str::trim)
        .filter(|team| *team != "not set")
        .map(str::to_owned)
}

/// Moves `current` aside to `outgoing`, `incoming` into its place, then
/// deletes the old bundle. Puts `current` back when the second move fails.
fn swap(current: &Path, incoming: &Path, outgoing: &Path) -> Result<()> {
    std::fs::rename(current, outgoing).map_err(io_error("cannot move aside", current))?;
    if let Err(source) = std::fs::rename(incoming, current) {
        if let Err(error) = std::fs::rename(outgoing, current) {
            tracing::error!(%error, path = %outgoing.display(), "cannot restore the previous app");
        }
        return Err(UpdateError::Io {
            action: "cannot replace",
            path: current.to_owned(),
            source,
        });
    }
    // The running process keeps its mapped binary; nothing else is read from
    // the bundle once the app has started.
    if let Err(error) = remove_path_blocking(outgoing) {
        tracing::warn!(%error, "cannot delete the previous app");
    }
    Ok(())
}

/// Runs `command` and returns its stdout, or its stderr as the error.
async fn run(program: &'static str, command: &mut Command) -> Result<String> {
    let output = command
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|source| UpdateError::Spawn { program, source })?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    Err(UpdateError::Command {
        program,
        message: if stderr.is_empty() {
            output.status.to_string()
        } else {
            stderr
        },
    })
}

fn io_error(action: &'static str, path: &Path) -> impl FnOnce(io::Error) -> UpdateError {
    let path = path.to_owned();
    move |source| UpdateError::Io {
        action,
        path,
        source,
    }
}

/// Deletes a file or directory tree; a missing path is fine.
async fn remove_path(path: &Path) -> Result<()> {
    let owned = path.to_owned();
    tokio::task::spawn_blocking(move || remove_path_blocking(&owned))
        .await
        .map_err(|error| UpdateError::Io {
            action: "cannot delete",
            path: path.to_owned(),
            source: io::Error::other(error),
        })?
}

fn remove_path_blocking(path: &Path) -> Result<()> {
    let mut result = remove_tree(path);
    // A copied bundle can hold read-only directories, whose entries can only
    // be deleted once the directories are writable again.
    if matches!(&result, Err(error) if error.kind() == io::ErrorKind::PermissionDenied) {
        make_writable(path);
        result = remove_tree(path);
    }
    result.map_err(io_error("cannot delete", path))
}

fn remove_tree(path: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Gives the owner full access to every directory under `path`, as far as
/// it is allowed to; files are left alone, deleting them needs only a
/// writable parent.
#[cfg(unix)]
fn make_writable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;

    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return;
    };
    if !meta.is_dir() {
        return;
    }
    let mut permissions = meta.permissions();
    permissions.set_mode(permissions.mode() | 0o700);
    let _ = std::fs::set_permissions(path, permissions);
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            make_writable(&entry.path());
        }
    }
}

#[cfg(not(unix))]
fn make_writable(_path: &Path) {}

/// Deletes the copies earlier, interrupted attempts left next to the app,
/// as far as it can; what stays only costs disk space.
async fn sweep_leftovers(parent: &Path) {
    let parent = parent.to_owned();
    let _ = tokio::task::spawn_blocking(move || {
        let Ok(entries) = std::fs::read_dir(&parent) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !(name.starts_with(INCOMING) || name.starts_with(OUTGOING)) {
                continue;
            }
            if let Err(error) = remove_path_blocking(&entry.path()) {
                tracing::warn!(%error, "cannot delete a leftover from an earlier update");
            }
        }
    })
    .await;
}

/// A suffix no earlier attempt has used.
fn unique_suffix() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    format!("-{}-{nanos}", std::process::id())
}

/// A path that is deleted in the background when dropped, unless kept: the
/// new bundle's copy is cleaned up when the copy or the swap fails, or the
/// install is cancelled.
struct Leftover {
    path: PathBuf,
    armed: bool,
}

impl Leftover {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    /// The path is in use now; leave it be.
    fn keep(mut self) {
        self.armed = false;
    }
}

impl Drop for Leftover {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let path = std::mem::take(&mut self.path);
        let remove = move || {
            if let Err(error) = remove_path_blocking(&path) {
                tracing::warn!(%error, "cannot clean up the update copy");
            }
        };
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn_blocking(remove);
            }
            Err(_) => remove(),
        }
    }
}
