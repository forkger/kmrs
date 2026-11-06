//! Equivalent of the read part of `LibraryController`: `GET /api/v1/libraries[/{libraryId}]`.

use crate::auth::RequireAuth;
use crate::dto::library::{LibraryCreationDto, LibraryUpdateDto};
use crate::error::{ApiError, Violation};
use crate::state::AppState;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::{routing, Json, Router};
use komga_core::dto::library::LibraryDto;
use komga_core::model::library::Library;
use komga_core::task::{BookMetadataPatchCapability, HIGHEST_PRIORITY, HIGH_PRIORITY};
use komga_core::time_codec::now_utc;
use komga_db::dao::book::BookDao;
use komga_db::dao::library::LibraryDao;
use komga_db::dao::series::SeriesDao;
use komga_media::scanner::path_to_url;
use std::path::Path as FsPath;

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/api/v1/libraries",
            routing::get(get_libraries).post(add_library),
        )
        .route(
            "/api/v1/libraries/{libraryId}",
            routing::get(get_library_by_id)
                .put(update_library_by_id)
                .patch(update_library_by_id)
                .delete(delete_library_by_id),
        )
        .route(
            "/api/v1/libraries/{libraryId}/scan",
            routing::post(library_scan),
        )
        .route(
            "/api/v1/libraries/{libraryId}/analyze",
            routing::post(library_analyze),
        )
        .route(
            "/api/v1/libraries/{libraryId}/metadata/refresh",
            routing::post(library_refresh_metadata),
        )
        .route(
            "/api/v1/libraries/{libraryId}/empty-trash",
            routing::post(library_empty_trash),
        )
}

async fn get_libraries(
    State(state): State<AppState>,
    auth: RequireAuth,
) -> Result<Json<Vec<LibraryDto>>, ApiError> {
    let user = &auth.0.user;
    let dao = LibraryDao::new(state.db.clone());
    let mut libraries = if user.can_access_all_libraries() {
        dao.find_all()?
    } else {
        dao.find_all()?
            .into_iter()
            .filter(|l| user.shared_libraries_ids.contains(&l.id))
            .collect()
    };
    libraries.sort_by_key(|l| l.name.to_lowercase());
    Ok(Json(
        libraries
            .iter()
            .map(|l| LibraryDto::of(l, user.is_admin()))
            .collect(),
    ))
}

async fn get_library_by_id(
    State(state): State<AppState>,
    auth: RequireAuth,
    Path(library_id): Path<String>,
) -> Result<Json<LibraryDto>, ApiError> {
    let user = &auth.0.user;
    let library = LibraryDao::new(state.db.clone())
        .find_by_id(&library_id)?
        .ok_or_else(|| ApiError::not_found(""))?;
    if !user.can_access_library(&library.id) {
        return Err(ApiError::forbidden(""));
    }
    Ok(Json(LibraryDto::of(&library, user.is_admin())))
}

fn library_dao(state: &AppState) -> LibraryDao {
    LibraryDao::new(state.db.clone())
}

fn blank_violation(field: &str, message: &str) -> Violation {
    Violation {
        field_name: field.into(),
        message: message.into(),
    }
}

fn map_library_error(e: crate::service::library::LibraryError) -> ApiError {
    if e.is_validation() {
        ApiError::bad_request(e.message())
    } else {
        ApiError::Internal(e.message())
    }
}

async fn add_library(
    State(state): State<AppState>,
    auth: RequireAuth,
    Json(body): Json<LibraryCreationDto>,
) -> Result<Json<LibraryDto>, ApiError> {
    auth.0.require_admin()?;

    let mut violations = vec![];
    if body.name.trim().is_empty() {
        violations.push(blank_violation("name", "must not be blank"));
    }
    if body.root.trim().is_empty() {
        violations.push(blank_violation("root", "must not be blank"));
    }
    if !violations.is_empty() {
        return Err(ApiError::Violations(violations));
    }

    let library = Library {
        id: String::new(),
        name: body.name,
        root: path_to_url(FsPath::new(&body.root)),
        import_comicinfo_book: body.import_comicinfo_book,
        import_comicinfo_series: body.import_comicinfo_series,
        import_comicinfo_collection: body.import_comicinfo_collection,
        import_comicinfo_readlist: body.import_comicinfo_readlist,
        import_comicinfo_series_append_volume: body.import_comicinfo_series_append_volume,
        import_epub_book: body.import_epub_book,
        import_epub_series: body.import_epub_series,
        import_mylar_series: body.import_mylar_series,
        import_local_artwork: body.import_local_artwork,
        import_barcode_isbn: body.import_barcode_isbn,
        scan_force_modified_time: body.scan_force_modified_time,
        scan_on_startup: body.scan_on_startup,
        scan_interval: body.scan_interval.to_domain(),
        scan_cbx: body.scan_cbx,
        scan_pdf: body.scan_pdf,
        scan_epub: body.scan_epub,
        scan_directory_exclusions: body.scan_directory_exclusions.into_iter().collect(),
        repair_extensions: body.repair_extensions,
        convert_to_cbz: body.convert_to_cbz,
        empty_trash_after_scan: body.empty_trash_after_scan,
        series_cover: body.series_cover.to_domain(),
        hash_files: body.hash_files,
        hash_pages: body.hash_pages,
        hash_koreader: body.hash_koreader,
        analyze_dimensions: body.analyze_dimensions,
        oneshots_directory: body.oneshots_directory.filter(|d| !d.trim().is_empty()),
        unavailable_date: None,
        created_date: now_utc(),
        last_modified_date: now_utc(),
    };

    let created =
        crate::service::library::add_library(&state, &library).map_err(map_library_error)?;
    Ok(Json(LibraryDto::of(&created, true)))
}

async fn update_library_by_id(
    State(state): State<AppState>,
    auth: RequireAuth,
    Path(library_id): Path<String>,
    Json(body): Json<LibraryUpdateDto>,
) -> Result<StatusCode, ApiError> {
    auth.0.require_admin()?;

    let mut violations = vec![];
    if let Some(name) = &body.name {
        if name.trim().is_empty() {
            violations.push(blank_violation("name", "Must be null or not blank"));
        }
    }
    if let Some(root) = &body.root {
        if root.trim().is_empty() {
            violations.push(blank_violation("root", "Must be null or not blank"));
        }
    }
    if !violations.is_empty() {
        return Err(ApiError::Violations(violations));
    }

    let dao = library_dao(&state);
    let Some(existing) = dao.find_by_id(&library_id)? else {
        return Err(ApiError::not_found(""));
    };

    let to_update = Library {
        name: body.name.unwrap_or_else(|| existing.name.clone()),
        root: body
            .root
            .map(|r| path_to_url(FsPath::new(&r)))
            .unwrap_or_else(|| existing.root.clone()),
        import_comicinfo_book: body
            .import_comicinfo_book
            .unwrap_or(existing.import_comicinfo_book),
        import_comicinfo_series: body
            .import_comicinfo_series
            .unwrap_or(existing.import_comicinfo_series),
        import_comicinfo_collection: body
            .import_comicinfo_collection
            .unwrap_or(existing.import_comicinfo_collection),
        import_comicinfo_readlist: body
            .import_comicinfo_readlist
            .unwrap_or(existing.import_comicinfo_readlist),
        import_comicinfo_series_append_volume: body
            .import_comicinfo_series_append_volume
            .unwrap_or(existing.import_comicinfo_series_append_volume),
        import_epub_book: body.import_epub_book.unwrap_or(existing.import_epub_book),
        import_epub_series: body
            .import_epub_series
            .unwrap_or(existing.import_epub_series),
        import_mylar_series: body
            .import_mylar_series
            .unwrap_or(existing.import_mylar_series),
        import_local_artwork: body
            .import_local_artwork
            .unwrap_or(existing.import_local_artwork),
        import_barcode_isbn: body
            .import_barcode_isbn
            .unwrap_or(existing.import_barcode_isbn),
        scan_force_modified_time: body
            .scan_force_modified_time
            .unwrap_or(existing.scan_force_modified_time),
        scan_on_startup: body.scan_on_startup.unwrap_or(existing.scan_on_startup),
        scan_interval: body
            .scan_interval
            .map(|i| i.to_domain())
            .unwrap_or(existing.scan_interval),
        scan_cbx: body.scan_cbx.unwrap_or(existing.scan_cbx),
        scan_pdf: body.scan_pdf.unwrap_or(existing.scan_pdf),
        scan_epub: body.scan_epub.unwrap_or(existing.scan_epub),
        scan_directory_exclusions: match body.scan_directory_exclusions {
            Some(v) => v.map(|s| s.into_iter().collect()).unwrap_or_default(),
            None => existing.scan_directory_exclusions,
        },
        repair_extensions: body.repair_extensions.unwrap_or(existing.repair_extensions),
        convert_to_cbz: body.convert_to_cbz.unwrap_or(existing.convert_to_cbz),
        empty_trash_after_scan: body
            .empty_trash_after_scan
            .unwrap_or(existing.empty_trash_after_scan),
        series_cover: body
            .series_cover
            .map(|c| c.to_domain())
            .unwrap_or(existing.series_cover),
        hash_files: body.hash_files.unwrap_or(existing.hash_files),
        hash_pages: body.hash_pages.unwrap_or(existing.hash_pages),
        hash_koreader: body.hash_koreader.unwrap_or(existing.hash_koreader),
        analyze_dimensions: body
            .analyze_dimensions
            .unwrap_or(existing.analyze_dimensions),
        oneshots_directory: match body.oneshots_directory {
            Some(v) => v.filter(|d| !d.trim().is_empty()),
            None => existing.oneshots_directory,
        },
        ..existing
    };

    crate::service::library::update_library(&state, &to_update).map_err(map_library_error)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_library_by_id(
    State(state): State<AppState>,
    auth: RequireAuth,
    Path(library_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    auth.0.require_admin()?;
    let Some(library) = library_dao(&state).find_by_id(&library_id)? else {
        return Err(ApiError::not_found(""));
    };
    crate::service::library::delete_library(&state, &library)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn library_scan(
    State(state): State<AppState>,
    auth: RequireAuth,
    Path(library_id): Path<String>,
    crate::http::pagination::QueryPageable { params, .. }: crate::http::pagination::QueryPageable,
) -> Result<StatusCode, ApiError> {
    auth.0.require_admin()?;
    let dao = library_dao(&state);
    if dao.find_by_id(&library_id)?.is_none() {
        return Err(ApiError::not_found(""));
    }
    let deep = crate::http::pagination::QueryExt::first_bool(&params, "deep").unwrap_or(false);
    state
        .task_emitter
        .scan_library(&library_id, deep, HIGHEST_PRIORITY)?;
    Ok(StatusCode::ACCEPTED)
}

async fn library_analyze(
    State(state): State<AppState>,
    auth: RequireAuth,
    Path(library_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    auth.0.require_admin()?;
    let books = books_of_library(&state, &library_id)?;
    state.task_emitter.analyze_books(&books, HIGH_PRIORITY)?;
    Ok(StatusCode::ACCEPTED)
}

async fn library_refresh_metadata(
    State(state): State<AppState>,
    auth: RequireAuth,
    Path(library_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    auth.0.require_admin()?;
    let books = books_of_library(&state, &library_id)?;
    for book in &books {
        state.task_emitter.refresh_book_metadata(
            book,
            BookMetadataPatchCapability::all(),
            HIGH_PRIORITY,
        )?;
    }
    state
        .task_emitter
        .refresh_books_local_artwork(&books, HIGH_PRIORITY)?;
    let series_ids = SeriesDao::new(state.db.clone()).find_all_ids_by_library_id(&library_id)?;
    for series_id in &series_ids {
        state
            .task_emitter
            .refresh_series_local_artwork(series_id, HIGH_PRIORITY)?;
    }
    Ok(StatusCode::ACCEPTED)
}

async fn library_empty_trash(
    State(state): State<AppState>,
    auth: RequireAuth,
    Path(library_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    auth.0.require_admin()?;
    if library_dao(&state).find_by_id(&library_id)?.is_none() {
        return Err(ApiError::not_found(""));
    }
    state
        .task_emitter
        .empty_trash(&library_id, HIGHEST_PRIORITY)?;
    Ok(StatusCode::ACCEPTED)
}

fn books_of_library(
    state: &AppState,
    library_id: &str,
) -> komga_db::Result<Vec<komga_core::model::book::Book>> {
    BookDao::new(state.db.clone()).find_all_by_condition(
        Some(&komga_core::search::SearchConditionBook::LibraryId {
            operator: komga_core::search::Equality::Is {
                value: library_id.to_string(),
            },
        }),
        &komga_core::search::SearchContext::default(),
        &[],
    )
}

#[cfg(test)]
pub(crate) mod test_support {
    //! Shared oneshot-test scaffolding for endpoint tests: in-memory databases with all
    //! migrations applied, an AppState, and fixture inserters. Defined here (first endpoint
    //! module with tests) and reused by sibling endpoint modules.

    use crate::auth;
    use crate::state::{test_app_state, test_kmrs_db, AppState};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use komga_core::model::user::{ApiKey, KomgaUser, UserRole};
    use komga_core::time_codec::now_utc;
    use komga_db::dao::user::UserDao;
    use komga_db::pool::Database;
    use komga_db::{Migrator, Placeholders};
    use std::path::PathBuf;
    use tower::ServiceExt;

    pub(crate) struct TestApp {
        pub state: AppState,
        app: axum::Router,
    }

    impl TestApp {
        pub(crate) fn new(routes: axum::Router<AppState>) -> Self {
            Self::with_config(routes, test_config())
        }

        pub(crate) fn with_config(
            routes: axum::Router<AppState>,
            config: crate::config::ServerConfig,
        ) -> Self {
            let db = Database::open_in_memory(true).unwrap();
            let migrations = komga_db::main_migrations();
            Migrator::new(&migrations, Placeholders::default())
                .migrate(&db.rw().unwrap())
                .unwrap();
            let tasks_db = Database::open_in_memory(false).unwrap();
            let tasks_migrations = komga_db::tasks_migrations();
            Migrator::new(&tasks_migrations, Placeholders::default())
                .migrate(&tasks_db.rw().unwrap())
                .unwrap();
            let (state, _) = test_app_state(config, db, tasks_db, test_kmrs_db());
            Self::with_state(routes, state)
        }

        pub(crate) fn with_state(routes: axum::Router<AppState>, state: AppState) -> Self {
            let app = routes
                .layer(axum::middleware::from_fn_with_state(
                    state.clone(),
                    auth::auth_middleware,
                ))
                .with_state(state.clone());
            Self { state, app }
        }

        pub(crate) async fn get(&self, uri: &str, api_key: &str) -> (StatusCode, Vec<u8>) {
            let (status, _, bytes) = self.get_response(uri, api_key).await;
            (status, bytes)
        }

        pub(crate) async fn get_response(
            &self,
            uri: &str,
            api_key: &str,
        ) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
            self.request(
                Request::get(uri)
                    .header("X-API-Key", api_key)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
        }

        pub(crate) async fn request(
            &self,
            request: Request<Body>,
        ) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
            let response = self.app.clone().oneshot(request).await.unwrap();
            let status = response.status();
            let headers = response.headers().clone();
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap()
                .to_vec();
            (status, headers, bytes)
        }

        pub(crate) async fn get_json(
            &self,
            uri: &str,
            api_key: &str,
        ) -> (StatusCode, serde_json::Value) {
            let (status, bytes) = self.get(uri, api_key).await;
            let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
            (status, json)
        }

        /// GET with an arbitrary auth header instead of `X-API-Key` (e.g. the KOReader
        /// sync chain's `X-Auth-User`).
        pub(crate) async fn get_json_with_header(
            &self,
            uri: &str,
            header: &str,
            value: &str,
        ) -> (StatusCode, serde_json::Value) {
            let request = Request::get(uri)
                .header(header, value)
                .body(Body::empty())
                .unwrap();
            let response = self.app.clone().oneshot(request).await.unwrap();
            let status = response.status();
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap()
                .to_vec();
            let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
            (status, json)
        }

        pub(crate) async fn request_json(
            &self,
            method: &str,
            uri: &str,
            api_key: &str,
            body: Option<serde_json::Value>,
        ) -> (StatusCode, serde_json::Value) {
            let builder = match method {
                "POST" => Request::post(uri),
                "PUT" => Request::put(uri),
                "PATCH" => Request::patch(uri),
                "DELETE" => Request::delete(uri),
                _ => Request::get(uri),
            };
            let request = builder
                .header("X-API-Key", api_key)
                .header("Content-Type", "application/json")
                .body(match body {
                    Some(v) => Body::from(v.to_string()),
                    None => Body::empty(),
                })
                .unwrap();
            let response = self.app.clone().oneshot(request).await.unwrap();
            let status = response.status();
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap()
                .to_vec();
            let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
            (status, json)
        }
    }

    pub(crate) fn test_config() -> crate::config::ServerConfig {
        crate::config::ServerConfig {
            config_dir: PathBuf::new(),
            lucene_dir: PathBuf::new(),
            fonts_dir: PathBuf::new(),
            port: 25600,
            database: Default::default(),
            tasks_db: Default::default(),
            kmrs_db: Default::default(),
            sessions_db: Default::default(),
            jdbc_session_enabled: false,
            session_timeout: std::time::Duration::from_secs(3600),
            cors_allowed_origins: vec![],
            page_hashing: 3,
            epub_divina_letter_count_threshold: 15,
            kobo_sync_item_limit: 100,
            kepubify_path: None,
            server_context_path: None,
            webhooks: Default::default(),
            migration_placeholders: Default::default(),
            oauth2: Default::default(),

            komf_url: None,
            komf_base_url: None,
            komf_auth_key: None,
            history_retention_days: 180,
            update_check_enabled: true,
            log_level: None,
            sort_locale: None,
            thumbnail_storage: Default::default(),
            thumbnail_deep_etag: true,
        }
    }

    pub(crate) fn insert_user(
        db: &Database,
        email: &str,
        admin: bool,
        shared_all: bool,
        shared_ids: &[&str],
    ) -> String {
        let user = KomgaUser {
            id: String::new(),
            email: email.into(),
            password: "x".into(),
            roles: if admin {
                [UserRole::Admin].into_iter().collect()
            } else {
                Default::default()
            },
            shared_libraries_ids: shared_ids.iter().map(|s| s.to_string()).collect(),
            shared_all_libraries: shared_all,
            restrictions: Default::default(),
            created_date: now_utc(),
            last_modified_date: now_utc(),
        };
        UserDao::new(db.clone()).insert(&user).unwrap()
    }

    pub(crate) fn insert_api_key(db: &Database, user_id: &str, plain: &str) {
        let key = ApiKey {
            id: String::new(),
            user_id: user_id.into(),
            key: auth::sha512_hex(plain),
            comment: "test".into(),
            created_date: now_utc(),
            last_modified_date: now_utc(),
        };
        UserDao::new(db.clone()).insert_api_key(&key).unwrap();
    }

    pub(crate) fn insert_library(db: &Database, id: &str, name: &str) {
        let library = komga_core::model::library::Library {
            id: id.into(),
            name: name.into(),
            root: format!("file:/data/{}/", name.to_lowercase()),
            import_comicinfo_book: false,
            import_comicinfo_series: false,
            import_comicinfo_collection: false,
            import_comicinfo_readlist: false,
            import_comicinfo_series_append_volume: false,
            import_epub_book: false,
            import_epub_series: false,
            import_mylar_series: false,
            import_local_artwork: false,
            import_barcode_isbn: false,
            scan_force_modified_time: false,
            scan_on_startup: false,
            scan_interval: komga_core::model::library::ScanInterval::Daily,
            scan_cbx: true,
            scan_pdf: true,
            scan_epub: true,
            scan_directory_exclusions: vec![],
            repair_extensions: false,
            convert_to_cbz: false,
            empty_trash_after_scan: false,
            series_cover: komga_core::model::library::SeriesCover::First,
            hash_files: false,
            hash_pages: false,
            hash_koreader: false,
            analyze_dimensions: false,
            oneshots_directory: None,
            unavailable_date: None,
            created_date: now_utc(),
            last_modified_date: now_utc(),
        };
        komga_db::dao::library::LibraryDao::new(db.clone())
            .insert(&library)
            .unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::libraries::test_support::{
        insert_api_key, insert_library, insert_user, TestApp,
    };
    use axum::http::StatusCode;

    fn app() -> TestApp {
        TestApp::new(router())
    }

    #[tokio::test]
    async fn list_sorted_and_root_only_for_admin() {
        let app = app();
        insert_library(&app.state.db, "l1", "Zeta");
        insert_library(&app.state.db, "l2", "alpha");
        let admin = insert_user(&app.state.db, "admin@x.c", true, true, &[]);
        insert_api_key(&app.state.db, &admin, "k-admin");

        let (status, body) = app.get_json("/api/v1/libraries", "k-admin").await;
        assert_eq!(status, StatusCode::OK);
        // sorted by name lowercase
        assert_eq!(
            body.as_array().unwrap().len(),
            2,
            "expected both libraries: {body}"
        );
        assert_eq!(body[0]["name"], "alpha");
        assert_eq!(body[1]["name"], "Zeta");
        assert_eq!(body[0]["root"], "/data/alpha");

        let user = insert_user(&app.state.db, "user@x.c", false, true, &[]);
        insert_api_key(&app.state.db, &user, "k-user");
        let (status, body) = app.get_json("/api/v1/libraries", "k-user").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body[0]["root"], "");
    }

    #[tokio::test]
    async fn list_filters_shared_libraries() {
        let app = app();
        insert_library(&app.state.db, "l1", "One");
        insert_library(&app.state.db, "l2", "Two");
        let user = insert_user(&app.state.db, "user@x.c", false, false, &["l2"]);
        insert_api_key(&app.state.db, &user, "k-user");

        let (status, body) = app.get_json("/api/v1/libraries", "k-user").await;
        assert_eq!(status, StatusCode::OK);
        let names: Vec<&str> = body
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["Two"]);
    }

    #[tokio::test]
    async fn by_id_not_found_and_forbidden() {
        let app = app();
        insert_library(&app.state.db, "l1", "One");
        let admin = insert_user(&app.state.db, "admin@x.c", true, true, &[]);
        insert_api_key(&app.state.db, &admin, "k-admin");
        let user = insert_user(&app.state.db, "user@x.c", false, false, &[]);
        insert_api_key(&app.state.db, &user, "k-user");

        let (status, body) = app.get_json("/api/v1/libraries/l1", "k-admin").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["name"], "One");

        let (status, _) = app.get_json("/api/v1/libraries/nope", "k-admin").await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let (status, _) = app.get_json("/api/v1/libraries/l1", "k-user").await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    fn admin_app() -> (TestApp, String) {
        let app = app();
        let admin = insert_user(&app.state.db, "admin@x.c", true, true, &[]);
        insert_api_key(&app.state.db, &admin, "k-admin");
        (app, "k-admin".to_string())
    }

    #[tokio::test]
    async fn create_library_full_flow() {
        let (app, key) = admin_app();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("manga");
        std::fs::create_dir(&root).unwrap();

        let (status, body) = app
            .request_json(
                "POST",
                "/api/v1/libraries",
                &key,
                Some(serde_json::json!({"name":"Manga","root":root.to_str().unwrap()})),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["name"], "Manga");
        assert_eq!(body["root"], root.to_str().unwrap());
        assert_eq!(body["importComicInfoBook"], true);
        assert_eq!(body["scanInterval"], "EVERY_6H");

        // validation: blank name
        let (status, body) = app
            .request_json(
                "POST",
                "/api/v1/libraries",
                &key,
                Some(serde_json::json!({"name":" ","root":"/tmp"})),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            body["violations"][0]["fieldName"],
            serde_json::json!("name")
        );

        // duplicate name
        let (status, body) = app
            .request_json(
                "POST",
                "/api/v1/libraries",
                &key,
                Some(serde_json::json!({"name":"Manga","root":root.to_str().unwrap()})),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            body["message"],
            "400 BAD_REQUEST \"Library name already exists\""
        );

        // missing folder
        let (status, body) = app
            .request_json(
                "POST",
                "/api/v1/libraries",
                &key,
                Some(serde_json::json!({"name":"Nope","root":"/nonexistent/path"})),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            body["message"]
                .as_str()
                .unwrap()
                .contains("Library root folder does not exist"),
            "{body}"
        );

        // non-admin is rejected
        let user = insert_user(&app.state.db, "user@x.c", false, true, &[]);
        insert_api_key(&app.state.db, &user, "k-user");
        let (status, _) = app
            .request_json(
                "POST",
                "/api/v1/libraries",
                "k-user",
                Some(serde_json::json!({"name":"X","root":"/tmp"})),
            )
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn update_library_isset_semantics() {
        let (app, key) = admin_app();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("manga");
        std::fs::create_dir(&root).unwrap();
        let (status, body) = app
            .request_json(
                "POST",
                "/api/v1/libraries",
                &key,
                Some(serde_json::json!({
                    "name":"Manga","root":root.to_str().unwrap(),
                    "oneshotsDirectory":"oneshots","scanDirectoryExclusions":["#recycle"]
                })),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        let id = body["id"].as_str().unwrap().to_string();

        // partial update: name only, others preserved
        let (status, _) = app
            .request_json(
                "PATCH",
                &format!("/api/v1/libraries/{id}"),
                &key,
                Some(serde_json::json!({"name":"Renamed"})),
            )
            .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (_, body) = app.get_json(&format!("/api/v1/libraries/{id}"), &key).await;
        assert_eq!(body["name"], "Renamed");
        assert_eq!(body["oneshotsDirectory"], "oneshots");
        assert_eq!(
            body["scanDirectoryExclusions"],
            serde_json::json!(["#recycle"])
        );

        // explicit null clears oneshotsDirectory and exclusions
        let (status, _) = app
            .request_json(
                "PATCH",
                &format!("/api/v1/libraries/{id}"),
                &key,
                Some(serde_json::json!({"oneshotsDirectory":null,"scanDirectoryExclusions":null})),
            )
            .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (_, body) = app.get_json(&format!("/api/v1/libraries/{id}"), &key).await;
        assert_eq!(body["oneshotsDirectory"], serde_json::Value::Null);
        assert_eq!(body["scanDirectoryExclusions"], serde_json::json!([]));

        // blank name rejected with NullOrNotBlank message
        let (status, body) = app
            .request_json(
                "PATCH",
                &format!("/api/v1/libraries/{id}"),
                &key,
                Some(serde_json::json!({"name":" "})),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            body["violations"][0]["message"],
            "Must be null or not blank"
        );

        // 404
        let (status, _) = app
            .request_json(
                "PATCH",
                "/api/v1/libraries/nope",
                &key,
                Some(serde_json::json!({"name":"X"})),
            )
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn delete_scan_analyze_refresh_empty_trash() {
        let (app, key) = admin_app();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("manga");
        std::fs::create_dir(&root).unwrap();
        let (_, body) = app
            .request_json(
                "POST",
                "/api/v1/libraries",
                &key,
                Some(serde_json::json!({"name":"Manga","root":root.to_str().unwrap()})),
            )
            .await;
        let id = body["id"].as_str().unwrap().to_string();
        let tasks = || komga_db::dao::tasks::TasksDao::new(app.state.tasks_db.clone());

        // scan
        tasks().delete_all().unwrap();
        let (status, _) = app
            .request_json(
                "POST",
                &format!("/api/v1/libraries/{id}/scan?deep=true"),
                &key,
                None,
            )
            .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        let all = tasks().find_all().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].unique_id(), format!("SCAN_LIBRARY_{id}_DEEP_true"));
        assert_eq!(all[0].priority(), 8);

        // analyze / refresh: no books, so no task submitted, but still 202
        tasks().delete_all().unwrap();
        let (status, _) = app
            .request_json(
                "POST",
                &format!("/api/v1/libraries/{id}/analyze"),
                &key,
                None,
            )
            .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        let (status, _) = app
            .request_json(
                "POST",
                &format!("/api/v1/libraries/{id}/metadata/refresh"),
                &key,
                None,
            )
            .await;
        assert_eq!(status, StatusCode::ACCEPTED);

        // empty-trash
        let (status, _) = app
            .request_json(
                "POST",
                &format!("/api/v1/libraries/{id}/empty-trash"),
                &key,
                None,
            )
            .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        let all = tasks().find_all().unwrap();
        assert!(all
            .iter()
            .any(|t| t.unique_id() == format!("EMPTY_TRASH_{id}")));

        // 404s
        let (status, _) = app
            .request_json("POST", "/api/v1/libraries/nope/scan", &key, None)
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = app
            .request_json("POST", "/api/v1/libraries/nope/empty-trash", &key, None)
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // delete
        let (status, _) = app
            .request_json("DELETE", &format!("/api/v1/libraries/{id}"), &key, None)
            .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (status, _) = app.get_json(&format!("/api/v1/libraries/{id}"), &key).await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // non-admin 403
        let user = insert_user(&app.state.db, "user@x.c", false, true, &[]);
        insert_api_key(&app.state.db, &user, "k-user");
        let (status, _) = app
            .request_json("POST", "/api/v1/libraries/l1/scan", "k-user", None)
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }
}
