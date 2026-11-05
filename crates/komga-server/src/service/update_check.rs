//! Daily check for newer kmrs releases on GitHub (kmrs enhancement; the Java version only
//! fetches the release list on demand for `/api/v1/releases`). The result lives in memory
//! and surfaces at `GET /api/v1/update-check`.

use serde::{Deserialize, Serialize};
use std::sync::Arc;
use time::OffsetDateTime;

const GITHUB_RELEASES_URL: &str = "https://api.github.com/repos/kmworks/kmrs/releases";

/// A newer upstream release, as exposed at `/api/v1/update-check`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateInfo {
    pub version: String,
    pub url: String,
    #[serde(with = "komga_core::dto::progression::zoned_date_time")]
    pub release_date: OffsetDateTime,
}

/// What one successful check learned: when it ran, and what it found (`None` = up to date).
#[derive(Debug, Clone)]
pub struct CheckOutcome {
    pub checked_at: OffsetDateTime,
    pub update: Option<UpdateInfo>,
}

/// Shared outcome of the update check: `None` until the first successful run.
#[derive(Debug, Clone)]
pub struct LatestUpdate {
    outcome: Arc<std::sync::RwLock<Option<CheckOutcome>>>,
    // the newest version found, for the desktop menu; changes only when the version does
    found: Arc<tokio::sync::watch::Sender<Option<String>>>,
}

impl Default for LatestUpdate {
    fn default() -> Self {
        Self {
            outcome: Arc::default(),
            found: Arc::new(tokio::sync::watch::channel(None).0),
        }
    }
}

impl LatestUpdate {
    pub fn get(&self) -> Option<CheckOutcome> {
        self.outcome.read().unwrap().clone()
    }

    pub fn set(&self, outcome: Option<CheckOutcome>) {
        let version = outcome
            .as_ref()
            .and_then(|o| o.update.as_ref())
            .map(|u| u.version.clone());
        *self.outcome.write().unwrap() = outcome;
        self.found.send_if_modified(|found| {
            let changed = *found != version;
            *found = version;
            changed
        });
    }

    /// The newest version found now and whenever a check finds a different one.
    pub fn subscribe_found(&self) -> tokio::sync::watch::Receiver<Option<String>> {
        self.found.subscribe()
    }
}

/// Runs the check on startup and every 24h; a finished placeholder task when disabled
/// (same lifecycle shape as the other schedulers).
pub fn start(state: crate::state::AppState) -> tokio::task::JoinHandle<()> {
    if !state.config.update_check_enabled {
        return tokio::spawn(async {});
    }
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(86_400));
        loop {
            // the first tick fires immediately, so the body runs at startup too
            interval.tick().await;
            check_once(&state.latest_update, GITHUB_RELEASES_URL).await;
        }
    })
}

/// A failed check keeps the previous state: a network blip must not hide a known update.
async fn check_once(latest: &LatestUpdate, base_url: &str) {
    let releases = match fetch_releases(&github_client(), base_url).await {
        Ok(releases) => releases,
        Err(e) => {
            tracing::warn!("update check failed: {e}");
            return;
        }
    };
    let current =
        semver::Version::parse(env!("CARGO_PKG_VERSION")).expect("CARGO_PKG_VERSION is semver");
    let update = select_update(&releases, &current).map(|(version, release)| UpdateInfo {
        version: version.to_string(),
        url: release.html_url.clone(),
        release_date: release.published_at,
    });
    // the daily re-check usually finds the same release; log only on a version change
    let known = latest.get().and_then(|o| o.update).map(|u| u.version);
    if known != update.as_ref().map(|u| u.version.clone()) {
        if let Some(u) = &update {
            tracing::info!("a new kmrs release is available: {} ({})", u.version, u.url);
        }
    }
    latest.set(Some(CheckOutcome {
        checked_at: komga_core::time_codec::now_utc(),
        update,
    }));
}

/// The latest stable release strictly newer than `current`. GitHub returns newest first,
/// so the first entry that is not a GitHub prerelease and carries a semver tag decides —
/// entries behind it are never considered.
fn select_update<'r>(
    releases: &'r [GithubReleaseDto],
    current: &semver::Version,
) -> Option<(semver::Version, &'r GithubReleaseDto)> {
    let (version, release) = releases.iter().find_map(|r| {
        if r.prerelease {
            return None;
        }
        let version =
            semver::Version::parse(r.tag_name.strip_prefix('v').unwrap_or(&r.tag_name)).ok()?;
        Some((version, r))
    })?;
    (version > *current).then_some((version, release))
}

/// GitHub 403s requests without a User-Agent
pub(crate) fn github_client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(concat!("kmrs/", env!("CARGO_PKG_VERSION")))
        .build()
        .expect("reqwest client")
}

pub(crate) async fn fetch_releases(
    http: &reqwest::Client,
    base_url: &str,
) -> Result<Vec<GithubReleaseDto>, reqwest::Error> {
    http.get(base_url)
        .query(&[("per_page", "20")])
        .send()
        .await?
        .error_for_status()?
        .json::<Vec<GithubReleaseDto>>()
        .await
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct GithubReleaseDto {
    pub html_url: String,
    pub tag_name: String,
    #[serde(with = "time::serde::iso8601")]
    pub published_at: OffsetDateTime,
    pub body: String,
    pub prerelease: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::releases::tests::serve;

    #[test]
    fn a_new_version_is_announced_once() {
        let latest = LatestUpdate::default();
        let mut found = latest.subscribe_found();
        let outcome = |version: Option<&str>| CheckOutcome {
            checked_at: komga_core::time_codec::now_utc(),
            update: version.map(|v| UpdateInfo {
                version: v.into(),
                url: String::new(),
                release_date: komga_core::time_codec::now_utc(),
            }),
        };
        latest.set(Some(outcome(Some("9.9.9"))));
        assert!(found.has_changed().unwrap());
        assert_eq!(found.borrow_and_update().as_deref(), Some("9.9.9"));
        // the daily re-check of the same release stays quiet
        latest.set(Some(outcome(Some("9.9.9"))));
        assert!(!found.has_changed().unwrap());
        latest.set(Some(outcome(None)));
        assert!(found.has_changed().unwrap());
        assert_eq!(*found.borrow_and_update(), None);
    }

    fn release(tag: &str, prerelease: bool) -> GithubReleaseDto {
        GithubReleaseDto {
            html_url: format!("https://github.com/kmworks/kmrs/releases/tag/{tag}"),
            tag_name: tag.to_string(),
            published_at: OffsetDateTime::UNIX_EPOCH,
            body: String::new(),
            prerelease,
        }
    }

    fn releases_json(tags: &[(&str, bool)]) -> &'static str {
        let entries = tags
            .iter()
            .map(|(tag, prerelease)| {
                format!(
                    r#"{{"html_url": "https://github.com/kmworks/kmrs/releases/tag/{tag}", "tag_name": "{tag}", "published_at": "2026-10-15T12:34:56Z", "body": "", "prerelease": {prerelease}}}"#
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        Box::leak(format!("[{entries}]").into_boxed_str())
    }

    #[test]
    fn select_update_picks_first_qualifying_stable() {
        let current = semver::Version::parse("0.5.0").unwrap();
        let cases: Vec<(Vec<GithubReleaseDto>, Option<&str>)> = vec![
            (vec![], None),
            (vec![release("v0.6.0", false)], Some("0.6.0")),
            // a GitHub prerelease is skipped; an older stable behind it still qualifies
            (
                vec![release("v0.6.0-beta.1", true), release("v0.5.1", false)],
                Some("0.5.1"),
            ),
            (vec![release("v0.6.0-beta.1", true)], None),
            // equal and older versions are not updates
            (vec![release("v0.5.0", false)], None),
            (vec![release("v0.4.9", false)], None),
            // non-semver tags are skipped
            (
                vec![release("nightly", false), release("v0.6.0", false)],
                Some("0.6.0"),
            ),
            // the first qualifying entry decides, even when a later one is newer
            (
                vec![release("v0.4.0", false), release("v0.6.0", false)],
                None,
            ),
            // tags without the v prefix parse too
            (vec![release("0.6.0", false)], Some("0.6.0")),
        ];
        for (releases, expected) in cases {
            let selected = select_update(&releases, &current).map(|(v, _)| v.to_string());
            assert_eq!(selected.as_deref(), expected, "{releases:?}");
        }
    }

    #[test]
    fn update_info_serializes_camel_case_with_zoned_timestamps() {
        let info = UpdateInfo {
            version: "0.7.0".into(),
            url: "https://github.com/kmworks/kmrs/releases/tag/v0.7.0".into(),
            release_date: time::macros::datetime!(2026-10-15 12:34:56 UTC),
        };
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "version": "0.7.0",
                "url": "https://github.com/kmworks/kmrs/releases/tag/v0.7.0",
                "releaseDate": "2026-10-15T12:34:56Z",
            })
        );
    }

    #[tokio::test]
    async fn check_once_sets_newer_stable() {
        let latest = LatestUpdate::default();
        let url = serve(releases_json(&[("v99.0.0", false)])).await;
        check_once(&latest, &url).await;
        let outcome = latest.get().expect("outcome stored");
        let update = outcome.update.expect("update stored");
        assert_eq!(update.version, "99.0.0");
        assert_eq!(
            update.url,
            "https://github.com/kmworks/kmrs/releases/tag/v99.0.0"
        );
        assert_eq!(update.release_date.year(), 2026);
        assert!(
            komga_core::time_codec::now_utc() - outcome.checked_at < time::Duration::minutes(1)
        );
    }

    #[tokio::test]
    async fn check_once_skips_prerelease_for_older_stable() {
        let latest = LatestUpdate::default();
        let url = serve(releases_json(&[("v99.0.0", true), ("v98.0.0", false)])).await;
        check_once(&latest, &url).await;
        assert_eq!(
            latest
                .get()
                .and_then(|o| o.update)
                .expect("update stored")
                .version,
            "98.0.0"
        );
    }

    #[tokio::test]
    async fn check_once_stores_none_without_a_newer_stable() {
        let equal: &'static str =
            Box::leak(format!("v{}", env!("CARGO_PKG_VERSION")).into_boxed_str());
        for body in [
            releases_json(&[("v99.0.0", true)]),
            releases_json(&[("v0.0.1", false)]),
            releases_json(&[(equal, false)]),
            releases_json(&[("nightly", false)]),
        ] {
            let latest = LatestUpdate::default();
            let url = serve(body).await;
            check_once(&latest, &url).await;
            let outcome = latest.get().expect("outcome stored");
            assert!(outcome.update.is_none(), "{body}");
        }
    }

    #[tokio::test]
    async fn check_once_failure_keeps_previous_state() {
        let latest = LatestUpdate::default();
        latest.set(Some(CheckOutcome {
            checked_at: OffsetDateTime::UNIX_EPOCH,
            update: Some(UpdateInfo {
                version: "1.2.3".into(),
                url: "https://example.com".into(),
                release_date: OffsetDateTime::UNIX_EPOCH,
            }),
        }));
        check_once(&latest, "http://127.0.0.1:1/releases").await;
        let outcome = latest.get().expect("state kept");
        assert_eq!(outcome.update.expect("update kept").version, "1.2.3");
        assert_eq!(outcome.checked_at, OffsetDateTime::UNIX_EPOCH);
    }
}
