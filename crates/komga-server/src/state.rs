//! Application state.

use crate::auth::SessionStore;
use crate::config::ServerConfig;
use crate::settings::SettingsProvider;
use komga_core::tsid::TsidFactory;
use komga_db::pool::Database;
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<ServerConfig>,
    pub db: Database,
    /// Dedicated connection pools over the same `database.sqlite` file, used only
    /// by background task execution so it never contends with API connections.
    pub task_db: Database,
    pub tasks_db: Database,
    /// kmrs-private state (`kmrs.sqlite`): per-book series metadata contributions,
    /// komf integration, thumbnail storage migration state, reading events,
    /// smart lists; never touches the main database.
    pub kmrs_db: Database,
    pub sessions: SessionStore,
    pub settings: Arc<SettingsProvider>,
    pub tsid: Arc<TsidFactory>,
    pub events: crate::events::EventBus,
    pub task_emitter: Arc<crate::service::TaskEmitter>,
    pub search_index: Arc<komga_search::SearchIndex>,
    pub tracker: Arc<crate::service::tracker::TrackerServices>,
    pub kepub: Arc<crate::service::kepub::KepubConverter>,
    pub kobo_proxy: Arc<crate::service::kobo_proxy::KoboProxy>,
    /// Broadcasts shutdown: the tray's quit action and desktop updates request it, and it is
    /// set once shutdown starts for any reason so open SSE streams can end
    /// (graceful shutdown waits for them)
    pub shutdown_tx: tokio::sync::watch::Sender<bool>,
    pub api_key_activity_throttle: Arc<crate::auth::ApiKeyActivityThrottle>,
    pub(crate) user_auth: Arc<crate::auth::UserAuthentication>,
    pub latest_update: crate::service::update_check::LatestUpdate,
}

/// A throwaway tantivy index for tests (one fresh directory per call).
#[cfg(test)]
pub(crate) fn test_search_index() -> Arc<komga_search::SearchIndex> {
    Arc::new(
        komga_search::SearchIndex::open(&tempfile::tempdir().unwrap().keep())
            .expect("test search index"),
    )
}

/// Without a subscriber the refresh degradation warnings never reach nextest's
/// failure output, leaving flaky failures undiagnosable.
#[cfg(test)]
pub(crate) fn init_test_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .try_init();
}

/// An in-memory `kmrs.sqlite` with the kmrs migrations applied, for tests.
#[cfg(test)]
pub(crate) fn test_kmrs_db() -> Database {
    let kmrs_db = Database::open_in_memory(false).unwrap();
    komga_db::Migrator::new(
        &komga_db::kmrs_migrations(),
        komga_db::Placeholders::default(),
    )
    .migrate(&kmrs_db.rw().unwrap())
    .unwrap();
    kmrs_db
}

#[cfg(test)]
pub(crate) fn test_app_state(
    config: ServerConfig,
    db: Database,
    tasks_db: Database,
    kmrs_db: Database,
) -> (AppState, tokio::sync::watch::Receiver<bool>) {
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let sessions = SessionStore::new(config.session_timeout);
    let state = AppState {
        config: Arc::new(config.clone()),
        db: db.clone(),
        // In-memory task pools share the main database so background writes are visible to assertions.
        task_db: db.clone(),
        tasks_db: tasks_db.clone(),
        kmrs_db: kmrs_db.clone(),
        sessions: sessions.clone(),
        user_auth: Arc::new(crate::auth::UserAuthentication::new(db.clone(), sessions)),
        settings: Arc::new(SettingsProvider::load(db.clone())),
        tsid: Arc::new(TsidFactory::new_random_node()),
        events: crate::events::event_bus(),
        task_emitter: Arc::new(crate::service::TaskEmitter::new(
            db,
            tasks_db,
            Arc::new(tokio::sync::Notify::new()),
        )),
        search_index: test_search_index(),
        tracker: Arc::new(crate::service::tracker::TrackerServices::empty(
            kmrs_db.clone(),
        )),
        kepub: crate::service::kepub::KepubConverter::new(tempfile::tempdir().unwrap().keep()),
        kobo_proxy: crate::service::kobo_proxy::KoboProxy::new(),
        shutdown_tx,
        api_key_activity_throttle: Arc::new(crate::auth::ApiKeyActivityThrottle::new()),
        latest_update: Default::default(),
    };
    (state, shutdown_rx)
}

/// Re-points a test state at a fresh temp config dir with the given thumbnail storage.
#[cfg(test)]
pub(crate) fn test_state_with_thumbnail_storage(
    state: AppState,
    storage: crate::config::ThumbnailStorage,
) -> AppState {
    let config_dir = tempfile::tempdir().unwrap().keep();
    let config = Arc::new(crate::config::ServerConfig {
        config_dir,
        thumbnail_storage: storage,
        ..(*state.config).clone()
    });
    AppState { config, ..state }
}

/// A test state with `[libraries] allow-file-deletion = false`.
#[cfg(test)]
pub(crate) fn test_state_without_file_deletion(state: AppState) -> AppState {
    let config = Arc::new(crate::config::ServerConfig {
        allow_file_deletion: false,
        ..(*state.config).clone()
    });
    AppState { config, ..state }
}

impl AppState {
    /// A clone of the state whose `db` points at the dedicated task pools.
    /// Background task execution (scan / analyze / hash / convert / maintenance)
    /// runs against this context, so its reads and writes never share pool slots
    /// with HTTP/API requests. The task emitter is re-pointed at the task pools
    /// as well — its follow-up candidate queries (e.g. `analyze_unknown_and_outdated_books`,
    /// `hash_books_without_hash`) are the heaviest scan-time reads. The task queue
    /// (`tasks_db`), event bus and settings are shared unchanged.
    pub fn task_context(&self) -> Self {
        Self {
            db: self.task_db.clone(),
            task_emitter: Arc::new(self.task_emitter.with_db(self.task_db.clone())),
            ..self.clone()
        }
    }

    /// Records authentication activity (success/failure); persisted asynchronously without blocking the request.
    pub async fn record_activity(
        &self,
        activity: &Option<crate::auth::ActivityDraft>,
        parts: &axum::http::request::Parts,
    ) {
        let Some(draft) = activity else { return };
        let ip = crate::http::client_addr::client_ip(parts, &self.config.trusted_proxies)
            .map(|ip| ip.to_string());
        let user_agent = parts
            .headers
            .get(axum::http::header::USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        if draft.success {
            // API keys throttle per key; password logins per user and device, so a login
            // from a new device is still recorded right away (key ids are UUIDs, so the
            // prefixed password keys can't collide with them)
            let throttle_key = match draft.source.as_str() {
                crate::auth::SOURCE_API_KEY => draft.api_key_id.clone(),
                crate::auth::SOURCE_PASSWORD => draft.user_id.as_ref().map(|user_id| {
                    format!(
                        "password:{user_id}:{}:{}",
                        ip.as_deref().unwrap_or_default(),
                        user_agent.as_deref().unwrap_or_default()
                    )
                }),
                _ => None,
            };
            if let Some(key) = throttle_key {
                if !self
                    .api_key_activity_throttle
                    .should_record(&key, std::time::Instant::now())
                {
                    return;
                }
            }
        }
        let db = self.db.clone();
        let draft = crate::auth::ActivityDraft {
            user_id: draft.user_id.clone(),
            email: draft.email.clone(),
            api_key_id: draft.api_key_id.clone(),
            api_key_comment: draft.api_key_comment.clone(),
            success: draft.success,
            error: draft.error.clone(),
            source: draft.source.clone(),
        };
        tokio::task::spawn_blocking(move || {
            let activity = komga_core::model::user::AuthenticationActivity {
                user_id: draft.user_id,
                email: draft.email,
                api_key_id: draft.api_key_id,
                api_key_comment: draft.api_key_comment,
                ip,
                user_agent,
                success: draft.success,
                error: draft.error,
                date_time: komga_core::time_codec::now_utc(),
                source: Some(draft.source),
            };
            if let Err(e) = komga_db::dao::user::UserDao::new(db).insert_activity(&activity) {
                tracing::warn!("failed to record authentication activity: {e}");
            }
        });
    }
}
