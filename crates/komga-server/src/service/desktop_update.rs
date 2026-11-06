//! Release downloads for desktop installations. Verification finishes before a
//! platform-specific helper is allowed to replace the running installation.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use serde::Deserialize;
use sha2::{Digest, Sha256};

const RELEASES_URL: &str = "https://api.github.com/repos/kmworks/kmrs/releases/latest";
const MAX_ASSET_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Status {
    #[default]
    Idle,
    Checking,
    Available(String),
    Downloading(String),
    Installing(String),
    Failed(String),
}

#[derive(Clone)]
pub struct DesktopUpdate {
    state: Arc<Mutex<State>>,
    runtime: tokio::runtime::Handle,
    // status changes happen on background threads; the tray follows them here
    changed: Arc<tokio::sync::watch::Sender<Status>>,
}

#[derive(Default)]
struct State {
    status: Status,
    selected: Option<Asset>,
    // the newer release the daily update check found
    announced: Option<String>,
}

impl DesktopUpdate {
    pub fn new(runtime: tokio::runtime::Handle) -> Self {
        Self {
            state: Arc::new(Mutex::new(State::default())),
            runtime,
            changed: Arc::new(tokio::sync::watch::channel(Status::default()).0),
        }
    }

    /// The current status now and after every change.
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<Status> {
        self.changed.subscribe()
    }

    fn publish(&self, status: &Status) {
        self.changed.send_replace(status.clone());
    }

    pub fn status(&self) -> Status {
        self.state.lock().unwrap().status.clone()
    }

    pub fn start_check(&self) -> Result<(), String> {
        self.begin_check()?;
        let this = self.clone();
        self.runtime.spawn(async move {
            if let Err(e) = check(&this).await {
                tracing::warn!("update check failed: {e}");
            }
        });
        Ok(())
    }

    /// Checks and waits for the result, for a retry that goes on to install. Call it off
    /// the runtime's threads, like `download_verified`.
    pub fn check_blocking(&self) -> Result<(), String> {
        self.begin_check()?;
        self.runtime.block_on(check(self)).map(|_| ())
    }

    fn begin_check(&self) -> Result<(), String> {
        {
            let mut state = self.state.lock().unwrap();
            if !matches!(
                state.status,
                Status::Idle | Status::Available(_) | Status::Failed(_)
            ) {
                return Err("an update is already in progress".into());
            }
            state.status = Status::Checking;
        }
        self.publish(&Status::Checking);
        Ok(())
    }

    fn set(&self, status: Status) {
        self.state.lock().unwrap().status = status.clone();
        self.publish(&status);
    }

    /// Records the newer release the daily update check found, if any, which a failed
    /// desktop check (such as a release without checksums) cannot tell on its own.
    pub fn announce(&self, version: Option<String>) {
        let mut state = self.state.lock().unwrap();
        state.announced = version;
        // the badge and the menu depend on it, so the tray refreshes even when the
        // status stays
        self.changed.send_replace(state.status.clone());
    }

    /// Whether a newer release is known, from either check.
    pub fn pending(&self) -> bool {
        self.known_version().is_some()
    }

    /// The newer release known from either check; the desktop check's pick wins.
    pub fn known_version(&self) -> Option<String> {
        let state = self.state.lock().unwrap();
        let selected = state.selected.as_ref().map(|asset| asset.version.clone());
        selected.or_else(|| state.announced.clone())
    }

    pub fn set_failed(&self, error: String) {
        self.set(Status::Failed(error));
    }

    pub fn installing(&self) {
        let mut state = self.state.lock().unwrap();
        if let Status::Downloading(version) = &state.status {
            state.status = Status::Installing(version.clone());
            self.publish(&state.status);
        }
    }

    fn begin_download(&self) -> Result<Asset, String> {
        let mut state = self.state.lock().unwrap();
        if !matches!(state.status, Status::Available(_)) {
            return Err("no checked update is ready".into());
        }
        let asset = state.selected.clone().ok_or("no checked update is ready")?;
        state.status = Status::Downloading(asset.version.clone());
        self.publish(&state.status);
        Ok(asset)
    }

    pub fn download_verified(&self, directory: &Path) -> Result<DownloadedUpdate, String> {
        let asset = match self.begin_download() {
            Ok(asset) => asset,
            Err(e) => {
                self.set_failed(e.clone());
                return Err(e);
            }
        };
        let path = directory.join(&asset.name);
        match self.runtime.block_on(download_asset(&asset, &path)) {
            Ok(()) => Ok(DownloadedUpdate { path }),
            Err(e) => {
                self.set(Status::Failed(e.clone()));
                Err(e)
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Asset {
    pub version: String,
    pub name: String,
    pub url: String,
    pub sha256: [u8; 32],
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    assets: Vec<ReleaseAsset>,
}

#[derive(Deserialize)]
struct ReleaseAsset {
    name: String,
    browser_download_url: String,
}

pub async fn check(state: &DesktopUpdate) -> Result<Option<Asset>, String> {
    match latest_asset().await {
        Ok(Some(asset)) => {
            state.state.lock().unwrap().selected = Some(asset.clone());
            state.set(Status::Available(asset.version.clone()));
            Ok(Some(asset))
        }
        Ok(None) => {
            state.state.lock().unwrap().selected = None;
            state.set(Status::Idle);
            Ok(None)
        }
        Err(e) => {
            state.set(Status::Failed(e.clone()));
            Err(e)
        }
    }
}

async fn latest_asset() -> Result<Option<Asset>, String> {
    let client = super::update_check::github_client();
    let release: Release = client
        .get(RELEASES_URL)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    let version = semver::Version::parse(release.tag_name.trim_start_matches('v'))
        .map_err(|e| e.to_string())?;
    let current = semver::Version::parse(env!("CARGO_PKG_VERSION")).unwrap();
    if version <= current {
        return Ok(None);
    }
    let Some(asset) = release
        .assets
        .iter()
        .find(|asset| asset.name == asset_name())
    else {
        return Ok(None);
    };
    let sums = release
        .assets
        .iter()
        .find(|asset| asset.name == "SHA256SUMS")
        .ok_or("release has no SHA256SUMS")?;
    let text = client
        .get(&sums.browser_download_url)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?
        .text()
        .await
        .map_err(|e| e.to_string())?;
    let sha256 = checksum_for(&text, &asset.name)?;
    Ok(Some(Asset {
        version: version.to_string(),
        name: asset.name.clone(),
        url: asset.browser_download_url.clone(),
        sha256,
    }))
}

#[cfg(target_os = "macos")]
fn asset_name() -> &'static str {
    "KMServer.zip"
}

#[cfg(target_os = "windows")]
fn asset_name() -> &'static str {
    "KMServer_x64-setup.exe"
}

fn checksum_for(text: &str, name: &str) -> Result<[u8; 32], String> {
    let mut found = None;
    for line in text.lines() {
        let Some((digest, file)) = line.split_once(char::is_whitespace) else {
            return Err("invalid SHA256SUMS line".into());
        };
        if file.trim_start() != name {
            continue;
        }
        if found.is_some() {
            return Err("duplicate asset checksum".into());
        }
        let bytes = hex::decode(digest).map_err(|_| "invalid asset checksum".to_string())?;
        found = Some(
            <[u8; 32]>::try_from(bytes.as_slice())
                .map_err(|_| "invalid asset checksum length".to_string())?,
        );
    }
    found.ok_or_else(|| format!("release has no checksum for {name}"))
}

pub struct DownloadedUpdate {
    pub path: PathBuf,
}

async fn download_asset(asset: &Asset, path: &Path) -> Result<(), String> {
    let response = super::update_check::github_client()
        .get(&asset.url)
        .timeout(Duration::from_secs(120))
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?;
    if response
        .content_length()
        .is_some_and(|size| size > MAX_ASSET_BYTES)
    {
        return Err("update is too large".into());
    }
    let mut file = std::fs::File::create(path).map_err(|e| e.to_string())?;
    let mut stream = response.bytes_stream();
    let mut hash = Sha256::new();
    let mut size = 0u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| e.to_string())?;
        size += chunk.len() as u64;
        if size > MAX_ASSET_BYTES {
            return Err("update is too large".into());
        }
        std::io::Write::write_all(&mut file, &chunk).map_err(|e| e.to_string())?;
        hash.update(&chunk);
    }
    file.sync_all().map_err(|e| e.to_string())?;
    if hash.finalize().as_slice() != asset.sha256 {
        return Err("update checksum does not match the release".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn status_changes_reach_subscribers() {
        let update = DesktopUpdate::new(tokio::runtime::Handle::current());
        let mut changes = update.subscribe();
        update.set(Status::Available("99.0.0".into()));
        assert!(changes.has_changed().unwrap());
        assert_eq!(
            *changes.borrow_and_update(),
            Status::Available("99.0.0".into())
        );
    }

    #[tokio::test]
    async fn an_announced_release_stays_pending_after_a_failed_check() {
        let update = DesktopUpdate::new(tokio::runtime::Handle::current());
        let mut changes = update.subscribe();
        update.set_failed("offline".into());
        assert!(!update.pending());
        changes.borrow_and_update();
        update.announce(Some("99.0.0".into()));
        assert!(changes.has_changed().unwrap());
        assert!(update.pending());
        assert_eq!(update.known_version().as_deref(), Some("99.0.0"));
        update.announce(None);
        assert!(!update.pending());
    }

    #[tokio::test]
    async fn second_download_cannot_interrupt_an_active_installation() {
        let update = DesktopUpdate::new(tokio::runtime::Handle::current());
        update.state.lock().unwrap().selected = Some(Asset {
            version: "99.0.0".into(),
            name: "KMServer.zip".into(),
            url: "https://example.invalid/KMServer.zip".into(),
            sha256: [0; 32],
        });
        update.set(Status::Available("99.0.0".into()));
        update.begin_download().unwrap();
        update.installing();
        assert!(update.begin_download().is_err());
        assert_eq!(update.status(), Status::Installing("99.0.0".into()));
    }

    #[tokio::test]
    async fn preparation_failure_leaves_update_retryable() {
        let update = DesktopUpdate::new(tokio::runtime::Handle::current());
        update.state.lock().unwrap().selected = Some(Asset {
            version: "99.0.0".into(),
            name: "KMServer.zip".into(),
            url: "https://example.invalid/KMServer.zip".into(),
            sha256: [0; 32],
        });
        update.set(Status::Available("99.0.0".into()));
        update.begin_download().unwrap();
        update.set_failed("signature verification failed".into());
        assert_eq!(
            update.status(),
            Status::Failed("signature verification failed".into())
        );
    }

    #[test]
    fn checksum_selects_the_named_asset() {
        let text = format!(
            "{}  KMServer.zip\n{}  KMServer_x64-setup.exe\n",
            "ab".repeat(32),
            "cd".repeat(32)
        );
        assert_eq!(checksum_for(&text, "KMServer.zip").unwrap(), [0xab; 32]);
        assert!(checksum_for(&text, "KMServer.zip").unwrap() != [0xcd; 32]);
        assert!(checksum_for(&text, "other").is_err());
    }

    #[tokio::test]
    async fn bad_download_does_not_become_installable() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/KMServer.zip", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut request = [0; 1024];
            let _ = socket.read(&mut request).await;
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\nbad")
                .await
                .unwrap();
        });
        let asset = Asset {
            version: "99.0.0".into(),
            name: "KMServer.zip".into(),
            url,
            sha256: [0; 32],
        };
        let directory = tempfile::tempdir().unwrap();
        let result = download_asset(&asset, &directory.path().join(&asset.name)).await;
        println!("result: {result:?}");
        assert!(result.unwrap_err().contains("checksum"));
    }

    #[test]
    fn checksum_rejects_duplicates_and_malformed_lines() {
        let digest = "ab".repeat(32);
        assert!(checksum_for(
            &format!("{digest} KMServer.zip\n{digest} KMServer.zip\n"),
            "KMServer.zip"
        )
        .is_err());
        assert!(checksum_for("not-a-digest KMServer.zip", "KMServer.zip").is_err());
        assert!(checksum_for("invalid", "KMServer.zip").is_err());
    }
}
