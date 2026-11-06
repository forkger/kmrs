//! The kmweb build, embedded into the binary at compile time (rust-embed over
//! `../../webui/dist`; build.rs substitutes a placeholder page when the UI was not
//! built). Unmatched paths outside the backend namespaces fall back to index.html —
//! the SPA history-mode equivalent of Java's `ResourceNotFoundController` forwarding
//! to `/`.
//!
//! Cache headers split like Java's `WebMvcConfiguration`: content-hashed build
//! output (Vite emits it under assets/) is cached for a year, everything else —
//! index.html, favicon — is no-store.

use axum::extract::Request;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use rust_embed::Embed;

#[derive(Embed)]
#[folder = "../../webui/dist"]
struct WebuiAssets;

/// First path segments owned by the backend: misses inside them stay 404 instead of
/// falling through to the SPA (Java's forward excludes /api, /opds, /sse the same way).
/// `/login` is deliberately absent — the OAuth2 callback is a registered route, and
/// plain `/login` is a webui route.
const BACKEND_SEGMENTS: &[&str] = &[
    "api", "opds", "sse", "oauth2", "actuator", "kobo", "koreader", "v3", "debug",
];

/// Content-hashed build output (Vite emits everything under assets/), safe to
/// cache long-term.
const LONG_CACHE_SEGMENTS: &[&str] = &["assets"];

pub async fn fallback(request: Request) -> Response {
    let path = request.uri().path();
    if path.split('/').any(|s| s == "..") {
        // junk paths are 404, not the SPA
        return StatusCode::NOT_FOUND.into_response();
    }
    let first_segment = path.split('/').nth(1).unwrap_or_default();
    if BACKEND_SEGMENTS.contains(&first_segment) {
        return StatusCode::NOT_FOUND.into_response();
    }

    // a miss is served as index.html: the SPA history-mode behavior
    let Some(file) =
        WebuiAssets::get(path.trim_start_matches('/')).or_else(|| WebuiAssets::get("index.html"))
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let cache_control = if LONG_CACHE_SEGMENTS.contains(&first_segment) {
        "max-age=31536000, public"
    } else {
        "no-store"
    };
    let mut response = (StatusCode::OK, file.data.to_vec()).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(file.metadata.mimetype()).unwrap(),
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(cache_control),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ServerConfig;
    use crate::state::{test_kmrs_db, AppState};
    use axum::body::Body;
    use axum::http::HeaderMap;
    use komga_db::pool::{Database, JournalMode};
    use komga_db::{Migrator, Placeholders};
    use std::sync::Arc;
    use tower::ServiceExt;

    fn test_state() -> AppState {
        let db = Database::open_in_memory(true).unwrap();
        Migrator::new(&komga_db::main_migrations(), Placeholders::default())
            .migrate(&db.rw().unwrap())
            .unwrap();
        let tasks_db = Database::open_in_memory(false).unwrap();
        Migrator::new(&komga_db::tasks_migrations(), Placeholders::default())
            .migrate(&tasks_db.rw().unwrap())
            .unwrap();
        let db_config = |register_udfs| komga_db::pool::DatabaseConfig {
            file: std::env::temp_dir(),
            register_udfs,
            journal_mode: JournalMode::Wal,
            ..Default::default()
        };
        let config = ServerConfig {
            config_dir: std::env::temp_dir(),
            lucene_dir: std::env::temp_dir(),
            fonts_dir: std::env::temp_dir(),
            port: 0,
            database: db_config(true),
            tasks_db: db_config(false),
            kmrs_db: db_config(false),
            sessions_db: db_config(false),
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
        };
        crate::state::test_app_state(config, db, tasks_db, test_kmrs_db()).0
    }

    async fn get(app: &axum::Router, uri: &str) -> (StatusCode, HeaderMap, String) {
        let response = app
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            headers,
            String::from_utf8(body.to_vec()).unwrap_or_default(),
        )
    }

    fn embedded_index() -> String {
        String::from_utf8(WebuiAssets::get("index.html").unwrap().data.into_owned()).unwrap()
    }

    #[tokio::test]
    async fn serves_embedded_index_and_assets_with_cache_policy() {
        let app = crate::build_router(test_state());

        let (status, headers, body) = get(&app, "/").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers.get(header::CACHE_CONTROL).unwrap(), "no-store");
        assert_eq!(body, embedded_index());

        // any content-hashed asset gets the year-long cache, whatever the bundle holds
        let asset = WebuiAssets::iter()
            .find(|p| p.starts_with("assets/") && p.ends_with(".js"))
            .expect("the bundle ships at least one script");
        let (status, headers, _) = get(&app, &format!("/{asset}")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            headers.get(header::CACHE_CONTROL).unwrap(),
            "max-age=31536000, public"
        );
        assert_eq!(
            headers.get(header::CONTENT_TYPE).unwrap(),
            "text/javascript"
        );
    }

    #[tokio::test]
    async fn spa_history_routes_fall_back_to_index() {
        let app = crate::build_router(test_state());

        // a webui route like /login or /libraries/<id> is not a backend route
        for uri in ["/login", "/libraries/0ABCDEF", "/book/1/pages/2"] {
            let (status, headers, body) = get(&app, uri).await;
            assert_eq!(status, StatusCode::OK, "{uri}");
            assert_eq!(headers.get(header::CACHE_CONTROL).unwrap(), "no-store");
            assert_eq!(body, embedded_index(), "{uri}");
        }
    }

    #[tokio::test]
    async fn backend_misses_stay_404() {
        let app = crate::build_router(test_state());

        for uri in [
            "/api/v1/nope",
            "/opds/v1.2/nope",
            "/sse/v1/nope",
            "/actuator/nope",
            "/oauth2/nope",
            "/koreader/nope",
        ] {
            let (status, _, _) = get(&app, uri).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
        }
    }

    #[tokio::test]
    async fn dotdot_paths_are_404_not_the_spa() {
        let app = crate::build_router(test_state());
        let (status, _, _) = get(&app, "/../secret.txt").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn anonymous_401_carries_spring_error_body() {
        let app = crate::build_router(test_state());

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v2/users/me")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/json"
        );
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["status"], 401);
        assert_eq!(body["error"], "Unauthorized");
        assert_eq!(body["message"], "Unauthorized");
        assert_eq!(body["path"], "/api/v2/users/me");
        assert!(body["timestamp"].is_string());

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v2/users/me")
                    .header("X-Requested-With", "XMLHttpRequest")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(response.headers().get(header::CONTENT_TYPE).is_none());
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(bytes.is_empty());
    }

    #[tokio::test]
    async fn xhr_401_has_no_basic_challenge() {
        let app = crate::build_router(test_state());

        // the web UI tags every API call with X-Requested-With: a bare 401
        // keeps browsers out of their native sign-in dialog
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v2/users/me")
                    .header("X-Requested-With", "XMLHttpRequest")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(response.headers().get(header::WWW_AUTHENTICATE).is_none());

        // the same request without the XHR tag keeps the Java challenge
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v2/users/me")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            response.headers().get(header::WWW_AUTHENTICATE).unwrap(),
            "Basic realm=\"Realm\""
        );
    }

    fn preflight(origin: &str) -> Request<Body> {
        Request::options("/api/v1/series")
            .header(header::ORIGIN, origin)
            .header(header::ACCESS_CONTROL_REQUEST_METHOD, "GET")
            .body(Body::empty())
            .unwrap()
    }

    #[tokio::test]
    async fn cors_preflight_follows_configured_origins() {
        let mut state = test_state();
        let mut config = (*state.config).clone();
        config.cors_allowed_origins = vec!["https://a.example".to_string()];
        state.config = Arc::new(config);
        let app = crate::build_router(state);

        let response = app
            .clone()
            .oneshot(preflight("https://a.example"))
            .await
            .unwrap();
        assert_eq!(
            response.headers().get(header::ACCESS_CONTROL_ALLOW_ORIGIN),
            Some(&HeaderValue::from_static("https://a.example"))
        );

        let response = app
            .oneshot(preflight("https://evil.example"))
            .await
            .unwrap();
        assert!(response
            .headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .is_none());
    }

    #[tokio::test]
    async fn no_cors_headers_without_configured_origins() {
        let app = crate::build_router(test_state());
        let response = app.oneshot(preflight("https://a.example")).await.unwrap();
        assert!(response
            .headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .is_none());
    }

    #[tokio::test]
    async fn cors_headers_survive_an_unauthorized_actual_request() {
        let mut state = test_state();
        let mut config = (*state.config).clone();
        config.cors_allowed_origins = vec!["https://a.example".to_string()];
        state.config = Arc::new(config);
        let app = crate::build_router(state);

        // the layer wraps auth: a cross-origin browser client must see the 401
        let response = app
            .oneshot(
                Request::get("/api/v2/users/me")
                    .header(header::ORIGIN, "https://a.example")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            response.headers().get(header::ACCESS_CONTROL_ALLOW_ORIGIN),
            Some(&HeaderValue::from_static("https://a.example"))
        );
    }
}
