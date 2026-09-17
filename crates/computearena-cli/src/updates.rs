//! Quiet, cached release hints: one for the client itself and one for BaseRT.
//!
//! The GitHub request runs on a worker thread and failures are deliberately
//! ignored: running and retaining benchmarks must keep working offline. A
//! successful result is cached for a day to avoid unnecessary API traffic.

use crate::config::COMPUTEARENA_QUICKSTART;
use crate::reports::Paths;
use semver::Version;
use serde_json::{json, Value};
use std::fs;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const UPDATE_CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const UPDATE_HTTP_TIMEOUT: Duration = Duration::from_secs(3);

/// Where the newest release of something is published, and where the answer
/// is kept between sessions.
pub(crate) struct ReleaseFeed {
    latest_release_api: &'static str,
    /// Names a different endpoint, for tests and mirrors. Only a version is
    /// ever read from the answer, so it cannot put words on the screen.
    api_override: Option<&'static str>,
    cache_file: &'static str,
    fallback_url: &'static str,
}

const COMPUTEARENA: ReleaseFeed = ReleaseFeed {
    latest_release_api: "https://api.github.com/repos/basecompute/computearena-cli/releases/latest",
    api_override: None,
    cache_file: "update-check.json",
    fallback_url: COMPUTEARENA_QUICKSTART,
};

pub(crate) const BASERT: ReleaseFeed = ReleaseFeed {
    latest_release_api: "https://api.github.com/repos/basecompute/baseRT/releases/latest",
    api_override: Some("COMPUTEARENA_BASERT_RELEASE_API"),
    cache_file: "basert-update-check.json",
    fallback_url: crate::runtimes::BASERT_RELEASES,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LatestRelease {
    checked_at: u64,
    pub(crate) version: Version,
    release_url: String,
}

/// A release lookup running on a worker thread.
pub(crate) struct ReleaseCheck {
    receiver: Receiver<Option<LatestRelease>>,
}

impl ReleaseCheck {
    /// `None` means still running; `Some(None)` means the check completed
    /// without an answer (including an offline/network failure).
    pub(crate) fn poll(&self) -> Option<Option<LatestRelease>> {
        match self.receiver.try_recv() {
            Ok(release) => Some(release),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(None),
        }
    }
}

impl ReleaseFeed {
    /// What the last lookup found, and whether it is recent enough to reuse.
    fn known(&self, paths: &Paths) -> (Option<LatestRelease>, bool) {
        let cached = read_cache(&paths.root.join(self.cache_file));
        let fresh = cached.as_ref().is_some_and(cache_is_fresh);
        (cached, fresh)
    }

    /// What is already known, and a lookup in progress when that is missing
    /// or more than a day old. Never waits for the network.
    pub(crate) fn start(&self, paths: &Paths) -> (Option<LatestRelease>, Option<ReleaseCheck>) {
        let (cached, fresh) = self.known(paths);
        if fresh {
            return (cached, None);
        }

        let file = paths.root.join(self.cache_file);
        let api = self
            .api_override
            .and_then(|variable| std::env::var(variable).ok())
            .filter(|url| !url.is_empty())
            .unwrap_or_else(|| self.latest_release_api.to_string());
        let fallback_url = self.fallback_url;
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let result = fetch_latest(&api, fallback_url);
            if let Some(release) = result.as_ref() {
                let _ = write_cache(&file, release);
            }
            let _ = sender.send(result);
        });
        (cached, Some(ReleaseCheck { receiver }))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct UpdateNotice {
    latest: Version,
}

impl UpdateNotice {
    pub(crate) fn message(&self) -> String {
        format!(
            "ComputeArena {} is available · update at {}",
            self.latest, COMPUTEARENA_QUICKSTART
        )
    }
}

pub(crate) struct UpdateCheck(ReleaseCheck);

impl UpdateCheck {
    /// `None` means still running; `Some(None)` means the check completed with
    /// no update (including an offline/network failure).
    pub(crate) fn poll(&self) -> Option<Option<UpdateNotice>> {
        self.0
            .poll()
            .map(|release| release.as_ref().and_then(update_notice))
    }
}

/// The client's own update hint.
pub(crate) fn start(paths: &Paths) -> (Option<UpdateNotice>, Option<UpdateCheck>) {
    let (known, check) = COMPUTEARENA.start(paths);
    (
        known.as_ref().and_then(update_notice),
        check.map(UpdateCheck),
    )
}

fn current_version() -> Option<Version> {
    Version::parse(env!("CARGO_PKG_VERSION")).ok()
}

fn update_notice(release: &LatestRelease) -> Option<UpdateNotice> {
    (release.version > current_version()?).then(|| UpdateNotice {
        latest: release.version.clone(),
    })
}

fn cache_is_fresh(release: &LatestRelease) -> bool {
    unix_seconds().saturating_sub(release.checked_at) < UPDATE_CACHE_TTL.as_secs()
}

fn fetch_latest(api: &str, fallback_url: &str) -> Option<LatestRelease> {
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(UPDATE_HTTP_TIMEOUT)
        .timeout(UPDATE_HTTP_TIMEOUT)
        .user_agent(concat!("computearena-cli/", env!("CARGO_PKG_VERSION")))
        .build()
        .ok()?;
    let response = client
        .get(api)
        .header("Accept", "application/vnd.github+json")
        .send()
        .ok()?
        .error_for_status()
        .ok()?;
    let body = response.text().ok()?;
    let value: Value = serde_json::from_str(&body).ok()?;
    let tag = value.get("tag_name")?.as_str()?.trim_start_matches('v');
    Some(LatestRelease {
        checked_at: unix_seconds(),
        version: Version::parse(tag).ok()?,
        release_url: value
            .get("html_url")
            .and_then(Value::as_str)
            .unwrap_or(fallback_url)
            .to_string(),
    })
}

fn read_cache(file: &std::path::Path) -> Option<LatestRelease> {
    let value: Value = serde_json::from_slice(&fs::read(file).ok()?).ok()?;
    Some(LatestRelease {
        checked_at: value.get("checkedAtUnixSeconds")?.as_u64()?,
        version: Version::parse(value.get("latestVersion")?.as_str()?).ok()?,
        release_url: value.get("releaseUrl")?.as_str()?.to_string(),
    })
}

fn write_cache(file: &std::path::Path, release: &LatestRelease) -> anyhow::Result<()> {
    if let Some(parent) = file.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(
        file,
        serde_json::to_vec_pretty(&json!({
            "checkedAtUnixSeconds": release.checked_at,
            "latestVersion": release.version.to_string(),
            "releaseUrl": release.release_url,
        }))?,
    )?;
    Ok(())
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
pub(crate) fn release_for_tests(version: &str) -> LatestRelease {
    LatestRelease {
        checked_at: unix_seconds(),
        version: Version::parse(version).unwrap(),
        release_url: format!("https://example.test/v{version}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_newer_semantic_versions_create_a_notice() {
        let current = current_version().unwrap();
        assert!(update_notice(&release_for_tests(&current.to_string())).is_none());
        let newer = Version::new(current.major, current.minor, current.patch + 1);
        assert_eq!(
            update_notice(&release_for_tests(&newer.to_string()))
                .unwrap()
                .latest,
            newer
        );
    }

    #[test]
    fn cache_round_trips() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join(COMPUTEARENA.cache_file);
        let expected = release_for_tests("9.8.7");
        write_cache(&file, &expected).unwrap();
        let actual = read_cache(&file).unwrap();
        assert_eq!(actual.version, expected.version);
        assert_eq!(actual.release_url, expected.release_url);
        assert!(cache_is_fresh(&actual));
    }

    #[test]
    fn a_fresh_answer_is_reused_without_a_lookup_and_feeds_do_not_share_it() {
        let directory = tempfile::tempdir().unwrap();
        let paths = Paths::resolve(Some(directory.path().to_path_buf())).unwrap();
        fs::create_dir_all(&paths.root).unwrap();
        write_cache(
            &paths.root.join(BASERT.cache_file),
            &release_for_tests("0.2.5"),
        )
        .unwrap();

        let (known, check) = BASERT.start(&paths);
        assert_eq!(known.unwrap().version, Version::new(0, 2, 5));
        assert!(check.is_none(), "a fresh answer must not start a lookup");
        // BaseRT's newest release says nothing about the client's own.
        assert!(read_cache(&paths.root.join(COMPUTEARENA.cache_file)).is_none());
    }

    #[test]
    fn a_stale_answer_is_still_offered_while_it_is_refreshed() {
        let directory = tempfile::tempdir().unwrap();
        let paths = Paths::resolve(Some(directory.path().to_path_buf())).unwrap();
        fs::create_dir_all(&paths.root).unwrap();
        assert_eq!(BASERT.known(&paths), (None, false));
        let mut stale = release_for_tests("0.2.5");
        stale.checked_at -= UPDATE_CACHE_TTL.as_secs() + 1;
        write_cache(&paths.root.join(BASERT.cache_file), &stale).unwrap();
        assert_eq!(BASERT.known(&paths), (Some(stale), false));
    }
}
