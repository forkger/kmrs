// windows subsystem in release: no console window is ever created and the server
// surfaces through the tray icon (src/tray.rs); debug builds keep the console so
// `cargo run` retains log output
#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

mod api;
mod auth;
mod config;
#[cfg(any(target_os = "macos", target_os = "windows"))]
mod desktop;
mod dto;
mod error;
mod events;
mod http;
mod logging;
#[cfg(all(feature = "profiling", unix))]
mod profiling;
mod search_index;
mod service;
mod settings;
mod sse;
mod state;
mod thumbnails;
#[cfg(target_os = "windows")]
mod tray;
#[cfg(target_os = "macos")]
#[path = "tray_macos.rs"]
mod tray;
mod webhook;
#[allow(dead_code)]
mod webpub;
mod webui;
mod zip_archive;

use anyhow::Context;
use clap::Parser;
use komga_db::pool::Database;
use komga_db::Migrator;
use state::AppState;
use std::sync::Arc;
use tower_http::trace::TraceLayer;

/// C libraries allocate through glibc malloc only in builds without the profiling
/// feature (profiling overrides the allocator process-wide). glibc's per-thread
/// arenas (8×cores by default) only ever return the heap top to the OS: transient
/// allocations during scans and queries linger as touched-but-free pages scattered
/// across dozens of heaps. Two arenas bound that retention; MALLOC_ARENA_MAX remains
/// the override channel.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn cap_glibc_arenas() {
    if std::env::var_os("MALLOC_ARENA_MAX").is_none() {
        unsafe { libc::mallopt(libc::M_ARENA_MAX, 2) };
    }
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn cap_glibc_arenas() {}

#[cfg(not(target_os = "macos"))]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let result = run().await;
    #[cfg(target_os = "windows")]
    if let Err(e) = &result {
        tray::show_startup_error(e);
    }
    result
}

// AppKit owns the main thread in a GUI session (src/tray_macos.rs), so the runtime
// is driven from whichever thread run_app hands the server to
#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    if tray::hand_off_to_login_item() {
        return Ok(());
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        // a failed runtime build is a startup error too: under the login item
        // it must not relaunch into the same resource exhaustion every 30s
        Err(e) => {
            let error = anyhow::Error::new(e).context("build the async runtime");
            tray::show_startup_error(&error);
            if tray::startup_failure_exits_cleanly() {
                std::process::exit(0);
            }
            return Err(error);
        }
    };
    // under the login item a panic before the listener is up is a startup error
    // like any other; elsewhere, and once serving, it keeps unwinding so the
    // process exits as crashed
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        tray::run_app(move || runtime.block_on(run()))
    }));
    let result = match outcome {
        Ok(result) => result,
        Err(panic) if tray::startup_failure_exits_cleanly() => Err(anyhow::anyhow!(
            "unexpected error: {}",
            panic_message(panic.as_ref())
        )),
        Err(panic) => std::panic::resume_unwind(panic),
    };
    if let Err(e) = &result {
        // a failure after serving is a crash for launchd to restart: a modal
        // alert would hold the process down until someone dismisses it
        if !tray::serving() && !tray::duplicate_of_running_server(e) {
            tray::show_startup_error(e);
        }
        // after the modal alert returns, so the process stays up while it shows
        if tray::startup_failure_exits_cleanly() {
            std::process::exit(0);
        }
    }
    result
}

#[cfg(target_os = "macos")]
fn panic_message(payload: &(dyn std::any::Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("the server panicked")
}

async fn run() -> anyhow::Result<()> {
    cap_glibc_arenas();
    // before Cli::parse, so --help/--version and parse errors print to the
    // invoking terminal despite the windows subsystem
    #[cfg(target_os = "windows")]
    tray::attach_parent_console();
    let cli = config::Cli::parse();
    // Start time must be recorded now, not lazily on the first metrics request,
    // or process.uptime starts counting from that request.
    service::metrics::process_start();
    let logs_dir = config::config_dir(&cli).join("logs");
    let logging = logging::init(&logs_dir);

    #[cfg(all(feature = "profiling", unix))]
    profiling::spawn_heap_dump_listener();

    let config = config::ServerConfig::load(&cli)?;
    logging.apply_config_level(config.log_level.as_deref());
    std::fs::create_dir_all(&config.config_dir).context("create config dir")?;

    // Sort locale must be set before any collator is constructed (lazily on the
    // first connection collation registration); later calls are ignored.
    komga_core::sort_locale::set_sort_locale(config.sort_locale.clone());

    // opening the main database creates the file, so its absence beforehand marks
    // the very first start
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    let first_run = !config.database.file.exists();
    let db = Database::open(&config.database).context("open main database")?;
    // Dedicated pools over the same file for background task execution; task
    // reads/writes never share pool slots with HTTP/API requests. The task side,
    // tasks queue and kmrs database are low-concurrency auxiliary pools.
    let task_db =
        Database::open(&config.database.aux_pools()).context("open task database pools")?;
    let tasks_db = Database::open(&config.tasks_db.aux_pools()).context("open tasks database")?;
    let kmrs_db = Database::open(&config.kmrs_db.aux_pools()).context("open kmrs database")?;
    // Spring Session JDBC store: like the Java datasource and its Flyway
    // initializer, only exists when komga.jdbc-session-enabled=true, and
    // sessions are then persisted in sessions.sqlite (same row layout as Java).
    let sessions_db = if config.jdbc_session_enabled {
        let sessions_db =
            Database::open(&config.sessions_db.aux_pools()).context("open sessions database")?;
        Migrator::new(
            &komga_db::sessions_migrations(),
            config.migration_placeholders.clone(),
        )
        .migrate(&*sessions_db.rw()?)
        .context("sessions db migration")?;
        Some(sessions_db)
    } else {
        None
    };

    {
        let migrations = komga_db::main_migrations();
        let applied = Migrator::new(&migrations, config.migration_placeholders.clone())
            .migrate(&*db.rw()?)
            .context("main db migration")?;
        if applied > 0 {
            tracing::info!("applied {applied} main db migrations");
        }
        let tasks_migrations = komga_db::tasks_migrations();
        Migrator::new(&tasks_migrations, config.migration_placeholders.clone())
            .migrate(&*tasks_db.rw()?)
            .context("tasks db migration")?;
        let kmrs_migrations = komga_db::kmrs_migrations();
        Migrator::new(&kmrs_migrations, config.migration_placeholders.clone())
            .migrate(&*kmrs_db.rw()?)
            .context("kmrs db migration")?;
    }

    let task_notify: service::TaskNotify = std::sync::Arc::new(tokio::sync::Notify::new());
    let search_rebuild = match komga_search::decide_startup(&config.lucene_dir) {
        komga_search::StartupDecision::Ready => false,
        komga_search::StartupDecision::Rebuild { wipe } => {
            if wipe {
                tracing::info!(
                    "wiping outdated search index at {}",
                    config.lucene_dir.display()
                );
                komga_search::wipe_index_dir(&config.lucene_dir).context("wipe search index")?;
            }
            true
        }
    };
    let search_index =
        Arc::new(komga_search::SearchIndex::open(&config.lucene_dir).context("open search index")?);
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let state = AppState {
        sessions: match sessions_db {
            Some(db) => auth::SessionStore::jdbc(db, config.session_timeout),
            None => auth::SessionStore::new(config.session_timeout),
        },
        settings: Arc::new(settings::SettingsProvider::load(db.clone())),
        tsid: Arc::new(komga_core::tsid::TsidFactory::new_random_node()),
        events: events::event_bus(),
        task_emitter: Arc::new(service::TaskEmitter::new(
            db.clone(),
            tasks_db.clone(),
            task_notify.clone(),
        )),
        search_index: search_index.clone(),
        tracker: Arc::new(service::tracker::TrackerServices::from_db(kmrs_db.clone())),
        kepub: service::kepub::KepubConverter::new(service::kepub::default_tmp_dir()),
        kobo_proxy: service::kobo_proxy::KoboProxy::new(),
        shutdown_tx,
        api_key_activity_throttle: Arc::new(crate::auth::ApiKeyActivityThrottle::new()),
        latest_update: service::update_check::LatestUpdate::default(),
        db,
        task_db,
        tasks_db,
        kmrs_db,
        config: Arc::new(config.clone()),
    };

    // the task processor outlives the HTTP server and is joined after the graceful
    // shutdown signal, so claimed tasks finish instead of being dropped mid-run
    let (processor_shutdown_tx, processor_shutdown_rx) = tokio::sync::watch::channel(false);
    let processor = service::processor::TaskProcessor::start_with_shutdown(
        state.clone(),
        task_notify,
        processor_shutdown_rx,
    );
    service::scheduler::ScanScheduler::start(state.clone());
    let update_check = service::update_check::start(state.clone());
    service::maintenance::MaintenanceScheduler::start_auth_activity_cleanup(state.clone());
    service::maintenance::MaintenanceScheduler::start_history_cleanup(state.clone());
    if config.jdbc_session_enabled {
        service::maintenance::MaintenanceScheduler::start_session_cleanup(state.clone());
    }
    // Thumbnail file storage: migrate existing blobs to files in the background. The
    // orphan sweep starts only after the migration — freshly written files are
    // referenced once their row is updated, and a concurrent sweep would delete them.
    let thumbnail_migration = if config.thumbnail_storage == config::ThumbnailStorage::File {
        let state = state.clone();
        Some(tokio::task::spawn_blocking(move || {
            thumbnails::migrate_blobs_to_files(&state)
        }))
    } else {
        None
    };
    let sweep_state = state.clone();
    tokio::spawn(async move {
        if let Some(migration) = thumbnail_migration {
            if let Err(e) = migration.await {
                tracing::error!("thumbnail file-storage migration task failed: {e}");
            }
        }
        service::maintenance::MaintenanceScheduler::start_thumbnail_sweep(sweep_state);
    });
    service::komf::KomfProvisioner::start(state.clone());
    search_index::check_on_startup(&state, search_rebuild);
    search_index::consume_events(state.clone(), state.events.tap());
    webhook::consume_events(state.clone());
    service::reading_stats::consume_events(state.clone(), state.events.tap());
    service::tracker_sync::consume_events(state.clone(), state.events.tap());

    let app = build_router(state.clone());

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], config.port));
    tracing::info!("kmrs listening on {addr}");
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind {addr}"))?;
    #[cfg(target_os = "macos")]
    tray::mark_serving();
    // startup output had its chance to reach an invoking terminal; serving must
    // not die with a terminal the user closes later
    #[cfg(target_os = "windows")]
    {
        tray::detach_console();
    }
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    let tray = tray::start(
        state.config.port,
        tray::TrayPaths {
            config_dir: state.config.config_dir.clone(),
            logs_dir,
        },
        state.shutdown_tx.clone(),
        desktop::update_handle(),
    );
    // the initial admin claim happens in the web UI; only in an interactive
    // session (tray present) is there a desktop to show it on
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    if first_run && tray.is_some() {
        tray::open_in_browser(state.config.port);
    }
    let serve_result = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal(shutdown_rx))
    .await;
    // the tray stands for a serving server: it goes as soon as serving ends, on
    // the error path too, not after the task drain below
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    if let Some(tray) = tray {
        tray.stop();
    }
    serve_result?;

    update_check.abort();
    // stop the supervisor between drains, then give the claimed tasks a bounded grace
    // period before aborting (a hung task must not prevent the process from exiting)
    drop(processor_shutdown_tx);
    if tokio::time::timeout(std::time::Duration::from_secs(30), processor)
        .await
        .is_err()
    {
        tracing::warn!("task processor did not finish within 30s, aborting");
    }
    drop(update_check);
    drop(state);
    Ok(())
}

pub fn build_router(state: AppState) -> axum::Router {
    let routes = axum::Router::new()
        .merge(api::claim::router())
        .merge(api::login::router())
        .merge(api::oauth2::router())
        .merge(api::users::router())
        .merge(api::libraries::router())
        .merge(api::referential::router())
        .merge(api::series::router())
        .merge(api::books::router())
        .merge(api::collections::router())
        .merge(api::readlists::router())
        .merge(api::smartlists::router())
        .merge(api::tasks::router())
        .merge(api::komf::router())
        .merge(api::tracker::router())
        .merge(api::opds_v1::router())
        .merge(api::opds_v2::router())
        .merge(api::openapi::router())
        .merge(api::kobo::router())
        .merge(api::koreader::router())
        .merge(api::syncpoints::router())
        .merge(api::page_hashes::router())
        .merge(api::transient_books::router())
        .merge(api::settings::router())
        .merge(api::client_settings::router())
        .merge(api::history::router())
        .merge(api::stats::router())
        .merge(api::server::router())
        .merge(api::announcements::router())
        .merge(api::releases::router())
        .merge(api::filesystem::router())
        .merge(api::fonts::router())
        .merge(api::actuator::router())
        .merge(sse::router());

    #[cfg(all(feature = "profiling", unix))]
    let routes = routes.merge(profiling::router());

    let app = routes
        .fallback(webui::fallback)
        .layer(axum::middleware::from_fn(
            http::error_path::error_path_middleware,
        ))
        .layer(axum::middleware::from_fn(http::etag::etag_middleware))
        .layer(axum::middleware::from_fn(
            http::cache::cache_control_middleware,
        ))
        .layer(axum::middleware::from_fn(
            http::www_authenticate::strip_challenge_for_xhr,
        ))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::auth_middleware,
        ))
        .layer(TraceLayer::new_for_http())
        .layer(axum::middleware::from_fn(http::offload::offload_middleware));

    // outermost: preflight OPTIONS must be answered before the auth middleware runs
    let app = match http::cors::layer(&state.config.cors_allowed_origins) {
        Some(cors) => app.layer(cors),
        None => app,
    };

    app.with_state(state)
}

async fn shutdown_signal(mut shutdown_rx: tokio::sync::watch::Receiver<bool>) {
    // launchd stops a login item with SIGTERM: at logout, and when Open at Login
    // is turned off while the login item runs the server
    #[cfg(target_os = "macos")]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(e) => {
                tracing::warn!("SIGTERM handler unavailable: {e}");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(target_os = "macos"))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {},
        _ = terminate => {},
        _ = shutdown_rx.changed() => {},
    }
    tracing::info!("shutting down");
}
