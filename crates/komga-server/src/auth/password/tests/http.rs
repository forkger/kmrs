use super::*;
use crate::state::AppState;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use tower::ServiceExt;

fn test_state() -> AppState {
    let db = Database::open_in_memory(true).unwrap();
    let migrations = komga_db::main_migrations();
    komga_db::Migrator::new(&migrations, komga_db::Placeholders::default())
        .migrate(&db.rw().unwrap())
        .unwrap();
    let tasks_db = Database::open_in_memory(false).unwrap();
    let migrations = komga_db::tasks_migrations();
    komga_db::Migrator::new(&migrations, komga_db::Placeholders::default())
        .migrate(&tasks_db.rw().unwrap())
        .unwrap();
    let (mut state, _rx) = crate::state::test_app_state(
        crate::config::ServerConfig::from_env(),
        db,
        tasks_db,
        crate::state::test_kmrs_db(),
    );
    state.user_auth = Arc::new(UserAuthentication {
        checks: Arc::new(Semaphore::new(1)),
        ..UserAuthentication::new(state.db.clone(), state.sessions.clone())
    });
    state
}

fn test_router(state: AppState) -> Router {
    Router::new()
        .merge(crate::api::users::router())
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::auth::auth_middleware,
        ))
        .with_state(state)
}

fn seed_user(state: &AppState, email: &str, roles: &[UserRole]) -> String {
    UserDao::new(state.db.clone())
        .insert(&KomgaUser {
            id: String::new(),
            email: email.into(),
            password: bcrypt::hash("pass", 4).unwrap(),
            roles: roles.iter().copied().collect(),
            shared_all_libraries: true,
            shared_libraries_ids: Default::default(),
            restrictions: Default::default(),
            created_date: komga_core::time_codec::now_utc(),
            last_modified_date: komga_core::time_codec::now_utc(),
        })
        .unwrap()
}

fn basic_request(credentials: &str, session: Option<&str>) -> Request<Body> {
    use base64::Engine;
    let mut builder = Request::builder().uri("/api/v2/users/me").header(
        "Authorization",
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(credentials)
        ),
    );
    if let Some(session) = session {
        builder = builder.header("X-Auth-Token", session);
    }
    builder.body(Body::empty()).unwrap()
}

#[test]
fn password_logins_complete_with_a_saturated_request_pool() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let state = test_state();
        seed_user(&state, "admin@example.com", &[UserRole::Admin]);
        let entered = Arc::new(tokio::sync::Barrier::new(3));
        let request_entered = entered.clone();
        let app = test_router(state)
            .layer(axum::middleware::from_fn(
                move |request: Request<Body>, next: axum::middleware::Next| {
                    let entered = request_entered.clone();
                    async move {
                        entered.wait().await;
                        next.run(request).await
                    }
                },
            ))
            .layer(axum::middleware::from_fn(
                crate::http::offload::offload_middleware,
            ));
        let first = tokio::spawn(
            app.clone()
                .oneshot(basic_request("admin@example.com:pass", None)),
        );
        let second = tokio::spawn(app.oneshot(basic_request("admin@example.com:pass", None)));
        entered.wait().await;
        assert_eq!(first.await.unwrap().unwrap().status(), StatusCode::OK);
        assert_eq!(second.await.unwrap().unwrap().status(), StatusCode::OK);
    });
}

#[tokio::test]
async fn queued_password_login_rejects_password_reset() {
    let state = test_state();
    let admin_id = seed_user(&state, "admin@example.com", &[UserRole::Admin]);
    let target_id = seed_user(&state, "reader@example.com", &[]);
    let admin_session = state.sessions.create(&admin_id).unwrap();
    let app = test_router(state.clone());
    let held = state
        .user_auth
        .checks
        .clone()
        .acquire_owned()
        .await
        .unwrap();
    let pending = app
        .clone()
        .oneshot(basic_request("reader@example.com:pass", None));
    tokio::pin!(pending);
    assert!(futures_util::poll!(pending.as_mut()).is_pending());

    let reset = Request::builder()
        .method("PATCH")
        .uri(format!("/api/v2/users/{target_id}/password"))
        .header("X-Auth-Token", admin_session)
        .header("Content-Type", "application/json")
        .body(Body::from(r#"{"password":"newpass123"}"#))
        .unwrap();
    assert_eq!(
        app.clone().oneshot(reset).await.unwrap().status(),
        StatusCode::NO_CONTENT
    );
    drop(held);
    let response = pending.await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(response.headers().get("set-cookie").is_none());
    assert!(state
        .sessions
        .all()
        .unwrap()
        .iter()
        .all(|(_, session)| session.user_id != target_id));
    assert_eq!(
        app.oneshot(basic_request("reader@example.com:newpass123", None))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn queued_password_login_rejects_deleted_user() {
    let state = test_state();
    let admin_id = seed_user(&state, "admin@example.com", &[UserRole::Admin]);
    let target_id = seed_user(&state, "reader@example.com", &[]);
    let admin_session = state.sessions.create(&admin_id).unwrap();
    let app = test_router(state.clone());
    let held = state
        .user_auth
        .checks
        .clone()
        .acquire_owned()
        .await
        .unwrap();
    let pending = app
        .clone()
        .oneshot(basic_request("reader@example.com:pass", None));
    tokio::pin!(pending);
    assert!(futures_util::poll!(pending.as_mut()).is_pending());

    let delete = Request::builder()
        .method("DELETE")
        .uri(format!("/api/v2/users/{target_id}"))
        .header("X-Auth-Token", admin_session)
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.oneshot(delete).await.unwrap().status(),
        StatusCode::NO_CONTENT
    );
    drop(held);
    let response = pending.await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(response.headers().get("set-cookie").is_none());
    assert!(state
        .sessions
        .all()
        .unwrap()
        .iter()
        .all(|(_, session)| session.user_id != target_id));
}

#[tokio::test]
async fn queued_password_login_uses_current_permissions() {
    let state = test_state();
    let admin_id = seed_user(&state, "admin@example.com", &[UserRole::Admin]);
    let target_id = seed_user(&state, "reader@example.com", &[UserRole::Admin]);
    let admin_session = state.sessions.create(&admin_id).unwrap();
    let app = test_router(state.clone());
    let held = state
        .user_auth
        .checks
        .clone()
        .acquire_owned()
        .await
        .unwrap();
    let mut request = basic_request("reader@example.com:pass", None);
    *request.uri_mut() = "/api/v2/users".parse().unwrap();
    let pending = app.clone().oneshot(request);
    tokio::pin!(pending);
    assert!(futures_util::poll!(pending.as_mut()).is_pending());

    let update = Request::builder()
        .method("PATCH")
        .uri(format!("/api/v2/users/{target_id}"))
        .header("X-Auth-Token", admin_session)
        .header("Content-Type", "application/json")
        .body(Body::from(r#"{"roles":[]}"#))
        .unwrap();
    assert_eq!(
        app.oneshot(update).await.unwrap().status(),
        StatusCode::NO_CONTENT
    );
    drop(held);
    assert_eq!(pending.await.unwrap().status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn saturated_password_checks_answer_503_and_count_nothing() {
    let state = test_state();
    seed_user(&state, "admin@example.com", &[UserRole::Admin]);
    let app = test_router(state.clone());
    let held = state
        .user_auth
        .checks
        .clone()
        .acquire_owned()
        .await
        .unwrap();
    let response = app
        .clone()
        .oneshot(basic_request("admin@example.com:pass", None))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(response.headers().get("retry-after").unwrap(), "1");
    assert!(response
        .extensions()
        .get::<crate::http::request_trace::DeliberateRefusal>()
        .is_some());
    assert!(response.headers().get("set-cookie").is_none());
    assert_eq!(state.user_auth.throttle.len(), 0);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let (activities, _) = UserDao::new(state.db.clone())
        .find_all_activities(None, 0)
        .unwrap();
    assert!(activities.is_empty());

    drop(held);
    assert_eq!(
        app.oneshot(basic_request("admin@example.com:pass", None))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn only_password_checks_wait_for_a_permit() {
    let state = test_state();
    let user_id = seed_user(&state, "admin@example.com", &[UserRole::Admin]);
    let session_id = state.sessions.create(&user_id).unwrap();
    let app = test_router(state.clone());
    for _ in 0..5 {
        assert_eq!(
            app.clone()
                .oneshot(basic_request("admin@example.com:wrong", None))
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
    }

    let _held = state
        .user_auth
        .checks
        .clone()
        .acquire_owned()
        .await
        .unwrap();
    for (request, expected) in [
        (
            basic_request("admin@example.com:pass", None),
            StatusCode::UNAUTHORIZED,
        ),
        (
            basic_request("nobody@example.com:pass", None),
            StatusCode::UNAUTHORIZED,
        ),
        (
            basic_request("admin@example.com:pass", Some(&session_id)),
            StatusCode::OK,
        ),
    ] {
        assert_eq!(
            app.clone().oneshot(request).await.unwrap().status(),
            expected
        );
    }
}

#[tokio::test]
async fn cached_password_login_still_issues_a_session() {
    let state = test_state();
    seed_user(&state, "admin@example.com", &[UserRole::Admin]);
    let app = test_router(state.clone());
    let first = app
        .clone()
        .oneshot(basic_request("admin@example.com:pass", None))
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    assert!(first.headers().get("set-cookie").is_some());

    let _held = state
        .user_auth
        .checks
        .clone()
        .acquire_owned()
        .await
        .unwrap();
    let cached = app
        .oneshot(basic_request("admin@example.com:pass", None))
        .await
        .unwrap();
    assert_eq!(cached.status(), StatusCode::OK);
    assert!(cached.headers().get("set-cookie").is_some());
    assert_ne!(
        first.headers().get("set-cookie"),
        cached.headers().get("set-cookie")
    );
    assert_eq!(state.sessions.all().unwrap().len(), 2);
}
