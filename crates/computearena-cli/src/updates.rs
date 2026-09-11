//! A quiet, cached update hint for the interactive client.
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

const LATEST_RELEASE_API: &str =
    "https://api.github.com/repos/basecompute/computearena-cli/releases/latest";
const UPDATE_CACHE_FILE: &str = "update-check.json";
const UPDATE_CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const UPDATE_HTTP_TIMEOUT: Duration = Duration::from_secs(3);

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

#[derive(Clone, Debug)]
struct CachedRelease {
    checked_at: u64,
    version: Version,
    release_url: String,
}

pub(crate) struct UpdateCheck {
    receiver: Receiver<Option<UpdateNotice>>,
}

impl UpdateCheck {
    /// `None` means still running; `Some(None)` means the check completed with
    /// no update (including an offline/network failure).
    pub(crate) fn poll(&self) -> Option<Option<UpdateNotice>> {
        match self.receiver.try_recv() {
            Ok(notice) => Some(notice),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(None),
        }
    }
}

pub(crate) fn start(paths: &Paths) -> (Option<UpdateNotice>, Option<UpdateCheck>) {
    let file = paths.root.join(UPDATE_CACHE_FILE);
    let cached = read_cache(&file);
    let initial = cached.as_ref().and_then(update_notice);
    if cached.as_ref().is_some_and(cache_is_fresh) {
        return (initial, None);
    }

    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let result = fetch_latest();
        if let Some(release) = result.as_ref() {
            let _ = write_cache(&file, release);
        }
        let _ = sender.send(result.as_ref().and_then(update_notice));
    });
    (initial, Some(UpdateCheck { receiver }))
}

fn current_version() -> Option<Version> {
    Version::parse(env!("CARGO_PKG_VERSION")).ok()
}

fn update_notice(release: &CachedRelease) -> Option<UpdateNotice> {
    (release.version > current_version()?).then(|| UpdateNotice {
        latest: release.version.clone(),
    })
}

fn cache_is_fresh(release: &CachedRelease) -> bool {
    unix_seconds().saturating_sub(release.checked_at) < UPDATE_CACHE_TTL.as_secs()
}

fn fetch_latest() -> Option<CachedRelease> {
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(UPDATE_HTTP_TIMEOUT)
        .timeout(UPDATE_HTTP_TIMEOUT)
        .user_agent(concat!("computearena-cli/", env!("CARGO_PKG_VERSION")))
        .build()
        .ok()?;
    let response = client
        .get(LATEST_RELEASE_API)
        .header("Accept", "application/vnd.github+json")
        .send()
        .ok()?
        .error_for_status()
        .ok()?;
    let body = response.text().ok()?;
    let value: Value = serde_json::from_str(&body).ok()?;
    let tag = value.get("tag_name")?.as_str()?.trim_start_matches('v');
    Some(CachedRelease {
        checked_at: unix_seconds(),
        version: Version::parse(tag).ok()?,
        release_url: value
            .get("html_url")
            .and_then(Value::as_str)
            .unwrap_or(COMPUTEARENA_QUICKSTART)
            .to_string(),
    })
}

fn read_cache(file: &std::path::Path) -> Option<CachedRelease> {
    let value: Value = serde_json::from_slice(&fs::read(file).ok()?).ok()?;
    Some(CachedRelease {
        checked_at: value.get("checkedAtUnixSeconds")?.as_u64()?,
        version: Version::parse(value.get("latestVersion")?.as_str()?).ok()?,
        release_url: value.get("releaseUrl")?.as_str()?.to_string(),
    })
}

fn write_cache(file: &std::path::Path, release: &CachedRelease) -> anyhow::Result<()> {
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
mod tests {
    use super::*;

    fn release(version: &str) -> CachedRelease {
        CachedRelease {
            checked_at: unix_seconds(),
            version: Version::parse(version).unwrap(),
            release_url: format!("https://example.test/v{version}"),
        }
    }

    #[test]
    fn only_newer_semantic_versions_create_a_notice() {
        let current = current_version().unwrap();
        assert!(update_notice(&release(&current.to_string())).is_none());
        let newer = Version::new(current.major, current.minor, current.patch + 1);
        assert_eq!(
            update_notice(&release(&newer.to_string())).unwrap().latest,
            newer
        );
    }

    #[test]
    fn cache_round_trips() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join(UPDATE_CACHE_FILE);
        let expected = release("9.8.7");
        write_cache(&file, &expected).unwrap();
        let actual = read_cache(&file).unwrap();
        assert_eq!(actual.version, expected.version);
        assert_eq!(actual.release_url, expected.release_url);
        assert!(cache_is_fresh(&actual));
    }
}
