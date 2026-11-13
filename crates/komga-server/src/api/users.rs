//! Equivalent of `UserController`: /api/v2/users/**.

use crate::api::claim::is_valid_email;
use crate::auth::RequireAuth;
use crate::dto::common::{Page, Pageable};
use crate::dto::user::*;
use crate::error::{ApiError, Violation};
use crate::events::DomainEvent;
use crate::http::pagination::{QueryExt, QueryPageable};
use crate::state::AppState;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::{routing, Json, Router};
use komga_core::model::user::{
    AgeRestriction, AllowExclude, ContentRestrictions, KomgaUser, UserRole,
};
use komga_core::time_codec::now_utc;
use komga_db::dao::user::UserDao;
use serde::Serialize;
use std::collections::BTreeSet;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v2/users/me", routing::get(get_me))
        .route(
            "/api/v2/users/me/password",
            routing::patch(update_my_password),
        )
        .route("/api/v2/users", routing::get(list_users).post(create_user))
        .route(
            "/api/v2/users/{id}",
            routing::patch(update_user).delete(delete_user),
        )
        .route(
            "/api/v2/users/{id}/password",
            routing::patch(update_password_by_id),
        )
        .route(
            "/api/v2/users/me/authentication-activity",
            routing::get(my_authentication_activity),
        )
        .route(
            "/api/v2/users/authentication-activity",
            routing::get(authentication_activity),
        )
        .route(
            "/api/v2/users/{id}/authentication-activity/latest",
            routing::get(latest_authentication_activity),
        )
        .route("/api/v2/users/me/sessions", routing::get(my_sessions))
        .route(
            "/api/v2/users/me/sessions/{id}",
            routing::delete(delete_my_session),
        )
        .route("/api/v2/users/sessions", routing::get(sessions))
        .route(
            "/api/v2/users/sessions/{id}",
            routing::delete(delete_session),
        )
        .route(
            "/api/v2/users/me/api-keys",
            routing::get(my_api_keys).post(create_api_key),
        )
        .route(
            "/api/v2/users/me/api-keys/{keyId}",
            routing::delete(delete_api_key),
        )
}

fn user_dao(state: &AppState) -> UserDao {
    UserDao::new(state.db.clone())
}

async fn get_me(auth: RequireAuth) -> Json<UserDto> {
    Json(UserDto::from(&auth.0.user))
}

fn validate_password(password: &str) -> Result<(), ApiError> {
    if password.trim().is_empty() {
        return Err(ApiError::Violations(vec![Violation {
            field_name: "password".into(),
            message: "must not be blank".into(),
        }]));
    }
    Ok(())
}

async fn update_my_password(
    State(state): State<AppState>,
    auth: RequireAuth,
    Json(body): Json<PasswordUpdateDto>,
) -> Result<StatusCode, ApiError> {
    validate_password(&body.password)?;
    let password =
        bcrypt::hash(&body.password, 10).map_err(|e| ApiError::Internal(e.to_string()))?;
    let (user, expire_session) =
        state
            .user_auth
            .update_user(&auth.0.user.id, &auth.0.user.id, |user| {
                user.password = password;
                Ok(())
            })?;
    let _ = state.events.send(DomainEvent::UserUpdated {
        user,
        expire_session,
    });
    Ok(StatusCode::NO_CONTENT)
}

async fn list_users(
    State(state): State<AppState>,
    auth: RequireAuth,
) -> Result<Json<Vec<UserDto>>, ApiError> {
    auth.0.require_admin()?;
    let users = user_dao(&state).find_all()?;
    Ok(Json(users.iter().map(UserDto::from).collect()))
}

/// `UserRoles.valuesOf`: invalid role names are silently ignored.
fn values_of(roles: &[String]) -> BTreeSet<UserRole> {
    roles
        .iter()
        .filter_map(|r| r.parse::<UserRole>().ok())
        .collect()
}

fn age_restriction_of(dto: Option<AgeRestrictionUpdateDto>) -> Option<AgeRestriction> {
    match dto {
        None
        | Some(AgeRestrictionUpdateDto {
            restriction: AllowExcludeDto::None,
            ..
        }) => None,
        Some(d) => Some(AgeRestriction {
            age: d.age,
            restriction: match d.restriction {
                AllowExcludeDto::AllowOnly => AllowExclude::AllowOnly,
                AllowExcludeDto::Exclude => AllowExclude::Exclude,
                AllowExcludeDto::None => unreachable!(),
            },
        }),
    }
}

async fn create_user(
    State(state): State<AppState>,
    auth: RequireAuth,
    Json(body): Json<UserCreationDto>,
) -> Result<(StatusCode, Json<UserDto>), ApiError> {
    auth.0.require_admin()?;
    let mut violations = Vec::new();
    if !is_valid_email(&body.email) {
        violations.push(Violation {
            field_name: "email".into(),
            message: "must be a well-formed email address".into(),
        });
    }
    if body.password.trim().is_empty() {
        violations.push(Violation {
            field_name: "password".into(),
            message: "must not be blank".into(),
        });
    }
    if let Some(ar) = &body.age_restriction {
        if ar.age < 0 {
            violations.push(Violation {
                field_name: "ageRestriction.age".into(),
                message: "must be greater than or equal to 0".into(),
            });
        }
    }
    if !violations.is_empty() {
        return Err(ApiError::Violations(violations));
    }

    let dao = user_dao(&state);
    if dao.exists_by_email_ignore_case(&body.email)? {
        return Err(ApiError::bad_request(
            "A user with this email already exists",
        ));
    }
    // legacy behavior: when sharedLibraries is not provided, all libraries are shared by default
    let (shared_all, shared_ids) = match &body.shared_libraries {
        None => (true, BTreeSet::new()),
        Some(sl) if sl.all => (true, BTreeSet::new()),
        Some(sl) => (false, sl.library_ids.clone()),
    };
    let user = KomgaUser {
        id: String::new(),
        email: body.email.clone(),
        password: bcrypt::hash(&body.password, 10)
            .map_err(|e| ApiError::Internal(e.to_string()))?,
        roles: values_of(&body.roles),
        shared_all_libraries: shared_all,
        shared_libraries_ids: shared_ids,
        restrictions: ContentRestrictions::new(
            age_restriction_of(body.age_restriction),
            body.labels_allow.unwrap_or_default(),
            body.labels_exclude.unwrap_or_default(),
        ),
        created_date: now_utc(),
        last_modified_date: now_utc(),
    };
    let id = dao.insert(&user)?;
    let created = dao
        .find_by_id(&id)?
        .ok_or_else(|| ApiError::Internal("user not found after insert".into()))?;
    Ok((StatusCode::CREATED, Json(UserDto::from(&created))))
}

async fn update_user(
    State(state): State<AppState>,
    auth: RequireAuth,
    Path(id): Path<String>,
    Json(patch): Json<UserUpdateDto>,
) -> Result<StatusCode, ApiError> {
    auth.0.require_admin()?;
    if auth.0.user.id == id {
        return Err(ApiError::forbidden(""));
    }
    let (user, expire_session) = state.user_auth.update_user(&id, &auth.0.user.id, |user| {
        if let Some(roles) = &patch.roles {
            // komga NPEs on explicit null (roles!!); aligned here as a 500
            let roles = roles
                .as_ref()
                .ok_or_else(|| ApiError::Internal("null value for roles".into()))?;
            user.roles = values_of(&roles.iter().cloned().collect::<Vec<_>>());
        }
        if let Some(shared) = &patch.shared_libraries {
            let shared = shared
                .as_ref()
                .ok_or_else(|| ApiError::Internal("null value for sharedLibraries".into()))?;
            user.shared_all_libraries = shared.all;
            user.shared_libraries_ids = if shared.all {
                BTreeSet::new()
            } else {
                shared.library_ids.clone()
            };
        }
        let restrictions = &mut user.restrictions;
        if let Some(age_restriction) = &patch.age_restriction {
            restrictions.age_restriction = age_restriction_of(*age_restriction);
        }
        if let Some(labels) = &patch.labels_allow {
            restrictions.labels_allow =
                komga_core::model::user::lower_not_blank(labels.clone().unwrap_or_default());
        }
        if let Some(labels) = &patch.labels_exclude {
            restrictions.labels_exclude =
                komga_core::model::user::lower_not_blank(labels.clone().unwrap_or_default());
        }
        // ContentRestrictions construction semantics: allow minus exclude
        user.restrictions = ContentRestrictions::new(
            user.restrictions.age_restriction,
            user.restrictions.labels_allow.clone(),
            user.restrictions.labels_exclude.clone(),
        );
        Ok(())
    })?;
    let _ = state.events.send(DomainEvent::UserUpdated {
        user,
        expire_session,
    });
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_user(
    State(state): State<AppState>,
    auth: RequireAuth,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    auth.0.require_admin()?;
    if auth.0.user.id == id {
        return Err(ApiError::forbidden(""));
    }
    let user = state.user_auth.delete_user(&id)?;
    // Java publishes UserUpdated (not UserDeleted) on delete, with sessions expired
    let _ = state.events.send(DomainEvent::UserUpdated {
        user,
        expire_session: true,
    });
    Ok(StatusCode::NO_CONTENT)
}

async fn update_password_by_id(
    State(state): State<AppState>,
    auth: RequireAuth,
    Path(id): Path<String>,
    Json(body): Json<PasswordUpdateDto>,
) -> Result<StatusCode, ApiError> {
    validate_password(&body.password)?;
    if !auth.0.is_admin() && auth.0.user.id != id {
        return Err(ApiError::forbidden(""));
    }
    let password =
        bcrypt::hash(&body.password, 10).map_err(|e| ApiError::Internal(e.to_string()))?;
    let (user, expire_session) = state.user_auth.update_user(&id, &auth.0.user.id, |user| {
        user.password = password;
        Ok(())
    })?;
    let _ = state.events.send(DomainEvent::UserUpdated {
        user,
        expire_session,
    });
    Ok(StatusCode::NO_CONTENT)
}

async fn my_authentication_activity(
    State(state): State<AppState>,
    auth: RequireAuth,
    query: QueryPageable,
) -> Result<Json<Page<AuthenticationActivityDto>>, ApiError> {
    let (items, total) = activity_page(&state, &query.pageable, Some(&auth.0.user))?;
    Ok(Json(Page::of(
        items.iter().map(AuthenticationActivityDto::from).collect(),
        total,
        &query.pageable,
    )))
}

async fn authentication_activity(
    State(state): State<AppState>,
    auth: RequireAuth,
    query: QueryPageable,
) -> Result<Json<Page<AuthenticationActivityDto>>, ApiError> {
    auth.0.require_admin()?;
    let (items, total) = activity_page(&state, &query.pageable, None)?;
    Ok(Json(Page::of(
        items.iter().map(AuthenticationActivityDto::from).collect(),
        total,
        &query.pageable,
    )))
}

/// Defaults to dateTime desc; returns everything when unpaged.
fn activity_page(
    state: &AppState,
    pageable: &Pageable,
    user: Option<&KomgaUser>,
) -> Result<(Vec<komga_core::model::user::AuthenticationActivity>, u64), ApiError> {
    let dao = user_dao(state);
    let (limit, offset) = if pageable.unpaged {
        (None, 0)
    } else {
        (Some(pageable.size), pageable.offset() as u32)
    };
    let (items, total) = match user {
        Some(user) => dao.find_activities_by_user(&user.id, &user.email, limit, offset)?,
        None => dao.find_all_activities(limit, offset)?,
    };
    Ok((items, total as u64))
}

async fn latest_authentication_activity(
    State(state): State<AppState>,
    auth: RequireAuth,
    Path(id): Path<String>,
    query: QueryPageable,
) -> Result<Json<AuthenticationActivityDto>, ApiError> {
    if !auth.0.is_admin() && auth.0.user.id != id {
        return Err(ApiError::forbidden(""));
    }
    let dao = user_dao(&state);
    let user = dao
        .find_by_id(&id)?
        .ok_or_else(|| ApiError::not_found(""))?;
    let api_key_id = query.params.first("apikey_id").map(str::to_string);
    let activity = dao
        .find_most_recent_activity_by_user(&user.id, &user.email, api_key_id.as_deref())?
        .ok_or_else(|| ApiError::not_found(""))?;
    Ok(Json(AuthenticationActivityDto::from(&activity)))
}

// region sessions

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MySessionDto {
    id: String,
    creation_time: String,
    last_accessed_time: String,
    current: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct UserSessionDto {
    id: String,
    user_id: String,
    /// `None` when the session outlives its user row (e.g. a Java-written user id).
    email: Option<String>,
    creation_time: String,
    last_accessed_time: String,
}

fn session_instant(millis: u64) -> String {
    komga_core::time_codec::format_offset_date_time(
        time::OffsetDateTime::from_unix_timestamp_nanos(millis as i128 * 1_000_000).unwrap(),
    )
}

/// The session store only reads in full, so sorting and paging happen in memory.
fn page_of_sorted<T: Serialize>(items: Vec<T>, pageable: &Pageable) -> Page<T> {
    let total = items.len() as u64;
    let content = if pageable.unpaged {
        items
    } else {
        items
            .into_iter()
            .skip(pageable.offset().min(total) as usize)
            .take(pageable.size as usize)
            .collect()
    };
    Page::of(content, total, pageable)
}

async fn my_sessions(
    State(state): State<AppState>,
    auth: RequireAuth,
    query: QueryPageable,
) -> Result<Json<Page<MySessionDto>>, ApiError> {
    let current = auth.0.session_id.as_deref();
    let mut sessions: Vec<MySessionDto> = state
        .sessions
        .all()?
        .into_iter()
        .filter(|(_, data)| data.user_id == auth.0.user.id)
        .map(|(id, data)| MySessionDto {
            current: current == Some(id.as_str()),
            id,
            creation_time: session_instant(data.created_at),
            last_accessed_time: session_instant(data.last_accessed_at),
        })
        .collect();
    sessions.sort_by(|a, b| b.last_accessed_time.cmp(&a.last_accessed_time));
    Ok(Json(page_of_sorted(sessions, &query.pageable)))
}

async fn delete_my_session(
    State(state): State<AppState>,
    auth: RequireAuth,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    match state.sessions.get(&id)? {
        Some(data) if data.user_id == auth.0.user.id => {
            state.sessions.invalidate(&id)?;
            Ok(StatusCode::NO_CONTENT)
        }
        // reads as missing whether the session is gone or belongs to someone else
        _ => Err(ApiError::not_found("")),
    }
}

async fn sessions(
    State(state): State<AppState>,
    auth: RequireAuth,
    query: QueryPageable,
) -> Result<Json<Page<UserSessionDto>>, ApiError> {
    auth.0.require_admin()?;
    let emails: std::collections::HashMap<String, String> = user_dao(&state)
        .find_all()?
        .into_iter()
        .map(|u| (u.id, u.email))
        .collect();
    let mut sessions: Vec<UserSessionDto> = state
        .sessions
        .all()?
        .into_iter()
        .map(|(id, data)| UserSessionDto {
            email: emails.get(&data.user_id).cloned(),
            id,
            user_id: data.user_id,
            creation_time: session_instant(data.created_at),
            last_accessed_time: session_instant(data.last_accessed_at),
        })
        .collect();
    sessions.sort_by(|a, b| b.last_accessed_time.cmp(&a.last_accessed_time));
    Ok(Json(page_of_sorted(sessions, &query.pageable)))
}

async fn delete_session(
    State(state): State<AppState>,
    auth: RequireAuth,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    auth.0.require_admin()?;
    state.sessions.invalidate(&id)?;
    Ok(StatusCode::NO_CONTENT)
}

// endregion

async fn my_api_keys(
    auth: RequireAuth,
    State(state): State<AppState>,
) -> Result<Json<Vec<ApiKeyDto>>, ApiError> {
    let keys = user_dao(&state).find_api_keys_by_user_id(&auth.0.user.id)?;
    Ok(Json(keys.iter().map(ApiKeyDto::of_redacted).collect()))
}

async fn create_api_key(
    State(state): State<AppState>,
    auth: RequireAuth,
    Json(body): Json<ApiKeyRequestDto>,
) -> Result<Json<ApiKeyDto>, ApiError> {
    if body.comment.trim().is_empty() {
        return Err(ApiError::Violations(vec![Violation {
            field_name: "comment".into(),
            message: "must not be blank".into(),
        }]));
    }
    let dao = user_dao(&state);
    if dao.exists_api_key_by_comment_and_user_id(&body.comment, &auth.0.user.id)? {
        return Err(ApiError::bad_request(komga_core::error::codes::ERR_1034));
    }
    let (api_key, plain) =
        crate::service::user::mint_api_key(state.db.clone(), &auth.0.user.id, &body.comment)
            .map_err(|_| ApiError::Status {
                status: StatusCode::SERVICE_UNAVAILABLE,
                message: "Failed to generate API key".into(),
            })?;
    let mut dto = ApiKeyDto::of(&api_key);
    dto.key = plain; // the plaintext is returned only this once
    Ok(Json(dto))
}

async fn delete_api_key(
    State(state): State<AppState>,
    auth: RequireAuth,
    Path(key_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let dao = user_dao(&state);
    if !dao.exists_api_key_by_id_and_user_id(&key_id, &auth.0.user.id)? {
        return Err(ApiError::not_found(""));
    }
    dao.delete_api_key_by_id_and_user_id(&key_id, &auth.0.user.id)?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::test_kmrs_db;
    use axum::body::Body;
    use axum::http::Request;
    use komga_core::model::user::ApiKey;
    use komga_db::pool::Database;
    use komga_db::{Migrator, Placeholders};
    use tower::ServiceExt;

    fn test_state() -> (AppState, tokio::sync::watch::Receiver<bool>) {
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
        let config = crate::config::ServerConfig::from_env();
        crate::state::test_app_state(config, db, tasks_db, test_kmrs_db())
    }

    fn test_router(state: AppState) -> Router {
        Router::new()
            .merge(router())
            .layer(axum::middleware::from_fn_with_state(
                state.clone(),
                crate::auth::auth_middleware,
            ))
            .with_state(state)
    }

    fn seed_user(state: &AppState, email: &str) -> String {
        let dao = UserDao::new(state.db.clone());
        let user_id = dao
            .insert(&KomgaUser {
                id: String::new(),
                email: email.to_string(),
                password: bcrypt::hash("pass", 10).unwrap(),
                roles: [UserRole::Admin].into_iter().collect(),
                shared_libraries_ids: BTreeSet::new(),
                shared_all_libraries: true,
                restrictions: ContentRestrictions::default(),
                created_date: now_utc(),
                last_modified_date: now_utc(),
            })
            .unwrap();
        dao.insert_api_key(&ApiKey {
            id: String::new(),
            user_id: user_id.clone(),
            key: crate::auth::sha512_hex("secret"),
            comment: "test".into(),
            created_date: now_utc(),
            last_modified_date: now_utc(),
        })
        .unwrap();
        user_id
    }

    /// Activity is persisted on a spawned task; poll until it lands.
    async fn wait_activity(
        state: &AppState,
        user_id: &str,
        email: &str,
    ) -> komga_core::model::user::AuthenticationActivity {
        let dao = UserDao::new(state.db.clone());
        for _ in 0..100 {
            if let Ok(Some(activity)) = dao.find_most_recent_activity_by_user(user_id, email, None)
            {
                return activity;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("authentication activity was not recorded");
    }

    #[tokio::test]
    async fn api_key_success_records_user_id_and_email() {
        let (state, _rx) = test_state();
        let user_id = seed_user(&state, "Admin@Example.com");
        let app = test_router(state.clone());
        let request = Request::builder()
            .method("GET")
            .uri("/api/v2/users/me")
            .header("X-API-Key", "secret")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let activity = wait_activity(&state, &user_id, "Admin@Example.com").await;
        assert_eq!(activity.user_id.as_deref(), Some(user_id.as_str()));
        assert_eq!(activity.email.as_deref(), Some("Admin@Example.com"));
        assert!(activity.success);
        assert_eq!(activity.source.as_deref(), Some("ApiKey"));
    }

    #[tokio::test]
    async fn password_success_records_user_id_and_canonical_email() {
        let (state, _rx) = test_state();
        let user_id = seed_user(&state, "Admin@Example.com");
        let app = test_router(state.clone());
        // credentials in a different case than the stored email
        let credentials = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            "admin@example.com:pass",
        );
        let request = Request::builder()
            .method("GET")
            .uri("/api/v2/users/me")
            .header("Authorization", format!("Basic {credentials}"))
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // LoginListener records the canonical user.email, not the submitted principal
        let activity = wait_activity(&state, &user_id, "Admin@Example.com").await;
        assert_eq!(activity.user_id.as_deref(), Some(user_id.as_str()));
        assert_eq!(activity.email.as_deref(), Some("Admin@Example.com"));
        assert!(activity.success);
        assert_eq!(activity.source.as_deref(), Some("Password"));
    }

    async fn activity_count(state: &AppState, user_id: &str, email: &str) -> i64 {
        UserDao::new(state.db.clone())
            .find_activities_by_user(user_id, email, None, 0)
            .unwrap()
            .1
    }

    /// Waits until `expected` activities are persisted (they are recorded on a spawned task).
    async fn wait_activity_count(state: &AppState, user_id: &str, email: &str, expected: i64) {
        for _ in 0..100 {
            if activity_count(state, user_id, email).await >= expected {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("expected {expected} activities");
    }

    /// Waits until at least `min` activities landed, then lets stragglers settle and returns
    /// the exact count (recording happens on a spawned task per request).
    async fn settled_activity_count(state: &AppState, user_id: &str, email: &str, min: i64) -> i64 {
        wait_activity_count(state, user_id, email, min).await;
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        activity_count(state, user_id, email).await
    }

    fn basic_request(credentials: &str, session: Option<(&str, &str)>) -> Request<Body> {
        let mut builder = Request::builder()
            .method("GET")
            .uri("/api/v2/users/me")
            .header(
                "Authorization",
                format!(
                    "Basic {}",
                    base64::Engine::encode(&base64::engine::general_purpose::STANDARD, credentials)
                ),
            );
        if let Some((name, value)) = session {
            builder = builder.header(name, value);
        }
        builder.body(Body::empty()).unwrap()
    }

    /// Spring's BasicAuthenticationFilter skips re-authentication when the session already
    /// holds the same user: Basic sent together with that session keeps it (header or cookie,
    /// email in any case), with no new session, no session header or cookie in the response
    /// and no activity row.
    #[tokio::test]
    async fn basic_with_own_session_reuses_it() {
        let (state, _rx) = test_state();
        let user_id = seed_user(&state, "Admin@Example.com");
        let session_id = state.sessions.create(&user_id).unwrap();
        let cookie = format!("{}={session_id}", crate::auth::SESSION_COOKIE_NAME);
        let app = test_router(state.clone());

        for session in [
            ("X-Auth-Token", session_id.as_str()),
            ("Cookie", cookie.as_str()),
        ] {
            let response = app
                .clone()
                .oneshot(basic_request("admin@example.com:pass", Some(session)))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert!(response.headers().get("x-auth-token").is_none());
            assert!(response.headers().get("set-cookie").is_none());
        }
        assert_eq!(state.sessions.all().unwrap().len(), 1);
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert_eq!(
            activity_count(&state, &user_id, "Admin@Example.com").await,
            0
        );
    }

    /// A session that belongs to someone else, or no longer exists, doesn't stand in for the
    /// credentials: Basic re-authenticates and issues a new session. An empty `X-Auth-Token`
    /// is how clients such as KMReader ask for the session id on login.
    #[tokio::test]
    async fn basic_with_foreign_or_unknown_session_reauthenticates() {
        let (state, _rx) = test_state();
        let user_id = seed_user(&state, "admin@example.com");
        let other_id = UserDao::new(state.db.clone())
            .insert(&KomgaUser {
                id: String::new(),
                email: "other@example.com".into(),
                password: bcrypt::hash("other", 4).unwrap(),
                roles: BTreeSet::new(),
                shared_libraries_ids: BTreeSet::new(),
                shared_all_libraries: true,
                restrictions: ContentRestrictions::default(),
                created_date: now_utc(),
                last_modified_date: now_utc(),
            })
            .unwrap();
        let other_session = state.sessions.create(&other_id).unwrap();
        let app = test_router(state.clone());

        for session in [other_session.as_str(), "unknown-session", ""] {
            let response = app
                .clone()
                .oneshot(basic_request(
                    "admin@example.com:pass",
                    Some(("X-Auth-Token", session)),
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let issued = response
                .headers()
                .get("x-auth-token")
                .expect("a new session")
                .to_str()
                .unwrap();
            assert_ne!(issued, session);
            let stored = state.sessions.get(issued).unwrap().expect("stored");
            assert_eq!(stored.user_id, user_id);
        }
        // three successes from the same device collapse into one throttled row
        assert_eq!(
            settled_activity_count(&state, &user_id, "admin@example.com", 1).await,
            1
        );
    }

    /// Successful password logins are recorded once per user and device per window, like
    /// API keys per key: a client sending Basic on every request doesn't flood the activity,
    /// while a login from another device is still recorded right away.
    #[tokio::test]
    async fn password_activity_is_throttled_per_device() {
        let (state, _rx) = test_state();
        let user_id = seed_user(&state, "admin@example.com");
        let app = test_router(state.clone());

        for agent in ["reader/1", "reader/1", "browser/2"] {
            let mut request = basic_request("admin@example.com:pass", None);
            request
                .headers_mut()
                .insert("User-Agent", agent.parse().unwrap());
            let response = app.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
        }
        assert_eq!(
            settled_activity_count(&state, &user_id, "admin@example.com", 2).await,
            2
        );
    }

    /// After repeated wrong passwords from one address the account refuses password logins
    /// from it, even the right password, with the same 401 and no session. The refusal is recorded in the activity,
    /// and a device that is already signed in keeps working.
    #[tokio::test]
    async fn repeated_wrong_passwords_lock_password_logins() {
        let (state, _rx) = test_state();
        let user_id = seed_user(&state, "admin@example.com");
        let session_id = state.sessions.create(&user_id).unwrap();
        let app = test_router(state.clone());

        for _ in 0..5 {
            let response = app
                .clone()
                .oneshot(basic_request("admin@example.com:wrong", None))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }
        let response = app
            .clone()
            .oneshot(basic_request("admin@example.com:pass", None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(response.headers().get("set-cookie").is_none());
        assert_eq!(state.sessions.all().unwrap().len(), 1);

        assert_eq!(
            settled_activity_count(&state, &user_id, "admin@example.com", 6).await,
            6
        );
        let (activities, _) = UserDao::new(state.db.clone())
            .find_activities_by_user(&user_id, "admin@example.com", None, 0)
            .unwrap();
        let refused: Vec<_> = activities
            .iter()
            .filter(|a| a.error.as_deref() == Some("Too many failed attempts"))
            .collect();
        assert_eq!(refused.len(), 1);
        assert!(!refused[0].success);
        // the refusal names the account, which is known at that point
        assert_eq!(refused[0].user_id.as_deref(), Some(user_id.as_str()));

        let response = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/v2/users/me")
                    .header("X-Auth-Token", session_id)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    /// A lock belongs to the address that was guessing: the same account still signs in
    /// from another address, while the right password from the locked one stays refused.
    #[tokio::test]
    async fn a_lock_does_not_reach_the_accounts_other_addresses() {
        let (state, _rx) = test_state();
        seed_user(&state, "admin@example.com");
        let app = test_router(state.clone());
        let from = |peer: &str, credentials: &str| {
            let mut request = basic_request(credentials, None);
            let peer = std::net::SocketAddr::new(peer.parse().unwrap(), 40000);
            request
                .extensions_mut()
                .insert(axum::extract::ConnectInfo(peer));
            request
        };

        for _ in 0..5 {
            let response = app
                .clone()
                .oneshot(from("203.0.113.9", "admin@example.com:wrong"))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }
        let response = app
            .clone()
            .oneshot(from("203.0.113.9", "admin@example.com:pass"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let response = app
            .clone()
            .oneshot(from("198.51.100.7", "admin@example.com:pass"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        // the success elsewhere does not lift the lock
        let response = app
            .oneshot(from("203.0.113.9", "admin@example.com:pass"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    /// The lock comes before the cache: a source that was locked for guessing is refused
    /// even with a password that was checked from that address just before.
    #[tokio::test]
    async fn a_locked_source_is_refused_whatever_was_checked_before() {
        let (state, _rx) = test_state();
        seed_user(&state, "admin@example.com");
        let app = test_router(state.clone());
        let status = |credentials: &'static str| {
            let app = app.clone();
            async move {
                app.oneshot(basic_request(credentials, None))
                    .await
                    .unwrap()
                    .status()
            }
        };

        assert_eq!(status("admin@example.com:pass").await, StatusCode::OK);
        for _ in 0..5 {
            assert_eq!(
                status("admin@example.com:wrong").await,
                StatusCode::UNAUTHORIZED
            );
        }
        assert_eq!(
            status("admin@example.com:pass").await,
            StatusCode::UNAUTHORIZED
        );
    }

    /// A user who changes their own password keeps their sessions, and still the old
    /// password stops working at once: what was remembered belongs to the old hash.
    #[tokio::test]
    async fn a_changed_password_is_checked_again() {
        let (state, _rx) = test_state();
        seed_user(&state, "admin@example.com");
        let app = test_router(state.clone());
        let status = |credentials: &'static str| {
            let app = app.clone();
            async move {
                app.oneshot(basic_request(credentials, None))
                    .await
                    .unwrap()
                    .status()
            }
        };

        assert_eq!(status("admin@example.com:pass").await, StatusCode::OK);
        let mut change = basic_request("admin@example.com:pass", None);
        *change.method_mut() = axum::http::Method::PATCH;
        *change.uri_mut() = "/api/v2/users/me/password".parse().unwrap();
        change.headers_mut().insert(
            axum::http::header::CONTENT_TYPE,
            "application/json".parse().unwrap(),
        );
        *change.body_mut() = Body::from(r#"{"password":"newpass123"}"#);
        let response = app.clone().oneshot(change).await.unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        assert_eq!(
            status("admin@example.com:pass").await,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(status("admin@example.com:newpass123").await, StatusCode::OK);
    }

    /// Clients that log in with Basic alone (KMReader keeps the returned token) still get a
    /// session, and wrong credentials are still rejected without one.
    #[tokio::test]
    async fn basic_without_session_still_issues_one() {
        let (state, _rx) = test_state();
        seed_user(&state, "admin@example.com");
        let app = test_router(state.clone());

        let response = app
            .clone()
            .oneshot(basic_request("admin@example.com:pass", None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let cookie = response
            .headers()
            .get("set-cookie")
            .expect("a session cookie")
            .to_str()
            .unwrap();
        assert!(cookie.starts_with(&format!("{}=", crate::auth::SESSION_COOKIE_NAME)));

        let response = app
            .oneshot(basic_request("admin@example.com:wrong", None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(state.sessions.all().unwrap().len(), 1);
    }

    /// The recorded address is the connection's own unless it comes from a trusted proxy:
    /// only then does `X-Forwarded-For` name the client, read from the proxy's end.
    #[tokio::test]
    async fn activity_address_honors_forwarded_for_only_from_trusted_proxies() {
        let (mut state, _rx) = test_state();
        state.config = std::sync::Arc::new(crate::config::ServerConfig {
            trusted_proxies: crate::http::client_addr::TrustedProxies::parse(&[
                "10.0.0.0/8".to_string()
            ])
            .unwrap(),
            ..(*state.config).clone()
        });
        seed_user(&state, "admin@example.com");
        let app = test_router(state.clone());

        for (peer, user_agent) in [("203.0.113.9", "direct"), ("10.0.0.2", "proxied")] {
            let mut request = basic_request("admin@example.com:wrong", None);
            let headers = request.headers_mut();
            headers.insert("User-Agent", user_agent.parse().unwrap());
            headers.insert("X-Forwarded-For", "1.1.1.1, 198.51.100.7".parse().unwrap());
            let peer = std::net::SocketAddr::new(peer.parse().unwrap(), 40000);
            request
                .extensions_mut()
                .insert(axum::extract::ConnectInfo(peer));
            let response = app.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }

        let dao = UserDao::new(state.db.clone());
        let mut recorded = Vec::new();
        for _ in 0..100 {
            recorded = dao.find_all_activities(None, 0).unwrap().0;
            if recorded.len() >= 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let address_of = |user_agent: &str| {
            recorded
                .iter()
                .find(|activity| activity.user_agent.as_deref() == Some(user_agent))
                .and_then(|activity| activity.ip.clone())
        };
        assert_eq!(address_of("direct").as_deref(), Some("203.0.113.9"));
        assert_eq!(address_of("proxied").as_deref(), Some("198.51.100.7"));
    }

    /// 1.29 made API-key authentication stateless (request-scoped SecurityContext, no session
    /// persistence): a session presented alongside the key neither suppresses re-authentication
    /// nor is marked with the key. Repeated successes with the same key collapse into a single
    /// throttled activity row.
    #[tokio::test]
    async fn api_key_with_session_is_stateless_and_activity_throttled() {
        let (state, _rx) = test_state();
        let user_id = seed_user(&state, "admin@example.com");
        let session_id = state.sessions.create(&user_id).unwrap();
        let app = test_router(state.clone());

        let call = |key: &str| {
            let app = app.clone();
            let session_id = session_id.clone();
            let key = key.to_string();
            async move {
                app.oneshot(
                    Request::builder()
                        .method("GET")
                        .uri("/api/v2/users/me")
                        .header("X-API-Key", key)
                        .header("X-Auth-Token", session_id)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap()
            }
        };

        for _ in 0..3 {
            let response = call("secret").await;
            assert_eq!(response.status(), StatusCode::OK);
            assert!(
                response.headers().get("set-cookie").is_none(),
                "API-key authentication must not issue a session cookie"
            );
        }
        assert_eq!(
            settled_activity_count(&state, &user_id, "admin@example.com", 1).await,
            1
        );
    }

    /// Without a session there is no stored context to reuse: every API-key request
    /// re-authenticates, but successful activity is throttled to one row per key per window.
    #[tokio::test]
    async fn api_key_without_session_records_activity_once() {
        let (state, _rx) = test_state();
        let user_id = seed_user(&state, "admin@example.com");
        let app = test_router(state.clone());

        for _ in 0..2 {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("GET")
                        .uri("/api/v2/users/me")
                        .header("X-API-Key", "secret")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
        }
        assert_eq!(
            settled_activity_count(&state, &user_id, "admin@example.com", 1).await,
            1
        );
    }

    /// The throttle is per key: two keys of the same user each get their own row.
    #[tokio::test]
    async fn api_key_activity_throttle_is_per_key() {
        let (state, _rx) = test_state();
        let user_id = seed_user(&state, "admin@example.com");
        UserDao::new(state.db.clone())
            .insert_api_key(&ApiKey {
                id: String::new(),
                user_id: user_id.clone(),
                key: crate::auth::sha512_hex("secret2"),
                comment: "second".into(),
                created_date: now_utc(),
                last_modified_date: now_utc(),
            })
            .unwrap();
        let app = test_router(state.clone());

        for key in ["secret", "secret", "secret2", "secret2"] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("GET")
                        .uri("/api/v2/users/me")
                        .header("X-API-Key", key)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
        }
        assert_eq!(
            settled_activity_count(&state, &user_id, "admin@example.com", 2).await,
            2
        );
    }

    /// Once the throttle window has passed, the next success records a fresh row.
    #[tokio::test]
    async fn api_key_activity_records_again_after_window() {
        let (state, _rx) = test_state();
        let user_id = seed_user(&state, "admin@example.com");
        let app = test_router(state.clone());

        let call = || {
            let app = app.clone();
            async move {
                app.oneshot(
                    Request::builder()
                        .method("GET")
                        .uri("/api/v2/users/me")
                        .header("X-API-Key", "secret")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap()
            }
        };

        assert_eq!(call().await.status(), StatusCode::OK);
        assert_eq!(
            settled_activity_count(&state, &user_id, "admin@example.com", 1).await,
            1
        );

        let (_, api_key) = UserDao::new(state.db.clone())
            .find_by_api_key(&crate::auth::sha512_hex("secret"))
            .unwrap()
            .unwrap();
        state.api_key_activity_throttle.expire(&api_key.id);

        assert_eq!(call().await.status(), StatusCode::OK);
        assert_eq!(
            settled_activity_count(&state, &user_id, "admin@example.com", 2).await,
            2
        );
    }

    /// 1.29's API-key filter never creates a session: no Set-Cookie/X-Auth-Token on the
    /// response, and a cookie the client might still hold from an earlier login is not
    /// touched by API-key requests.
    #[tokio::test]
    async fn api_key_never_issues_session_cookie() {
        let (state, _rx) = test_state();
        let user_id = seed_user(&state, "admin@example.com");
        let app = test_router(state.clone());

        for _ in 0..2 {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("GET")
                        .uri("/api/v2/users/me")
                        .header("X-API-Key", "secret")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert!(response.headers().get("set-cookie").is_none());
            assert!(response.headers().get("x-auth-token").is_none());
        }

        assert_eq!(
            settled_activity_count(&state, &user_id, "admin@example.com", 1).await,
            1
        );
    }

    /// LoginListener.onFailure for an ApiKey source records the masked key (XXH3-128) as
    /// apiKeyComment and the BadCredentialsException message; user/email stay null.
    #[tokio::test]
    async fn api_key_failure_records_masked_key_and_bad_credentials() {
        let (state, _rx) = test_state();
        seed_user(&state, "admin@example.com");
        let app = test_router(state.clone());
        let response = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/v2/users/me")
                    .header("X-API-Key", "wrong")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        // the failure row has no user_id/email, so poll the unfiltered listing
        let dao = UserDao::new(state.db.clone());
        let expected_comment = komga_media::hash::compute_hash_bytes(b"wrong");
        let activity = {
            let mut found = None;
            for _ in 0..100 {
                let (rows, _) = dao.find_all_activities(Some(1), 0).unwrap();
                if let Some(row) = rows.into_iter().next() {
                    found = Some(row);
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            found.expect("authentication activity was not recorded")
        };
        assert_eq!(activity.user_id, None);
        assert_eq!(activity.email, None);
        assert!(!activity.success);
        assert_eq!(activity.source.as_deref(), Some("ApiKey"));
        assert_eq!(activity.error.as_deref(), Some("Bad credentials"));
        assert_eq!(
            activity.api_key_comment.as_deref(),
            Some(expected_comment.as_str())
        );
    }

    /// Failed API-key authentications are the brute-force signal: they stay per-request and
    /// are not covered by the success throttle.
    #[tokio::test]
    async fn api_key_failure_is_not_throttled() {
        let (state, _rx) = test_state();
        seed_user(&state, "admin@example.com");
        let app = test_router(state.clone());

        for _ in 0..2 {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("GET")
                        .uri("/api/v2/users/me")
                        .header("X-API-Key", "wrong")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }

        let dao = UserDao::new(state.db.clone());
        for _ in 0..100 {
            if dao.find_all_activities(None, 0).unwrap().1 >= 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert_eq!(dao.find_all_activities(None, 0).unwrap().1, 2);
    }

    fn seed_plain_user(state: &AppState, email: &str) -> String {
        UserDao::new(state.db.clone())
            .insert(&KomgaUser {
                id: String::new(),
                email: email.to_string(),
                password: bcrypt::hash("pass", 10).unwrap(),
                roles: BTreeSet::new(),
                shared_libraries_ids: BTreeSet::new(),
                shared_all_libraries: true,
                restrictions: ContentRestrictions::default(),
                created_date: now_utc(),
                last_modified_date: now_utc(),
            })
            .unwrap()
    }

    async fn call_with_body(
        state: &AppState,
        method: &str,
        uri: &str,
        body: serde_json::Value,
    ) -> StatusCode {
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header("X-API-Key", "secret")
            .header("Content-Type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        test_router(state.clone())
            .oneshot(request)
            .await
            .unwrap()
            .status()
    }

    /// Next `UserUpdated` on the bus; anything else (notably `UserDeleted`) fails the test.
    async fn next_user_updated(rx: &mut crate::events::DomainReceiver) -> (String, bool) {
        let event = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        match event {
            crate::events::DomainEvent::UserUpdated {
                user,
                expire_session,
            } => (user.id, expire_session),
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn update_user_roles_expires_sessions_and_emits() {
        let (state, _rx) = test_state();
        seed_user(&state, "admin@b.c");
        let target = seed_plain_user(&state, "user@b.c");
        let sid = state.sessions.create(&target).unwrap();
        let mut events = state.events.subscribe();

        let status = call_with_body(
            &state,
            "PATCH",
            &format!("/api/v2/users/{target}"),
            serde_json::json!({"roles": ["ADMIN"]}),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        assert_eq!(next_user_updated(&mut events).await, (target.clone(), true));
        assert!(state.sessions.get(&sid).unwrap().is_none());
    }

    #[tokio::test]
    async fn update_user_noop_keeps_sessions_and_emits_unexpired() {
        let (state, _rx) = test_state();
        seed_user(&state, "admin@b.c");
        let target = seed_plain_user(&state, "user@b.c");
        let sid = state.sessions.create(&target).unwrap();
        let mut events = state.events.subscribe();

        let status = call_with_body(
            &state,
            "PATCH",
            &format!("/api/v2/users/{target}"),
            serde_json::json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        assert_eq!(
            next_user_updated(&mut events).await,
            (target.clone(), false)
        );
        assert!(state.sessions.get(&sid).unwrap().is_some());
    }

    #[tokio::test]
    async fn delete_user_emits_updated_expired_not_deleted() {
        let (state, _rx) = test_state();
        seed_user(&state, "admin@b.c");
        let target = seed_plain_user(&state, "user@b.c");
        let mut events = state.events.subscribe();

        let request = Request::builder()
            .method("DELETE")
            .uri(format!("/api/v2/users/{target}"))
            .header("X-API-Key", "secret")
            .body(Body::empty())
            .unwrap();
        let status = test_router(state.clone())
            .oneshot(request)
            .await
            .unwrap()
            .status();
        assert_eq!(status, StatusCode::NO_CONTENT);

        // Java parity: delete publishes UserUpdated (not UserDeleted) with sessions expired
        assert_eq!(next_user_updated(&mut events).await, (target, true));
    }

    #[tokio::test]
    async fn password_change_flag_follows_whose_password() {
        let (state, _rx) = test_state();
        let admin = seed_user(&state, "admin@b.c");
        let target = seed_plain_user(&state, "user@b.c");
        let mut events = state.events.subscribe();

        // admin changes someone else's password: sessions expire
        let status = call_with_body(
            &state,
            "PATCH",
            &format!("/api/v2/users/{target}/password"),
            serde_json::json!({"password": "newpass123"}),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert_eq!(next_user_updated(&mut events).await, (target, true));

        // changing your own password keeps the session
        let status = call_with_body(
            &state,
            "PATCH",
            "/api/v2/users/me/password",
            serde_json::json!({"password": "newpass123"}),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert_eq!(next_user_updated(&mut events).await, (admin, false));
    }

    async fn json_body(response: axum::response::Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn session_request(method: &str, uri: &str, session_id: &str) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .header("X-Auth-Token", session_id)
            .body(Body::empty())
            .unwrap()
    }

    #[tokio::test]
    async fn my_sessions_scopes_to_owner_and_marks_current() {
        let (state, _rx) = test_state();
        let admin_id = seed_user(&state, "admin@s.c");
        let user_id = seed_plain_user(&state, "user@s.c");
        let current = state.sessions.create(&user_id).unwrap();
        let other = state.sessions.create(&user_id).unwrap();
        let admins = state.sessions.create(&admin_id).unwrap();

        // anonymous is rejected
        let status = test_router(state.clone())
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/v2/users/me/sessions")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
            .status();
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        let response = test_router(state.clone())
            .oneshot(session_request(
                "GET",
                "/api/v2/users/me/sessions",
                &current,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;
        let sessions = body["content"].as_array().unwrap();
        assert_eq!(sessions.len(), 2);
        assert_eq!(body["totalElements"], 2);
        for session in sessions {
            assert!(session["creationTime"].as_str().unwrap().ends_with('Z'));
            assert_eq!(
                session["current"].as_bool().unwrap(),
                *session["id"].as_str().unwrap() == current
            );
        }
        assert!(!sessions.iter().any(|s| s["id"] == admins));

        // page params slice the sorted list
        let body = json_body(
            test_router(state.clone())
                .oneshot(session_request(
                    "GET",
                    "/api/v2/users/me/sessions?page=1&size=1",
                    &current,
                ))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(body["content"].as_array().unwrap().len(), 1);
        assert_eq!(body["totalElements"], 2);
        assert_eq!(body["totalPages"], 2);
        assert_eq!(body["number"], 1);
        assert_eq!(body["last"], true);

        // someone else's session reads as missing
        let status = test_router(state.clone())
            .oneshot(session_request(
                "DELETE",
                &format!("/api/v2/users/me/sessions/{admins}"),
                &current,
            ))
            .await
            .unwrap()
            .status();
        assert_eq!(status, StatusCode::NOT_FOUND);

        let status = test_router(state.clone())
            .oneshot(session_request(
                "DELETE",
                &format!("/api/v2/users/me/sessions/{other}"),
                &current,
            ))
            .await
            .unwrap()
            .status();
        assert_eq!(status, StatusCode::NO_CONTENT);
        let body = json_body(
            test_router(state.clone())
                .oneshot(session_request(
                    "GET",
                    "/api/v2/users/me/sessions",
                    &current,
                ))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(body["content"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn sessions_admin_listing_exposes_owner_and_kicks() {
        let (state, _rx) = test_state();
        let admin_id = seed_user(&state, "admin@t.c");
        let user_id = seed_plain_user(&state, "user@t.c");
        let user_session = state.sessions.create(&user_id).unwrap();
        state.sessions.create(&admin_id).unwrap();

        // non-admin is rejected on both verbs
        let status = test_router(state.clone())
            .oneshot(session_request(
                "GET",
                "/api/v2/users/sessions",
                &user_session,
            ))
            .await
            .unwrap()
            .status();
        assert_eq!(status, StatusCode::FORBIDDEN);
        let status = test_router(state.clone())
            .oneshot(session_request(
                "DELETE",
                &format!("/api/v2/users/sessions/{user_session}"),
                &user_session,
            ))
            .await
            .unwrap()
            .status();
        assert_eq!(status, StatusCode::FORBIDDEN);

        let response = test_router(state.clone())
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/v2/users/sessions")
                    .header("X-API-Key", "secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;
        let sessions = body["content"].as_array().unwrap();
        assert_eq!(sessions.len(), 2);
        assert_eq!(body["totalElements"], 2);
        let row = sessions.iter().find(|s| s["id"] == user_session).unwrap();
        assert_eq!(row["userId"], user_id);
        assert_eq!(row["email"], "user@t.c");
        assert!(row["creationTime"].as_str().unwrap().ends_with('Z'));
        assert!(row["lastAccessedTime"].as_str().unwrap().ends_with('Z'));

        // API-key auth has no session, so nothing is marked current
        let body = json_body(
            test_router(state.clone())
                .oneshot(
                    Request::builder()
                        .method("GET")
                        .uri("/api/v2/users/me/sessions")
                        .header("X-API-Key", "secret")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap(),
        )
        .await;
        assert!(body["content"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["current"] == false));

        let status = test_router(state.clone())
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/api/v2/users/sessions/{user_session}"))
                    .header("X-API-Key", "secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
            .status();
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert!(state.sessions.get(&user_session).unwrap().is_none());
    }
}
