//! The size limit for request bodies. Every handler reads its body through a buffering
//! extractor (JSON, raw bytes or multipart), so one limit on those covers all of them.
//!
//! - An uploaded file may be `MAX_UPLOAD_FILE_SIZE` bytes, the number `/api/v1/settings`
//!   reports as `maxUploadFileSizeBytes`. Java komga has Spring Boot's default of 1 MB.
//! - A whole body may be a little larger, so a multipart request carrying a file of
//!   exactly that size still fits with its boundaries and other parts.
//! - A body or file over the limit answers 413 in the API's error format. The multipart
//!   extractor only hands its error to the handler, which has to map it, and the other
//!   extractors answer in plain text.

use crate::error::ApiError;
use axum::extract::multipart::{Field, MultipartError};
use axum::extract::{DefaultBodyLimit, Request};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Router;

pub const MAX_UPLOAD_FILE_SIZE: usize = 2 * 1024 * 1024;
/// room for the multipart boundaries and the small parts next to a file
const MULTIPART_OVERHEAD: usize = 64 * 1024;
const MAX_BODY_SIZE: usize = MAX_UPLOAD_FILE_SIZE + MULTIPART_OVERHEAD;

/// Applies the limit to every route of the router and reports bodies over it as 413.
pub fn apply<S: Clone + Send + Sync + 'static>(router: Router<S>) -> Router<S> {
    router
        .layer(DefaultBodyLimit::max(MAX_BODY_SIZE))
        .layer(axum::middleware::from_fn(report_too_large_bodies))
}

/// Names the rule, not the part that broke it: the body limit can also be reached by the
/// other parts of an upload.
fn upload_too_large() -> ApiError {
    ApiError::payload_too_large(format!(
        "Upload is too large: the maximum file size is {} MB",
        MAX_UPLOAD_FILE_SIZE / (1024 * 1024)
    ))
}

/// The extractors refuse a body over the limit with a plain-text 413: give it the API's
/// error format.
async fn report_too_large_bodies(request: Request, next: Next) -> Response {
    let response = next.run(request).await;
    // the extractors' own refusal is plain text: a 413 with any other body is left alone
    let plain_text = response
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("text/plain"));
    if response.status() == StatusCode::PAYLOAD_TOO_LARGE && plain_text {
        return body_too_large().into_response();
    }
    response
}

fn body_too_large() -> ApiError {
    ApiError::payload_too_large("Request body is too large")
}

/// A multipart body that could not be read: 413 when it ran into the body limit, 400 for
/// a malformed one.
pub fn multipart_error(error: MultipartError) -> ApiError {
    if error.status() == StatusCode::PAYLOAD_TOO_LARGE {
        upload_too_large()
    } else {
        ApiError::bad_request(error.to_string())
    }
}

/// The bytes of an uploaded file, refused with 413 when it is larger than
/// `MAX_UPLOAD_FILE_SIZE`. A file far over that runs into the body limit while it is read
/// and gets the same answer.
pub async fn file_bytes(field: Field<'_>) -> Result<Vec<u8>, ApiError> {
    let bytes = field.bytes().await.map_err(multipart_error)?;
    if bytes.len() > MAX_UPLOAD_FILE_SIZE {
        return Err(upload_too_large());
    }
    Ok(bytes.into())
}

/// What the route tests share: bodies as they arrive over a connection, and requests
/// answered by the router the server runs.
#[cfg(test)]
pub(crate) mod testing {
    use axum::body::{Body, Bytes};
    use axum::extract::Request;
    use axum::http::StatusCode;
    use tower::ServiceExt;

    /// The body in frames of 16 KiB with a pause before each, as it arrives over a
    /// connection. A body that is there all at once is read ahead in full, so one over
    /// the limit is refused before its first part is handed out; one that arrives bit by
    /// bit is refused in the part that crosses the limit.
    pub(crate) fn in_frames(body: Vec<u8>) -> Body {
        use futures_util::StreamExt;
        let frames: Vec<Result<Bytes, std::io::Error>> = body
            .chunks(16 * 1024)
            .map(|frame| Ok(Bytes::copy_from_slice(frame)))
            .collect();
        Body::from_stream(futures_util::stream::iter(frames).then(|frame| async move {
            tokio::task::yield_now().await;
            frame
        }))
    }

    /// The request answered by the server's own router, with every layer it has.
    pub(crate) async fn send_to_server(
        state: &crate::state::AppState,
        request: Request<Body>,
    ) -> (StatusCode, serde_json::Value) {
        let response = crate::build_router(state.clone())
            .oneshot(request)
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, body)
    }

    /// The same, with the request's body sent in frames.
    pub(crate) async fn send_to_server_in_frames(
        state: &crate::state::AppState,
        request: Request<Body>,
    ) -> (StatusCode, serde_json::Value) {
        let (parts, body) = request.into_parts();
        let body = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        send_to_server(state, Request::from_parts(parts, in_frames(body.to_vec()))).await
    }

    pub(crate) fn assert_upload_too_large(status: StatusCode, body: &serde_json::Value) {
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(
            body["message"],
            "413 PAYLOAD_TOO_LARGE \"Upload is too large: the maximum file size is 2 MB\""
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, Bytes};
    use axum::extract::Multipart;
    use axum::routing::post;
    use axum::Json;
    use tower::ServiceExt;

    async fn upload(mut multipart: Multipart) -> Result<String, ApiError> {
        let mut size = None;
        while let Some(field) = multipart.next_field().await.map_err(multipart_error)? {
            if field.name() == Some("file") {
                size = Some(file_bytes(field).await?.len());
            } else {
                field.text().await.map_err(multipart_error)?;
            }
        }
        Ok(format!("{size:?}"))
    }

    /// The three kinds of body the server reads, behind the limit and the middleware that
    /// fills in the path of an error: the two layers of the real router that shape the
    /// answer.
    fn app() -> Router {
        apply(
            Router::new()
                .route(
                    "/json",
                    post(|Json(value): Json<serde_json::Value>| async move {
                        value.to_string().len().to_string()
                    }),
                )
                .route(
                    "/bytes",
                    post(|body: Bytes| async move { body.len().to_string() }),
                )
                .route("/upload", post(upload))
                .route(
                    "/proxied",
                    post(|| async { (StatusCode::PAYLOAD_TOO_LARGE, Json("from elsewhere")) }),
                )
                .route("/bare", post(|| async { StatusCode::PAYLOAD_TOO_LARGE })),
        )
        .layer(axum::middleware::from_fn(
            crate::http::error_path::error_path_middleware,
        ))
    }

    async fn send(
        path: &str,
        content_type: &str,
        body: Vec<u8>,
    ) -> (StatusCode, serde_json::Value) {
        send_body(path, content_type, Body::from(body)).await
    }

    async fn send_body(
        path: &str,
        content_type: &str,
        body: Body,
    ) -> (StatusCode, serde_json::Value) {
        let request = axum::http::Request::builder()
            .method("POST")
            .uri(path)
            .header("Content-Type", content_type)
            .body(body)
            .unwrap();
        let response = app().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| serde_json::Value::String(String::from_utf8_lossy(&bytes).into()));
        (status, body)
    }

    /// A JSON string padded so that the whole body has the given size.
    fn json_of(size: usize) -> Vec<u8> {
        format!("\"{}\"", "a".repeat(size - 2)).into_bytes()
    }

    const BOUNDARY: &str = "limit-test-boundary";
    const UPLOAD_MESSAGE: &str = "Upload is too large: the maximum file size is 2 MB";

    fn multipart_of(file_size: usize) -> (String, Vec<u8>) {
        let mut body = format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"cover.png\"\r\nContent-Type: image/png\r\n\r\n"
        )
        .into_bytes();
        body.extend(std::iter::repeat_n(b'x', file_size));
        body.extend(
            format!(
                "\r\n--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"selected\"\r\n\r\ntrue\r\n--{BOUNDARY}--\r\n"
            )
            .into_bytes(),
        );
        (format!("multipart/form-data; boundary={BOUNDARY}"), body)
    }

    fn assert_too_large(status: StatusCode, body: &serde_json::Value, path: &str, message: &str) {
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(body["status"], 413);
        assert_eq!(body["error"], "Payload Too Large");
        // the API's errors carry Spring's status prefix in the message
        assert_eq!(
            body["message"],
            format!("413 PAYLOAD_TOO_LARGE \"{message}\"")
        );
        assert_eq!(body["path"], path);
    }

    /// The limit is the same for every kind of body: JSON and raw bytes get the room a
    /// multipart request needs around its file.
    #[tokio::test]
    async fn json_up_to_the_limit_is_read_and_one_byte_more_is_refused() {
        let (status, _) = send("/json", "application/json", json_of(MAX_BODY_SIZE)).await;
        assert_eq!(status, StatusCode::OK);
        let (status, body) = send("/json", "application/json", json_of(MAX_BODY_SIZE + 1)).await;
        assert_too_large(status, &body, "/json", "Request body is too large");
    }

    #[tokio::test]
    async fn raw_bytes_up_to_the_limit_are_read_and_one_more_is_refused() {
        let (status, body) =
            send("/bytes", "application/octet-stream", vec![0; MAX_BODY_SIZE]).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, serde_json::json!(MAX_BODY_SIZE));
        let (status, body) = send(
            "/bytes",
            "application/octet-stream",
            vec![0; MAX_BODY_SIZE + 1],
        )
        .await;
        assert_too_large(status, &body, "/bytes", "Request body is too large");
    }

    /// A file of exactly the advertised size uploads, with its multipart framing on top.
    #[tokio::test]
    async fn a_file_of_the_advertised_size_uploads_and_one_byte_more_is_refused() {
        let (content_type, body) = multipart_of(MAX_UPLOAD_FILE_SIZE);
        assert!(body.len() > MAX_UPLOAD_FILE_SIZE);
        let (status, answer) = send("/upload", &content_type, body).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            answer,
            serde_json::json!(format!("Some({MAX_UPLOAD_FILE_SIZE})"))
        );

        let (content_type, body) = multipart_of(MAX_UPLOAD_FILE_SIZE + 1);
        let (status, answer) = send("/upload", &content_type, body).await;
        assert_too_large(status, &answer, "/upload", UPLOAD_MESSAGE);
    }

    /// A file far over the limit runs into the body limit while it is read: the same answer.
    #[tokio::test]
    async fn a_file_beyond_the_body_limit_is_refused_the_same_way() {
        let (content_type, body) = multipart_of(3 * MAX_UPLOAD_FILE_SIZE);
        let (status, answer) = send("/upload", &content_type, body).await;
        assert_too_large(status, &answer, "/upload", UPLOAD_MESSAGE);
    }

    /// When a part other than the file makes the body too large, the answer is the same
    /// 413, worded so that it holds for this case too.
    #[tokio::test]
    async fn a_too_large_part_next_to_the_file_is_refused_too() {
        let (content_type, mut body) = multipart_of(10);
        let end = format!("--{BOUNDARY}--\r\n");
        body.truncate(body.len() - end.len());
        body.extend(
            format!("--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"selected\"\r\n\r\n")
                .into_bytes(),
        );
        body.extend(std::iter::repeat_n(b'x', MAX_BODY_SIZE));
        body.extend(format!("\r\n{end}").into_bytes());
        let (status, answer) = send("/upload", &content_type, body).await;
        assert_too_large(status, &answer, "/upload", UPLOAD_MESSAGE);
    }

    /// In frames, the file itself crosses the body limit while it is read.
    #[tokio::test]
    async fn a_file_that_crosses_the_body_limit_while_it_is_read_is_refused_the_same_way() {
        let (content_type, body) = multipart_of(3 * MAX_UPLOAD_FILE_SIZE);
        let (status, answer) = send_body("/upload", &content_type, testing::in_frames(body)).await;
        assert_too_large(status, &answer, "/upload", UPLOAD_MESSAGE);
    }

    /// A body in frames is counted like one that arrives at once.
    #[tokio::test]
    async fn json_in_frames_is_refused_one_byte_over_the_limit() {
        let body = testing::in_frames(json_of(MAX_BODY_SIZE));
        let (status, _) = send_body("/json", "application/json", body).await;
        assert_eq!(status, StatusCode::OK);
        let body = testing::in_frames(json_of(MAX_BODY_SIZE + 1));
        let (status, answer) = send_body("/json", "application/json", body).await;
        assert_too_large(status, &answer, "/json", "Request body is too large");
    }

    #[tokio::test]
    async fn a_malformed_multipart_body_is_still_a_bad_request() {
        let content_type = format!("multipart/form-data; boundary={BOUNDARY}");
        let (status, _) = send("/upload", &content_type, b"not multipart".to_vec()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    /// Only the extractors' plain-text refusal is rewritten, not a 413 a handler answers.
    #[tokio::test]
    async fn a_413_with_its_own_body_is_left_alone() {
        let (status, body) = send("/proxied", "application/json", b"{}".to_vec()).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(body, serde_json::json!("from elsewhere"));
    }

    #[tokio::test]
    async fn a_413_without_a_body_is_left_alone() {
        let (status, body) = send("/bare", "application/json", b"{}".to_vec()).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(body, serde_json::json!(""));
    }

    /// The extractors' other plain-text refusals keep their status and their text.
    #[tokio::test]
    async fn a_plain_text_refusal_that_is_not_a_413_is_left_alone() {
        let (status, body) = send("/json", "application/json", b"{not json".to_vec()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.is_string(), "{body}");
        let (status, body) = send("/json", "text/plain", b"{}".to_vec()).await;
        assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert!(body.is_string(), "{body}");
    }
}
