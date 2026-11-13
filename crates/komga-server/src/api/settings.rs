//! `SettingsController.kt`: server settings read/update.

use crate::auth::RequireAuth;
use crate::dto::settings::{SettingMultiSource, SettingsDto, SettingsUpdateDto, ThumbnailSizeDto};
use crate::error::ApiError;
use crate::state::AppState;
use axum::extract::State;
use axum::http::StatusCode;
use axum::{routing, Json, Router};
use komga_db::dao::settings::SettingsDao;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/settings", routing::get(get_server_settings))
        .route("/api/v1/settings", routing::patch(update_server_settings))
}

/// Upper bound for `recentReadingDays` (ten years): larger windows behave like off anyway.
const MAX_RECENT_READING_DAYS: u32 = 3650;

async fn get_server_settings(
    State(state): State<AppState>,
    auth: RequireAuth,
) -> Json<SettingsDto> {
    let s = state.settings.get();
    let config = &state.config;
    let dto = SettingsDto {
        delete_empty_collections: Some(s.delete_empty_collections),
        delete_empty_read_lists: Some(s.delete_empty_readlists),
        remember_me_duration_days: Some((s.remember_me_duration.as_secs() / 86_400) as i64),
        thumbnail_size: Some(ThumbnailSizeDto::from(s.thumbnail_size)),
        task_pool_size: Some(s.task_pool_size as i64),
        server_port: Some(SettingMultiSource {
            configuration_source: Some(config.port),
            database_source: None,
            effective_value: Some(config.port),
        }),
        server_context_path: Some(SettingMultiSource {
            configuration_source: config.server_context_path.clone(),
            database_source: None,
            effective_value: Some(config.server_context_path.clone().unwrap_or_default()),
        }),
        kobo_proxy: Some(s.kobo_proxy),
        kepubify_path: Some(SettingMultiSource {
            configuration_source: config
                .kepubify_path
                .as_ref()
                .map(|p| p.display().to_string()),
            database_source: None,
            // Java reports the resolved path (configuration value, probed)
            effective_value: state
                .kepub
                .kepubify_path(config)
                .map(|p| p.display().to_string()),
        }),
        max_upload_file_size_bytes: Some(crate::http::body_limit::MAX_UPLOAD_FILE_SIZE as i64),
        skip_read_on_next: Some(s.next_skip_read),
        recent_reading_days: Some(i64::from(s.recent_reading_days)),
    };
    if auth.0.user.is_admin() {
        Json(dto)
    } else {
        Json(dto.public())
    }
}

async fn update_server_settings(
    State(state): State<AppState>,
    auth: RequireAuth,
    Json(body): Json<SettingsUpdateDto>,
) -> Result<StatusCode, ApiError> {
    auth.0.require_admin()?;
    let dao = SettingsDao::new(state.db.clone());
    let mut violations = vec![];
    if body.task_pool_size.is_some_and(|v| v <= 0) {
        violations.push(violation("taskPoolSize", "must be greater than 0"));
    }
    if body.remember_me_duration_days.is_some_and(|v| v <= 0) {
        violations.push(violation(
            "rememberMeDurationDays",
            "must be greater than 0",
        ));
    }
    if body
        .recent_reading_days
        .is_some_and(|v| !(0..=i64::from(MAX_RECENT_READING_DAYS)).contains(&v))
    {
        violations.push(violation("recentReadingDays", "must be between 0 and 3650"));
    }
    if !violations.is_empty() {
        return Err(ApiError::Violations(violations));
    }

    if let Some(v) = body.delete_empty_collections {
        dao.save_setting_bool("DELETE_EMPTY_COLLECTIONS", v)?;
    }
    if let Some(v) = body.delete_empty_read_lists {
        dao.save_setting_bool("DELETE_EMPTY_READLISTS", v)?;
    }
    if let Some(v) = body.remember_me_duration_days {
        dao.save_setting_i64("REMEMBER_ME_DURATION", v)?;
    }
    if body.renew_remember_me_key == Some(true) {
        state.settings.renew_remember_me_key();
    }
    if let Some(v) = body.thumbnail_size {
        dao.save_setting("THUMBNAIL_SIZE", v.as_str())?;
    }
    if let Some(v) = body.task_pool_size {
        dao.save_setting_i64("TASK_POOL_SIZE", v)?;
    }
    if let Some(v) = body.kobo_proxy {
        dao.save_setting_bool("KOBO_PROXY", v)?;
    }
    if let Some(v) = body.skip_read_on_next {
        dao.save_setting_bool(crate::settings::KEY_NEXT_SKIP_READ, v)?;
    }
    if let Some(v) = body.recent_reading_days {
        dao.save_setting_i64(crate::settings::KEY_RECENT_READING_DAYS, v)?;
    }

    state.settings.reload();
    Ok(StatusCode::NO_CONTENT)
}

fn violation(field: &str, message: &str) -> crate::error::Violation {
    crate::error::Violation {
        field_name: field.into(),
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::collections::tests::{call, get, insert_user, test_state};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};

    fn seed_admin(state: &AppState) -> String {
        insert_user(
            &state.db,
            "a@b.c",
            &[komga_core::model::user::UserRole::Admin],
            &[],
            Default::default(),
            "k",
        );
        "k".to_string()
    }

    #[tokio::test]
    async fn get_settings_admin_and_public() {
        let state = test_state();
        let key = seed_admin(&state);
        let (status, json) = {
            let (s, _, b) = call(&state, router(), get("/api/v1/settings", &key)).await;
            (s, serde_json::from_slice::<serde_json::Value>(&b).unwrap())
        };
        assert_eq!(status, StatusCode::OK);
        // migration placeholders default both to true
        assert_eq!(json["deleteEmptyCollections"], true);
        assert_eq!(json["deleteEmptyReadLists"], true);
        assert_eq!(json["rememberMeDurationDays"], 365);
        assert_eq!(json["thumbnailSize"], "DEFAULT");
        assert_eq!(json["taskPoolSize"], 2);
        assert_eq!(json["serverPort"]["effectiveValue"], 25600);
        assert_eq!(json["serverPort"]["configurationSource"], 25600);
        assert_eq!(
            json["serverPort"]["databaseSource"],
            serde_json::Value::Null
        );
        assert_eq!(json["koboProxy"], false);
        assert_eq!(json["skipReadOnNext"], false);
        assert_eq!(json["maxUploadFileSizeBytes"], 2 * 1024 * 1024);
        // the limit the server enforces, not a number of its own
        assert_eq!(
            json["maxUploadFileSizeBytes"],
            crate::http::body_limit::MAX_UPLOAD_FILE_SIZE
        );
        assert_eq!(json["recentReadingDays"], 0);

        // non-admin: only maxUploadFileSizeBytes, skipReadOnNext and recentReadingDays survive
        insert_user(&state.db, "u@b.c", &[], &[], Default::default(), "u");
        let (status, json) = {
            let (s, _, b) = call(&state, router(), get("/api/v1/settings", "u")).await;
            (s, serde_json::from_slice::<serde_json::Value>(&b).unwrap())
        };
        assert_eq!(status, StatusCode::OK);
        // the limit the server enforces, not a number of its own
        assert_eq!(
            json["maxUploadFileSizeBytes"],
            crate::http::body_limit::MAX_UPLOAD_FILE_SIZE
        );
        assert_eq!(json["recentReadingDays"], 0);
        assert!(json.get("taskPoolSize").is_none());
        assert!(json.get("serverPort").is_none());
        assert!(json.get("deleteEmptyCollections").is_none());
        assert_eq!(json["skipReadOnNext"], false);
    }

    async fn get_kepubify(state: &AppState, key: &str) -> serde_json::Value {
        let (status, json) = {
            let (s, _, b) = call(state, router(), get("/api/v1/settings", key)).await;
            (s, serde_json::from_slice::<serde_json::Value>(&b).unwrap())
        };
        assert_eq!(status, StatusCode::OK);
        json["kepubifyPath"].clone()
    }

    // chmod-based script setup is POSIX-only; skipped on Windows
    #[cfg(unix)]
    #[tokio::test]
    async fn kepubify_effective_value_comes_from_configuration() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("kepubify");
        std::fs::write(&script, "#!/bin/sh\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let config_str = script.to_str().unwrap().to_string();

        let mut state = test_state();
        let mut config = (*state.config).clone();
        config.kepubify_path = Some(script);
        state.config = std::sync::Arc::new(config);
        let key = seed_admin(&state);

        let kepubify = get_kepubify(&state, &key).await;
        assert_eq!(kepubify["configurationSource"], config_str);
        assert_eq!(kepubify["databaseSource"], serde_json::Value::Null);
        assert_eq!(kepubify["effectiveValue"], config_str);

        // a stale DB row from the Java era is not honored
        let dao = SettingsDao::new(state.db.clone());
        dao.save_setting("KEPUBIFY_PATH", "/nonexistent/kepubify")
            .unwrap();
        state.settings.reload();
        let kepubify = get_kepubify(&state, &key).await;
        assert_eq!(kepubify["databaseSource"], serde_json::Value::Null);
        assert_eq!(kepubify["effectiveValue"], config_str);

        // an unprobed configuration value reports no effective path
        let mut config = (*state.config).clone();
        config.kepubify_path = Some(std::path::PathBuf::from("/nonexistent/kepubify"));
        state.config = std::sync::Arc::new(config);
        let kepubify = get_kepubify(&state, &key).await;
        assert_eq!(kepubify["effectiveValue"], serde_json::Value::Null);
    }

    #[tokio::test]
    async fn patch_updates_and_reload() {
        let state = test_state();
        seed_admin(&state);
        let body = serde_json::json!({
            "deleteEmptyCollections": true,
            "taskPoolSize": 4,
            "thumbnailSize": "LARGE",
            "koboProxy": true,
            "skipReadOnNext": true
        });
        let (status, _, _) = call(
            &state,
            router(),
            Request::patch("/api/v1/settings")
                .header("X-API-Key", "k")
                .header("Content-Type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let s = state.settings.get();
        assert!(s.delete_empty_collections);
        assert_eq!(s.task_pool_size, 4);
        assert_eq!(s.thumbnail_size.as_str(), "LARGE");
        assert!(s.kobo_proxy);
        assert!(s.next_skip_read);
        // stored under the kmrs prefix, next to the Komga keys
        assert_eq!(
            komga_db::dao::settings::SettingsDao::new(state.db.clone())
                .get_setting("KMRS_NEXT_SKIP_READ")
                .unwrap()
                .as_deref(),
            Some("true")
        );
    }

    #[tokio::test]
    async fn recent_reading_days_patch_validate_and_public() {
        let state = test_state();
        seed_admin(&state);
        insert_user(&state.db, "u@b.c", &[], &[], Default::default(), "u");
        let patch = |value: serde_json::Value| {
            let state = state.clone();
            async move {
                let (status, _, _) = call(
                    &state,
                    router(),
                    Request::patch("/api/v1/settings")
                        .header("X-API-Key", "k")
                        .header("Content-Type", "application/json")
                        .body(Body::from(
                            serde_json::json!({ "recentReadingDays": value }).to_string(),
                        ))
                        .unwrap(),
                )
                .await;
                status
            }
        };
        let days = |api_key: &'static str| {
            let state = state.clone();
            async move {
                let (_, _, b) = call(&state, router(), get("/api/v1/settings", api_key)).await;
                serde_json::from_slice::<serde_json::Value>(&b).unwrap()["recentReadingDays"]
                    .clone()
            }
        };

        assert_eq!(patch(serde_json::json!(90)).await, StatusCode::NO_CONTENT);
        assert_eq!(state.settings.get().recent_reading_days, 90);
        assert!(state.settings.get().recent_reading_since().is_some());
        assert_eq!(
            SettingsDao::new(state.db.clone())
                .get_setting_i64("KMRS_RECENT_READING_DAYS")
                .unwrap(),
            Some(90)
        );
        assert_eq!(days("k").await, 90);
        // every client can read it
        assert_eq!(days("u").await, 90);

        assert_eq!(patch(serde_json::json!(-1)).await, StatusCode::BAD_REQUEST);
        assert_eq!(
            patch(serde_json::json!(3651)).await,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(state.settings.get().recent_reading_days, 90);

        // 0 turns it off
        assert_eq!(patch(serde_json::json!(0)).await, StatusCode::NO_CONTENT);
        assert_eq!(state.settings.get().recent_reading_days, 0);
        assert!(state.settings.get().recent_reading_since().is_none());
        assert_eq!(days("u").await, 0);
    }

    #[tokio::test]
    async fn patch_ignores_dropped_fields() {
        let state = test_state();
        seed_admin(&state);
        let body = serde_json::json!({
            "serverPort": 8080,
            "serverContextPath": "/komga",
            "kepubifyPath": "/tmp/kepubify",
            "koboPort": 17878
        });
        let (status, _, _) = call(
            &state,
            router(),
            Request::patch("/api/v1/settings")
                .header("X-API-Key", "k")
                .header("Content-Type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let dao = SettingsDao::new(state.db.clone());
        assert_eq!(dao.get_setting("SERVER_PORT").unwrap(), None);
        assert_eq!(dao.get_setting("SERVER_CONTEXT_PATH").unwrap(), None);
        assert_eq!(dao.get_setting("KEPUBIFY_PATH").unwrap(), None);
        assert_eq!(dao.get_setting("KOBO_PORT").unwrap(), None);
        let (_, _, body) = call(&state, router(), get("/api/v1/settings", "k")).await;
        let json = serde_json::from_slice::<serde_json::Value>(&body).unwrap();
        assert_eq!(json["serverPort"]["effectiveValue"], 25600);
        assert!(json.get("koboPort").is_none());
    }

    #[tokio::test]
    async fn patch_accepts_string_numbers_from_legacy_webui() {
        let state = test_state();
        seed_admin(&state);
        // the legacy WebUI serializes v-text-field number inputs as JSON strings
        let body = serde_json::json!({
            "taskPoolSize": "4",
            "rememberMeDurationDays": "30",
            "deleteEmptyCollections": "true",
            "deleteEmptyReadLists": "0",
            "koboProxy": "1"
        });
        let (status, _, _) = call(
            &state,
            router(),
            Request::patch("/api/v1/settings")
                .header("X-API-Key", "k")
                .header("Content-Type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let s = state.settings.get();
        assert_eq!(s.task_pool_size, 4);
        assert_eq!(s.remember_me_duration.as_secs() / 86_400, 30);
        assert!(s.delete_empty_collections);
        assert!(!s.delete_empty_readlists);
        assert!(s.kobo_proxy);

        // non-numeric strings are still rejected
        let body = serde_json::json!({"taskPoolSize": "abc"});
        let (status, _, _) = call(
            &state,
            router(),
            Request::patch("/api/v1/settings")
                .header("X-API-Key", "k")
                .header("Content-Type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await;
        assert!(!status.is_success());
    }

    #[tokio::test]
    async fn patch_violations_and_renew_key() {
        let state = test_state();
        seed_admin(&state);

        let body = serde_json::json!({"taskPoolSize": 0, "serverContextPath": "komga"});
        let (status, json) = {
            let (s, _, b) = call(
                &state,
                router(),
                Request::patch("/api/v1/settings")
                    .header("X-API-Key", "k")
                    .header("Content-Type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await;
            (s, serde_json::from_slice::<serde_json::Value>(&b).unwrap())
        };
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let violations = json["violations"].as_array().unwrap();
        // serverContextPath is configuration-only: silently ignored, not a violation
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0]["fieldName"], "taskPoolSize");

        // renewRememberMeKey generates a new key
        let old_key = state.settings.get().remember_me_key;
        let body = serde_json::json!({"renewRememberMeKey": true});
        let (status, _, _) = call(
            &state,
            router(),
            Request::patch("/api/v1/settings")
                .header("X-API-Key", "k")
                .header("Content-Type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let new_key = state.settings.get().remember_me_key;
        assert_ne!(old_key, new_key);
        assert_eq!(new_key.len(), 32);
    }

    #[tokio::test]
    async fn patch_requires_admin() {
        let state = test_state();
        insert_user(&state.db, "u@b.c", &[], &[], Default::default(), "k");
        let (status, _, _) = call(
            &state,
            router(),
            Request::patch("/api/v1/settings")
                .header("X-API-Key", "k")
                .header("Content-Type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }
}
