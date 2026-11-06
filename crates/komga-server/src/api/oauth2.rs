//! OAuth2/OIDC login, ported from `OAuth2Controller.kt`,
//! `KomgaOAuth2UserServiceConfiguration.kt`, and `GithubOAuth2UserService.kt`.
//!
//! Authorization-code flow:
//! - `GET /api/v1/oauth2/providers` lists the registered clients.
//! - `GET /oauth2/authorization/{registrationId}` redirects to the provider with a state we keep
//!   in memory (10 min).
//! - `GET /login/oauth2/code/{registrationId}` exchanges the code, resolves the user's email,
//!   applies the ERR_1024-1028 rules, establishes a session on success, and redirects to
//!   `/?server_redirect=Y`; failures redirect to `/login?server_redirect=Y&error=<code>`.
//!
//! With no registrations, OAuth2 login is disabled entirely (`oauth2Enabled = false`).

use crate::config::{OAuth2ClientRegistration, ServerConfig};
use crate::dto::oauth2::{
    GithubEmail, OAuth2ClientDto, OidcDiscovery, TokenResponse, UserInfoClaims,
};
use crate::http::base_url::base_url_from_headers;
use crate::state::AppState;
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::{routing, Json, Router};
use komga_core::model::user::{ContentRestrictions, KomgaUser, UserRole};
use komga_core::time_codec::now_utc;
use komga_db::dao::user::UserDao;
use std::collections::BTreeSet;

const STATE_TTL: std::time::Duration = std::time::Duration::from_secs(600);

#[derive(Clone)]
struct OAuth2State {
    states: moka::sync::Cache<String, String>,
    discovery: moka::sync::Cache<String, OidcDiscovery>,
    http: reqwest::Client,
}

impl OAuth2State {
    fn new() -> Self {
        Self {
            states: moka::sync::Cache::builder().time_to_idle(STATE_TTL).build(),
            discovery: moka::sync::Cache::builder().build(),
            http: reqwest::Client::new(),
        }
    }
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/oauth2/providers", routing::get(get_providers))
        .route(
            "/oauth2/authorization/{registration_id}",
            routing::get(authorize),
        )
        .route(
            "/login/oauth2/code/{registration_id}",
            routing::get(callback),
        )
        .layer(axum::Extension(OAuth2State::new()))
}

/// PermitAll in `SecurityConfiguration.kt`: the webui login page calls this before any
/// authentication exists. With no registrations this is an empty list, same as Java's
/// null `clientRegistrationRepository`.
async fn get_providers(State(state): State<AppState>) -> Json<Vec<OAuth2ClientDto>> {
    Json(
        state
            .config
            .oauth2
            .registrations
            .iter()
            .map(|r| OAuth2ClientDto {
                name: r.client_name_or_id().to_string(),
                registration_id: r.registration_id.clone(),
            })
            .collect(),
    )
}

fn find_registration<'a>(
    config: &'a ServerConfig,
    registration_id: &str,
) -> Option<&'a OAuth2ClientRegistration> {
    config
        .oauth2
        .registrations
        .iter()
        .find(|r| r.registration_id == registration_id)
}

async fn authorize(
    State(state): State<AppState>,
    axum::Extension(oauth2): axum::Extension<OAuth2State>,
    Path(registration_id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, StatusCode> {
    let Some(registration) = find_registration(&state.config, &registration_id) else {
        return Err(StatusCode::NOT_FOUND);
    };
    let endpoints = match resolve_endpoints(&oauth2, registration).await {
        Some(e) => e,
        None => return Err(StatusCode::NOT_FOUND),
    };

    let scopes = registration.effective_scopes();
    let state_key = uuid::Uuid::new_v4().to_string();
    oauth2
        .states
        .insert(state_key.clone(), registration_id.clone());

    let base = base_url_from_headers(&headers, &state.config);
    let redirect_uri = registration
        .redirect_uri
        .clone()
        .unwrap_or_else(|| format!("{base}/login/oauth2/code/{registration_id}"));

    let url = format!(
        "{}?response_type=code&client_id={}&scope={}&state={}&redirect_uri={}",
        endpoints.0,
        urlencoding(&registration.client_id),
        urlencoding(&scopes.join(" ")),
        state_key,
        urlencoding(&redirect_uri),
    );
    Ok(Redirect::temporary(&url).into_response())
}

/// (authorization, token, userinfo) endpoints, explicit config first, OIDC discovery as fallback.
async fn resolve_endpoints(
    oauth2: &OAuth2State,
    registration: &OAuth2ClientRegistration,
) -> Option<(String, String, String)> {
    let (mut authorization, mut token, mut userinfo) = (
        registration.authorization_uri.clone(),
        registration.token_uri.clone(),
        registration.user_info_uri.clone(),
    );
    if authorization.is_some() && token.is_some() && userinfo.is_some() {
        return Some((authorization?, token?, userinfo?));
    }
    let issuer = registration.issuer_uri.as_deref()?;
    let discovery = match oauth2.discovery.get(issuer) {
        Some(d) => Some(d),
        None => {
            let url = format!(
                "{}/.well-known/openid-configuration",
                issuer.trim_end_matches('/')
            );
            let discovered = oauth2
                .http
                .get(&url)
                .send()
                .await
                .ok()?
                .json::<OidcDiscovery>()
                .await
                .ok()?;
            oauth2
                .discovery
                .insert(issuer.to_string(), discovered.clone());
            Some(discovered)
        }
    }?;
    authorization.get_or_insert(discovery.authorization_endpoint.clone());
    token.get_or_insert(discovery.token_endpoint.clone());
    userinfo.get_or_insert(discovery.userinfo_endpoint.clone());
    Some((authorization?, token?, userinfo?))
}

#[derive(serde::Deserialize)]
struct CallbackParams {
    code: Option<String>,
    state: Option<String>,
}

async fn callback(
    State(state): State<AppState>,
    axum::Extension(oauth2): axum::Extension<OAuth2State>,
    Path(registration_id): Path<String>,
    Query(params): Query<CallbackParams>,
    headers: HeaderMap,
    request: Request,
) -> Result<Response, StatusCode> {
    let (parts, _) = request.into_parts();
    let Some(registration) = find_registration(&state.config, &registration_id) else {
        return Err(StatusCode::NOT_FOUND);
    };
    let client_name = registration.client_name_or_id().to_string();

    let fail = |code: &str, error: Option<String>| {
        state.record_activity_sync(&client_name, None, false, error, &parts);
        Redirect::temporary(&format!("/login?server_redirect=Y&error={code}")).into_response()
    };

    // state check (Spring's `invalid_state`)
    let state_ok = params
        .state
        .as_deref()
        .and_then(|s| oauth2.states.remove(s))
        .is_some_and(|id| id == registration_id);
    if !state_ok {
        return Ok(fail("invalid_state", Some("invalid state".into())));
    }
    let Some(code) = params.code.filter(|c| !c.is_empty()) else {
        return Ok(fail("invalid_request", Some("missing code".into())));
    };

    let Some((_, token_uri, userinfo_uri)) = resolve_endpoints(&oauth2, registration).await else {
        return Ok(fail(
            "invalid_request",
            Some("cannot resolve endpoints".into()),
        ));
    };

    // exchange the code for a token (client_secret_basic)
    let base = base_url_from_headers(&headers, &state.config);
    let redirect_uri = registration
        .redirect_uri
        .clone()
        .unwrap_or_else(|| format!("{base}/login/oauth2/code/{registration_id}"));
    let token = exchange_token(&oauth2, registration, &token_uri, &code, &redirect_uri).await;
    let access_token = match token {
        Ok(Some(t)) => t,
        Ok(None) => return Ok(fail("invalid_token_response", None)),
        Err(e) => {
            tracing::warn!("OAuth2 token exchange failed: {e}");
            return Ok(fail("invalid_token_response", Some(e.to_string())));
        }
    };

    // load the user info
    let claims = match load_user_info(&oauth2, &userinfo_uri, &access_token).await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("OAuth2 user info failed: {e}");
            return Ok(fail("invalid_user_info_response", Some(e.to_string())));
        }
    };

    let email = match resolve_email(
        &oauth2,
        registration,
        &userinfo_uri,
        &access_token,
        &claims,
        state.config.oauth2.oidc_email_verification,
    )
    .await
    {
        Ok(email) => email,
        Err(code) => return Ok(fail(code, None)),
    };
    let dao = UserDao::new(state.db.clone());
    let user = match dao.find_by_email_ignore_case(&email) {
        Ok(Some(user)) => user,
        _ if state.config.oauth2.account_creation => {
            let new_user = KomgaUser {
                id: String::new(),
                email: email.clone(),
                password: bcrypt::hash(random_password(), 10)
                    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?,
                roles: [UserRole::FileDownload, UserRole::PageStreaming]
                    .into_iter()
                    .collect::<BTreeSet<_>>(),
                shared_libraries_ids: BTreeSet::new(),
                shared_all_libraries: true,
                restrictions: ContentRestrictions::default(),
                created_date: now_utc(),
                last_modified_date: now_utc(),
            };
            match dao.insert(&new_user) {
                Ok(id) => dao
                    .find_by_id(&id)
                    .ok()
                    .flatten()
                    .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?,
                Err(e) => {
                    tracing::warn!("OAuth2 user creation failed: {e}");
                    return Ok(fail("server_error", Some(e.to_string())));
                }
            }
        }
        _ => return Ok(fail("ERR_1025", Some("user does not exist".into()))),
    };

    // success: session + activity + redirect to the SPA root
    let session_id = state
        .sessions
        .create(&user.id)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    state.record_activity_sync(
        &client_name,
        Some((&user.id, &user.email)),
        true,
        None,
        &parts,
    );
    let cookie = format!(
        "{}={}; Path=/; HttpOnly; SameSite=Lax",
        crate::auth::SESSION_COOKIE_NAME,
        session_id
    );
    Ok((
        [(
            axum::http::header::SET_COOKIE,
            axum::http::HeaderValue::from_str(&cookie).unwrap(),
        )],
        Redirect::temporary("/?server_redirect=Y"),
    )
        .into_response())
}

/// Records authentication activity with the `OAuth2:{clientName}` source of `LoginListener`.
trait RecordOAuth2Activity {
    fn record_activity_sync(
        &self,
        client_name: &str,
        user: Option<(&str, &str)>,
        success: bool,
        error: Option<String>,
        parts: &axum::http::request::Parts,
    );
}

impl RecordOAuth2Activity for AppState {
    fn record_activity_sync(
        &self,
        client_name: &str,
        user: Option<(&str, &str)>,
        success: bool,
        error: Option<String>,
        parts: &axum::http::request::Parts,
    ) {
        let draft = crate::auth::ActivityDraft {
            user_id: user.map(|(id, _)| id.to_string()),
            email: user.map(|(_, email)| email.to_string()),
            api_key_id: None,
            api_key_comment: None,
            success,
            error,
            source: format!("OAuth2:{client_name}"),
        };
        let state = self.clone();
        let activity = Some(draft);
        let parts = parts.clone();
        tokio::spawn(async move {
            state.record_activity(&activity, &parts).await;
        });
    }
}

fn random_password() -> String {
    // `RandomStringUtils.secure().nextAlphanumeric(12)`
    const ALNUM: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    uuid::Uuid::new_v4()
        .as_bytes()
        .iter()
        .take(12)
        .map(|b| ALNUM[(*b as usize) % ALNUM.len()] as char)
        .collect()
}

fn urlencoding(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        let c = b as char;
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | '~') {
            out.push(c);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

async fn exchange_token(
    oauth2: &OAuth2State,
    registration: &OAuth2ClientRegistration,
    token_uri: &str,
    code: &str,
    redirect_uri: &str,
) -> Result<Option<String>, reqwest::Error> {
    let response = oauth2
        .http
        .post(token_uri)
        .basic_auth(&registration.client_id, Some(&registration.client_secret))
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
        ])
        .send()
        .await?;
    let parsed = response.json::<TokenResponse>().await?;
    Ok(parsed.access_token.filter(|t| !t.is_empty()))
}

async fn load_user_info(
    oauth2: &OAuth2State,
    userinfo_uri: &str,
    access_token: &str,
) -> Result<UserInfoClaims, reqwest::Error> {
    oauth2
        .http
        .get(userinfo_uri)
        .bearer_auth(access_token)
        .send()
        .await?
        .error_for_status()?
        .json::<UserInfoClaims>()
        .await
}

/// The ERR_1024/1026/1027/1028 rules; the GitHub `/emails` fallback is applied on demand.
/// The ERR_1024/1026/1027/1028 rules; the GitHub `/emails` fallback is applied on demand.
/// OIDC checks run first (`KomgaOAuth2UserServiceConfiguration.oidcUserService`);
/// the GitHub fallback only applies to plain OAuth2 (`oauth2UserService`).
async fn resolve_email(
    oauth2: &OAuth2State,
    registration: &OAuth2ClientRegistration,
    userinfo_uri: &str,
    access_token: &str,
    claims: &UserInfoClaims,
    oidc_email_verification: bool,
) -> Result<String, &'static str> {
    let email = claims.email.clone().filter(|e| !e.is_empty());

    if registration.is_oidc() {
        let Some(email) = email else {
            return Err("ERR_1028");
        };
        if oidc_email_verification {
            match claims.email_verified {
                None => return Err("ERR_1027"),
                Some(false) => return Err("ERR_1026"),
                Some(true) => {}
            }
        }
        return Ok(email);
    }

    // plain OAuth2: GitHub /emails fallback when the profile has no email
    let email = match email {
        Some(email) => email,
        None if registration.registration_id.eq_ignore_ascii_case("github")
            && registration
                .effective_scopes()
                .iter()
                .any(|s| s == "user:email" || s == "user") =>
        {
            let emails: Vec<GithubEmail> = oauth2
                .http
                .get(format!("{userinfo_uri}/emails"))
                .bearer_auth(access_token)
                .send()
                .await
                .map_err(|_| "server_error")?
                .json()
                .await
                .map_err(|_| "server_error")?;
            match emails
                .into_iter()
                .find(|e| e.verified == Some(true) && e.primary == Some(true))
                .and_then(|e| e.email)
                .filter(|e| !e.is_empty())
            {
                Some(email) => email,
                None => return Err("ERR_1024"),
            }
        }
        None => return Err("ERR_1024"),
    };
    Ok(email)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth;
    use crate::config::{OAuth2ClientRegistration, OAuth2Config, ServerConfig};
    use crate::http;
    use crate::state::test_kmrs_db;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use komga_core::model::user::{ContentRestrictions, KomgaUser, UserRole};
    use komga_core::time_codec::now_utc;
    use komga_db::pool::{Database, DatabaseConfig, JournalMode};
    use komga_db::{Migrator, Placeholders};
    use std::collections::BTreeSet;
    use tower::ServiceExt;

    fn test_state(oauth2: OAuth2Config) -> AppState {
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
        let db_config = |register_udfs| DatabaseConfig {
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
            oauth2,

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

    fn test_app(state: &AppState) -> Router {
        router()
            .layer(axum::middleware::from_fn(
                http::error_path::error_path_middleware,
            ))
            .layer(axum::middleware::from_fn(http::etag::etag_middleware))
            .layer(axum::middleware::from_fn(
                http::cache::cache_control_middleware,
            ))
            .layer(axum::middleware::from_fn_with_state(
                state.clone(),
                auth::auth_middleware,
            ))
            .with_state(state.clone())
    }

    fn registration(base: &str, id: &str, oidc: bool) -> OAuth2ClientRegistration {
        OAuth2ClientRegistration {
            registration_id: id.to_string(),
            client_name: None,
            client_id: "cid".into(),
            client_secret: "secret".into(),
            authorization_grant_type: "authorization_code".into(),
            redirect_uri: None,
            scopes: if id == "github" {
                vec!["user:email".into()]
            } else {
                vec![]
            },
            issuer_uri: if oidc { Some(base.to_string()) } else { None },
            authorization_uri: Some(format!("{base}/authorize")),
            token_uri: Some(format!("{base}/token")),
            user_info_uri: Some(format!("{base}/userinfo")),
            user_name_attribute: None,
        }
    }

    fn seed_user(state: &AppState, email: &str) -> KomgaUser {
        let user = KomgaUser {
            id: String::new(),
            email: email.into(),
            password: "x".into(),
            roles: [UserRole::FileDownload, UserRole::PageStreaming]
                .into_iter()
                .collect(),
            shared_libraries_ids: BTreeSet::new(),
            shared_all_libraries: true,
            restrictions: ContentRestrictions::default(),
            created_date: now_utc(),
            last_modified_date: now_utc(),
        };
        let id = UserDao::new(state.db.clone()).insert(&user).unwrap();
        UserDao::new(state.db.clone())
            .find_by_id(&id)
            .unwrap()
            .unwrap()
    }

    struct MockOauth2 {
        base: String,
        handle: tokio::task::JoinHandle<()>,
    }

    impl Drop for MockOauth2 {
        fn drop(&mut self) {
            self.handle.abort();
        }
    }

    /// Mock OAuth2 provider: /token, /userinfo, /userinfo/emails, /.well-known/openid-configuration.
    /// `claims_json` is the exact JSON body returned by /userinfo.
    async fn mock_server(claims_json: serde_json::Value) -> MockOauth2 {
        mock_server_with(claims_json, None).await
    }

    async fn mock_server_with(
        claims_json: serde_json::Value,
        emails_json: Option<serde_json::Value>,
    ) -> MockOauth2 {
        let claims = claims_json.clone();
        let app = Router::new()
            .route(
                "/token",
                routing::post(|| async {
                    Json(serde_json::json!({"access_token": "tok-123", "token_type": "bearer"}))
                }),
            )
            .route(
                "/userinfo",
                routing::get(move || {
                    let claims = claims.clone();
                    async move { Json(claims) }
                }),
            )
            .route(
                "/userinfo/emails",
                routing::get(move || async move {
                    Json(emails_json.unwrap_or_else(|| serde_json::json!([])))
                }),
            )
            .route(
                "/.well-known/openid-configuration",
                routing::get(|| async { Json(serde_json::json!({})) }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        MockOauth2 {
            base: format!("http://{addr}"),
            handle,
        }
    }

    fn oauth2_config(
        registrations: Vec<OAuth2ClientRegistration>,
        account_creation: bool,
        oidc_email_verification: bool,
    ) -> OAuth2Config {
        OAuth2Config {
            registrations,
            account_creation,
            oidc_email_verification,
        }
    }

    async fn get(app: &Router, uri: &str) -> axum::response::Response {
        app.clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    async fn get_with(
        app: &Router,
        uri: &str,
        headers: &[(&str, &str)],
    ) -> axum::response::Response {
        let mut builder = Request::builder().uri(uri);
        for (k, v) in headers {
            builder = builder.header(*k, *v);
        }
        app.clone()
            .oneshot(builder.body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn providers_empty_when_no_registrations() {
        let state = test_state(oauth2_config(vec![], false, true));
        let app = test_app(&state);
        let user = seed_user(&state, "admin@komga.org");
        let session = state.sessions.create(&user.id).unwrap();
        let response = get_with(
            &app,
            "/api/v1/oauth2/providers",
            &[("X-Auth-Token", &session)],
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = http_body_to_string(response).await;
        assert_eq!(body, "[]");
    }

    #[tokio::test]
    async fn providers_permit_anonymous() {
        let state = test_state(oauth2_config(vec![], false, true));
        let app = test_app(&state);
        let response = get(&app, "/api/v1/oauth2/providers").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(http_body_to_string(response).await, "[]");
    }

    #[tokio::test]
    async fn providers_list_uses_client_name_fallback() {
        let github = registration("http://x", "github", false);
        let custom = OAuth2ClientRegistration {
            client_name: Some("My IdP".into()),
            ..registration("http://y", "keycloak", true)
        };
        let state = test_state(oauth2_config(vec![github, custom], false, true));
        let app = test_app(&state);
        let user = seed_user(&state, "admin@komga.org");
        let session = state.sessions.create(&user.id).unwrap();
        let response = get_with(
            &app,
            "/api/v1/oauth2/providers",
            &[("X-Auth-Token", &session)],
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value =
            serde_json::from_str(&http_body_to_string(response).await).unwrap();
        assert_eq!(
            body,
            serde_json::json!([
                {"name": "github", "registrationId": "github"},
                {"name": "My IdP", "registrationId": "keycloak"},
            ])
        );
    }

    #[tokio::test]
    async fn authorize_redirects_with_all_parameters() {
        let mock = mock_server(serde_json::json!({})).await;
        let state = test_state(oauth2_config(
            vec![registration(&mock.base, "github", false)],
            false,
            true,
        ));
        let app = test_app(&state);
        let response = get(&app, "/oauth2/authorization/github").await;
        assert_eq!(response.status(), StatusCode::TEMPORARY_REDIRECT);
        let location = response
            .headers()
            .get(axum::http::header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap();
        let (path, query) = location.split_once('?').unwrap();
        assert_eq!(path, format!("{}/authorize", mock.base));
        let params: std::collections::HashMap<_, _> =
            form_urlencoded::parse(query.as_bytes()).collect();
        assert_eq!(params["response_type"], "code");
        assert_eq!(params["client_id"], "cid");
        assert_eq!(params["scope"], "user:email");
        assert!(params["state"].len() > 10);
        assert_eq!(
            params["redirect_uri"],
            "http://localhost/login/oauth2/code/github"
        );
    }

    #[tokio::test]
    async fn authorize_404_when_unregistered() {
        let state = test_state(oauth2_config(vec![], false, true));
        let app = test_app(&state);
        let response = get(&app, "/oauth2/authorization/github").await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let response = get(&app, "/login/oauth2/code/github?code=x&state=y").await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn callback_success_establishes_session() {
        let mock = mock_server(serde_json::json!({"email": "admin@komga.org"})).await;
        let state = test_state(oauth2_config(
            vec![registration(&mock.base, "github", false)],
            false,
            true,
        ));
        let app = test_app(&state);
        seed_user(&state, "admin@komga.org");

        let location = authorize_location(&app, "github").await;
        let state_key = location
            .split_once('?')
            .and_then(|(_, query)| {
                form_urlencoded::parse(query.as_bytes())
                    .find(|(k, _)| k == "state")
                    .map(|(_, v)| v.into_owned())
            })
            .unwrap();
        let response = get(
            &app,
            &format!("/login/oauth2/code/github?code=abc&state={state_key}"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::LOCATION)
                .unwrap(),
            "/?server_redirect=Y"
        );
        let cookie = response
            .headers()
            .get(axum::http::header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(cookie.contains("KOMGA-SESSION="));
        let session_id = cookie
            .split("KOMGA-SESSION=")
            .nth(1)
            .unwrap()
            .split(';')
            .next()
            .unwrap();
        assert!(state.sessions.get(session_id).unwrap().is_some());

        // activity recorded with the OAuth2:github source
        let activities = wait_activities(&state, 1).await;
        assert_eq!(activities.len(), 1);
        assert_eq!(activities[0].0, "OAuth2:github");
        assert!(activities[0].1);
    }

    #[tokio::test]
    async fn callback_github_emails_fallback() {
        let mock = mock_server_with(
            serde_json::json!({"login": "octocat"}),
            Some(serde_json::json!([
                {"email": "other@x.io", "verified": true, "primary": false},
                {"email": "octo@x.io", "verified": true, "primary": true},
            ])),
        )
        .await;
        let state = test_state(oauth2_config(
            vec![registration(&mock.base, "github", false)],
            false,
            true,
        ));
        let app = test_app(&state);
        seed_user(&state, "octo@x.io");

        let state_key = authorize_state_key(&app, "github").await;
        let response = get(
            &app,
            &format!("/login/oauth2/code/github?code=abc&state={state_key}"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::LOCATION)
                .unwrap(),
            "/?server_redirect=Y"
        );
    }

    #[tokio::test]
    async fn callback_err_1024_oauth2_email_missing() {
        let mock = mock_server(serde_json::json!({"login": "octocat"})).await;
        let state = test_state(oauth2_config(
            vec![registration(&mock.base, "gitlab", false)],
            false,
            true,
        ));
        let app = test_app(&state);
        let state_key = authorize_state_key(&app, "gitlab").await;
        let response = get(
            &app,
            &format!("/login/oauth2/code/gitlab?code=abc&state={state_key}"),
        )
        .await;
        assert_error_redirect(&response, "ERR_1024");
    }

    #[tokio::test]
    async fn callback_oidc_err_codes() {
        for (claims, expected) in [
            (serde_json::json!({"login": "x"}), "ERR_1028"),
            (serde_json::json!({"email": "x@y.io"}), "ERR_1027"),
            (
                serde_json::json!({"email": "x@y.io", "email_verified": false}),
                "ERR_1026",
            ),
        ] {
            let mock = mock_server(claims).await;
            let state = test_state(oauth2_config(
                vec![registration(&mock.base, "keycloak", true)],
                false,
                true,
            ));
            let app = test_app(&state);
            let state_key = authorize_state_key(&app, "keycloak").await;
            let response = get(
                &app,
                &format!("/login/oauth2/code/keycloak?code=abc&state={state_key}"),
            )
            .await;
            assert_error_redirect(&response, expected);
        }
    }

    #[tokio::test]
    async fn callback_oidc_success() {
        let mock =
            mock_server(serde_json::json!({"email": "admin@komga.org", "email_verified": true}))
                .await;
        let state = test_state(oauth2_config(
            vec![registration(&mock.base, "keycloak", true)],
            false,
            true,
        ));
        let app = test_app(&state);
        seed_user(&state, "admin@komga.org");
        let state_key = authorize_state_key(&app, "keycloak").await;
        let response = get(
            &app,
            &format!("/login/oauth2/code/keycloak?code=abc&state={state_key}"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::LOCATION)
                .unwrap(),
            "/?server_redirect=Y"
        );
    }

    #[tokio::test]
    async fn callback_err_1025_user_missing_and_creation_disabled() {
        let mock = mock_server(serde_json::json!({"email": "ghost@komga.org"})).await;
        let state = test_state(oauth2_config(
            vec![registration(&mock.base, "github", false)],
            false,
            true,
        ));
        let app = test_app(&state);
        let state_key = authorize_state_key(&app, "github").await;
        let response = get(
            &app,
            &format!("/login/oauth2/code/github?code=abc&state={state_key}"),
        )
        .await;
        assert_error_redirect(&response, "ERR_1025");
    }

    #[tokio::test]
    async fn callback_creates_user_when_enabled() {
        let mock = mock_server(serde_json::json!({"email": "newbie@komga.org"})).await;
        let state = test_state(oauth2_config(
            vec![registration(&mock.base, "github", false)],
            true,
            true,
        ));
        let app = test_app(&state);
        let state_key = authorize_state_key(&app, "github").await;
        let response = get(
            &app,
            &format!("/login/oauth2/code/github?code=abc&state={state_key}"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::LOCATION)
                .unwrap(),
            "/?server_redirect=Y"
        );

        let created = UserDao::new(state.db.clone())
            .find_by_email_ignore_case("newbie@komga.org")
            .unwrap()
            .expect("user created");
        assert!(created.shared_all_libraries);
        assert!(created.roles.contains(&UserRole::FileDownload));
        assert!(created.roles.contains(&UserRole::PageStreaming));
        assert!(!created.roles.contains(&UserRole::Admin));
        // bcrypt hash, not the random plaintext
        assert!(created.password.starts_with("$2"));

        let cookie = response
            .headers()
            .get(axum::http::header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap();
        let session_id = cookie
            .split("KOMGA-SESSION=")
            .nth(1)
            .unwrap()
            .split(';')
            .next()
            .unwrap();
        assert_eq!(
            state.sessions.get(session_id).unwrap().map(|s| s.user_id),
            Some(created.id.clone())
        );
    }

    #[tokio::test]
    async fn callback_invalid_state() {
        let mock = mock_server(serde_json::json!({"email": "admin@komga.org"})).await;
        let state = test_state(oauth2_config(
            vec![registration(&mock.base, "github", false)],
            false,
            true,
        ));
        let app = test_app(&state);
        let response = get(&app, "/login/oauth2/code/github?code=abc&state=nope").await;
        assert_error_redirect(&response, "invalid_state");
    }

    async fn authorize_location(app: &Router, registration_id: &str) -> String {
        let response = get(app, &format!("/oauth2/authorization/{registration_id}")).await;
        assert_eq!(response.status(), StatusCode::TEMPORARY_REDIRECT);
        response
            .headers()
            .get(axum::http::header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string()
    }

    async fn authorize_state_key(app: &Router, registration_id: &str) -> String {
        let location = authorize_location(app, registration_id).await;
        let (_, query) = location.split_once('?').unwrap();
        form_urlencoded::parse(query.as_bytes())
            .find(|(k, _)| k == "state")
            .map(|(_, v)| v.into_owned())
            .unwrap()
    }

    fn assert_error_redirect(response: &axum::response::Response, expected: &str) {
        assert_eq!(response.status(), StatusCode::TEMPORARY_REDIRECT);
        let location = response
            .headers()
            .get(axum::http::header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(
            location,
            format!("/login?server_redirect=Y&error={expected}")
        );
    }

    fn activity_rows(state: &AppState) -> Vec<(String, bool)> {
        state
            .db
            .ro()
            .unwrap()
            .prepare("SELECT SOURCE, SUCCESS FROM AUTHENTICATION_ACTIVITY")
            .unwrap()
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, bool>(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    /// The activity is recorded on a spawned task; poll until it lands.
    async fn wait_activities(state: &AppState, expected: usize) -> Vec<(String, bool)> {
        for _ in 0..50 {
            let rows = activity_rows(state);
            if rows.len() >= expected {
                return rows;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        activity_rows(state)
    }

    async fn http_body_to_string(response: axum::response::Response) -> String {
        let bytes = http_body_util::BodyExt::collect(response.into_body())
            .await
            .unwrap()
            .to_bytes();
        String::from_utf8(bytes.to_vec()).unwrap()
    }
}
