//! API error shapes, aligned with Spring Boot (`include-message: always`) and komga's `ErrorHandlingControllerAdvice`:
//! - business errors: `{timestamp,status,error,message,path}`.
//! - bean validation failures: 400 `{violations:[{fieldName,message}]}`.
//! - unauthenticated: 401 `{timestamp,status,error,message,path}` with a Basic challenge.
//! - `EntityNotFoundException`: 404 empty body.
//! - `CodedException`: 400, message is `ERR_xxxx`.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use komga_core::error::CodedError;
use serde::Serialize;

#[derive(Debug)]
#[allow(dead_code)] // variants like NotFoundEmpty are used by endpoints in M3
pub enum ApiError {
    /// 401 Spring default error JSON + `WWW-Authenticate: Basic realm="Realm"`
    Unauthorized,
    /// 404 empty body (EntityNotFoundException semantics)
    NotFoundEmpty,
    /// Spring default error JSON
    Status { status: StatusCode, message: String },
    /// 400 + violations
    Violations(Vec<Violation>),
    /// 500
    Internal(String),
}

#[derive(Debug, Serialize)]
pub struct Violation {
    #[serde(rename = "fieldName")]
    pub field_name: String,
    pub message: String,
}

#[allow(dead_code)]
impl ApiError {
    pub fn unauthorized() -> Self {
        Self::Unauthorized
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::Status {
            status: StatusCode::NOT_FOUND,
            message: message.into(),
        }
    }

    pub fn payload_too_large(message: impl Into<String>) -> Self {
        Self::Status {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            message: message.into(),
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::Status {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }

    /// 403 unless `[libraries] allow-file-deletion` permits deleting files from the library.
    pub fn require_file_deletion_allowed(config: &crate::config::ServerConfig) -> Result<(), Self> {
        if config.allow_file_deletion {
            Ok(())
        } else {
            Err(Self::forbidden(crate::service::FILE_DELETION_DISABLED))
        }
    }

    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::Status {
            status: StatusCode::FORBIDDEN,
            message: message.into(),
        }
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self::Status {
            status: StatusCode::CONFLICT,
            message: message.into(),
        }
    }

    pub fn unsupported_media_type(message: impl Into<String>) -> Self {
        Self::Status {
            status: StatusCode::UNSUPPORTED_MEDIA_TYPE,
            message: message.into(),
        }
    }

    pub fn bad_gateway(message: impl Into<String>) -> Self {
        Self::Status {
            status: StatusCode::BAD_GATEWAY,
            message: message.into(),
        }
    }
}

impl From<CodedError> for ApiError {
    fn from(e: CodedError) -> Self {
        Self::bad_request(e.0)
    }
}

impl From<crate::service::readlist::MatchCblError> for ApiError {
    fn from(e: crate::service::readlist::MatchCblError) -> Self {
        match e {
            crate::service::readlist::MatchCblError::Coded(c) => Self::bad_request(c.0),
            crate::service::readlist::MatchCblError::Db(d) => Self::Internal(d.to_string()),
        }
    }
}

impl From<komga_db::Error> for ApiError {
    fn from(e: komga_db::Error) -> Self {
        Self::Internal(e.to_string())
    }
}

#[derive(Serialize)]
struct ErrorBody {
    timestamp: String,
    status: u16,
    error: String,
    message: String,
    path: String,
}

/// Response extension marking a 401 from Spring's `BasicAuthenticationEntryPoint`
/// equivalent, as opposed to a controller-level 401 that owns its body.
#[derive(Clone, Copy)]
pub struct BasicChallenge;

fn unauthorized_response() -> Response {
    let status = StatusCode::UNAUTHORIZED;
    let reason = reason_phrase(status);
    let body = ErrorBody {
        timestamp: now_timestamp(),
        status: status.as_u16(),
        error: reason.clone(),
        message: reason,
        path: String::new(), // filled in with the request path by error_path_middleware
    };
    let mut response = (
        status,
        [(
            axum::http::header::WWW_AUTHENTICATE,
            "Basic realm=\"Realm\"",
        )],
        Json(body),
    )
        .into_response();
    response.headers_mut().insert(
        crate::http::error_path::ERROR_MARKER,
        axum::http::HeaderValue::from_static("1"),
    );
    response.extensions_mut().insert(BasicChallenge);
    response
}

/// RFC 9457 ProblemDetail error for the `/api/v3` prefix (komga 1.29
/// `ApiV3ErrorHandlingControllerAdvice`): controller exceptions under v3 serialize as
/// `application/problem+json`; the security-level 401/empty-404 stay as-is (the advice only
/// wraps controller exceptions). `instance` carries the request path — an RFC 9457
/// extension Spring leaves unset — for log correlation.
pub struct V3Error {
    error: ApiError,
    /// request path for the ProblemDetail `instance`, without the query string
    path: String,
}

impl V3Error {
    pub fn new(error: ApiError, path: &str) -> Self {
        Self {
            error,
            path: path.to_string(),
        }
    }
}

#[derive(Serialize)]
struct ProblemDetailBody {
    #[serde(rename = "type")]
    type_: &'static str,
    title: String,
    status: u16,
    detail: String,
    instance: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    violations: Option<Vec<V3Violation>>,
}

#[derive(Serialize)]
struct V3Violation {
    field: Option<String>,
    message: Option<String>,
}

impl IntoResponse for V3Error {
    fn into_response(self) -> Response {
        match self.error {
            ApiError::Unauthorized => unauthorized_response(),
            ApiError::NotFoundEmpty => StatusCode::NOT_FOUND.into_response(),
            ApiError::Status { status, message } => {
                problem_response(status, message, self.path, None)
            }
            ApiError::Violations(violations) => problem_response(
                StatusCode::BAD_REQUEST,
                "Invalid request content.".to_string(),
                self.path,
                Some(
                    violations
                        .into_iter()
                        .map(|v| V3Violation {
                            field: Some(v.field_name),
                            message: Some(v.message),
                        })
                        .collect(),
                ),
            ),
            ApiError::Internal(message) => {
                tracing::error!("{message}");
                problem_response(StatusCode::INTERNAL_SERVER_ERROR, message, self.path, None)
            }
        }
    }
}

fn problem_response(
    status: StatusCode,
    detail: String,
    path: String,
    violations: Option<Vec<V3Violation>>,
) -> Response {
    let body = ProblemDetailBody {
        type_: "about:blank",
        title: reason_phrase(status),
        status: status.as_u16(),
        detail,
        instance: path,
        violations,
    };
    let mut response = (status, Json(body)).into_response();
    response.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/problem+json"),
    );
    response
}

#[derive(Serialize)]
struct ViolationsBody {
    violations: Vec<Violation>,
}

fn reason_phrase(status: StatusCode) -> String {
    status.canonical_reason().unwrap_or("").to_string()
}

/// Spring's `HttpStatus.toString()` uses the enum constant name, e.g. `NOT_FOUND`
fn spring_status_name(status: StatusCode) -> String {
    reason_phrase(status)
        .chars()
        .map(|c| {
            if c.is_ascii_alphabetic() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

/// `ResponseStatusException.getMessage()`: `"404 NOT_FOUND"` without a reason,
/// `404 NOT_FOUND "reason"` with one.
fn spring_message(status: StatusCode, reason: &str) -> String {
    let base = format!("{} {}", status.as_u16(), spring_status_name(status));
    if reason.is_empty() {
        base
    } else {
        format!("{base} \"{reason}\"")
    }
}

/// Timestamp of Spring `DefaultErrorAttributes`: `yyyy-MM-dd'T'HH:mm:ss.SSS+00:00`.
fn now_timestamp() -> String {
    let now = time::OffsetDateTime::now_utc();
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}+00:00",
        now.year(),
        now.month() as u8,
        now.day(),
        now.hour(),
        now.minute(),
        now.second(),
        now.nanosecond() / 1_000_000,
    )
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        match self {
            ApiError::Unauthorized => unauthorized_response(),
            ApiError::NotFoundEmpty => StatusCode::NOT_FOUND.into_response(),
            ApiError::Status { status, message } => {
                let body = ErrorBody {
                    timestamp: now_timestamp(),
                    status: status.as_u16(),
                    error: reason_phrase(status),
                    message: spring_message(status, &message),
                    path: String::new(), // filled in with the request path by error_path_middleware
                };
                let mut response = (status, Json(body)).into_response();
                response.headers_mut().insert(
                    crate::http::error_path::ERROR_MARKER,
                    axum::http::HeaderValue::from_static("1"),
                );
                response
            }
            ApiError::Violations(violations) => {
                (StatusCode::BAD_REQUEST, Json(ViolationsBody { violations })).into_response()
            }
            // Spring logs the uncaught exception behind a 500; without this the detail
            // (e.g. which archive entry failed to decompress) only exists in the response body
            ApiError::Internal(message) => {
                tracing::error!("{message}");
                let body = ErrorBody {
                    timestamp: now_timestamp(),
                    status: 500,
                    error: "Internal Server Error".into(),
                    message,
                    path: String::new(),
                };
                let mut response = (StatusCode::INTERNAL_SERVER_ERROR, Json(body)).into_response();
                response.headers_mut().insert(
                    crate::http::error_path::ERROR_MARKER,
                    axum::http::HeaderValue::from_static("1"),
                );
                response
            }
        }
    }
}
