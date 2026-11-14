//! The idle limit for request bodies. A handler that waits for its body holds one thread
//! of the blocking pool, and a client that went away in the middle of a body would hold
//! it for as long as the connection stays open.
//!
//! - When no part of a body arrives for `server.body-idle-timeout`, the request answers
//!   408 in the API's error format. The default of 60 seconds is where Java komga gives
//!   up on a body, at Tomcat's default connection timeout.
//! - The time restarts with every part that arrives: a slow upload that keeps moving is
//!   never cut, whatever it takes in total. So this ends requests from clients that are
//!   gone, not from one that sends a byte now and then on purpose.
//! - Only the wait for a request body is limited. Requests without a body, the handler's
//!   own work and response bodies are not.
//! - The extractors report a body that failed as a malformed request, each in its own
//!   way. The body remembers that it ran out of time, so the answer does not depend on
//!   which extractor read it.

use crate::error::ApiError;
use axum::body::{Body, Bytes};
use axum::extract::Request;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Router;
use http_body::{Frame, SizeHint};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::time::Sleep;

pub const DEFAULT_BODY_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// Applies the idle limit to the request bodies of every route of the router.
pub fn apply<S: Clone + Send + Sync + 'static>(router: Router<S>, idle: Duration) -> Router<S> {
    router.layer(axum::middleware::from_fn(
        move |request: Request, next: Next| limit_body_idle_time(idle, request, next),
    ))
}

async fn limit_body_idle_time(idle: Duration, request: Request, next: Next) -> Response {
    use http_body::Body as _;
    // most requests have no body: nothing to wait for
    if request.body().is_end_stream() {
        return next.run(request).await;
    }
    let timed_out = Arc::new(AtomicBool::new(false));
    let request = request.map(|body| {
        Body::new(IdleTimeoutBody {
            body,
            idle,
            idle_timer: None,
            reading: false,
            timed_out: timed_out.clone(),
        })
    });
    let response = next.run(request).await;
    // only the error the failed body led to is replaced: an answer from a handler that
    // carried on without the body stands
    let failed = response.status().is_client_error() || response.status().is_server_error();
    if timed_out.load(Ordering::Relaxed) && failed {
        tracing::debug!("request body idle for {idle:?}, answering 408");
        return ApiError::request_timeout("Request body was not received in time").into_response();
    }
    response
}

/// A request body that fails once nothing has arrived for `idle`.
struct IdleTimeoutBody {
    body: Body,
    idle: Duration,
    /// counts only while a read of the body is pending
    idle_timer: Option<Pin<Box<Sleep>>>,
    /// a read is pending and `idle_timer` is counting for it
    reading: bool,
    timed_out: Arc<AtomicBool>,
}

#[derive(Debug)]
struct BodyIdleTimeout;

impl std::fmt::Display for BodyIdleTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("request body was not received in time")
    }
}

impl std::error::Error for BodyIdleTimeout {}

impl http_body::Body for IdleTimeoutBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, axum::Error>>> {
        // the body first: what has arrived is handed out however long the reader took
        // to ask for it
        let this = &mut *self;
        if let Poll::Ready(frame) = Pin::new(&mut this.body).poll_frame(cx) {
            this.reading = false;
            return Poll::Ready(frame);
        }
        let idle_timer = match &mut this.idle_timer {
            Some(idle_timer) => {
                if !this.reading {
                    // not `reset`: adding a very long limit to the current time
                    // would overflow, a new sleep is clamped
                    idle_timer.set(tokio::time::sleep(this.idle));
                }
                idle_timer
            }
            none => none.insert(Box::pin(tokio::time::sleep(this.idle))),
        };
        this.reading = true;
        if idle_timer.as_mut().poll(cx).is_pending() {
            return Poll::Pending;
        }
        self.timed_out.store(true, Ordering::Relaxed);
        Poll::Ready(Some(Err(axum::Error::new(BodyIdleTimeout))))
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.body.size_hint()
    }
}

/// What the route tests share: bodies that stop arriving, and requests answered by the
/// router the server runs, with a limit short enough to wait for.
#[cfg(test)]
pub(crate) mod testing {
    use axum::body::{Body, Bytes};
    use axum::extract::Request;
    use axum::http::StatusCode;
    use std::time::Duration;
    use tower::ServiceExt;

    pub(crate) const SHORT_IDLE_TIMEOUT: Duration = Duration::from_millis(500);

    /// The first half of the body, then nothing for twenty times the limit, as on a
    /// connection that stays open while the rest does not come. It ends in an error
    /// after that, because a request nobody gives up on could not be abandoned by the
    /// test either: its thread of the blocking pool would wait for the body without end.
    pub(crate) fn stalled(body: Vec<u8>) -> Body {
        use futures_util::StreamExt;
        let sent = body[..body.len() / 2].to_vec();
        let first = futures_util::stream::once(async move { Ok(Bytes::from(sent)) });
        let rest = futures_util::stream::once(async {
            tokio::time::sleep(20 * SHORT_IDLE_TIMEOUT).await;
            Err(std::io::Error::other("the rest of the body never came"))
        });
        Body::from_stream(first.chain(rest))
    }

    /// The body in parts with a pause before each, as a slow connection delivers it.
    pub(crate) fn with_pauses(body: Vec<u8>, parts: usize, pause: Duration) -> Body {
        use futures_util::StreamExt;
        let size = body.len().div_ceil(parts).max(1);
        let frames: Vec<Result<Bytes, std::io::Error>> = body
            .chunks(size)
            .map(|frame| Ok(Bytes::copy_from_slice(frame)))
            .collect();
        Body::from_stream(
            futures_util::stream::iter(frames).then(move |frame| async move {
                tokio::time::sleep(pause).await;
                frame
            }),
        )
    }

    /// The request answered by the server's own router, with every layer it has and
    /// `SHORT_IDLE_TIMEOUT` configured as the idle limit.
    pub(crate) async fn send_to_server(
        state: &crate::state::AppState,
        request: Request<Body>,
    ) -> (StatusCode, serde_json::Value) {
        send_to_server_configured(state, Some(SHORT_IDLE_TIMEOUT), request).await
    }

    /// The same with the idle limit turned off in the configuration.
    pub(crate) async fn send_to_server_without_limit(
        state: &crate::state::AppState,
        request: Request<Body>,
    ) -> (StatusCode, serde_json::Value) {
        send_to_server_configured(state, None, request).await
    }

    async fn send_to_server_configured(
        state: &crate::state::AppState,
        body_idle_timeout: Option<Duration>,
        request: Request<Body>,
    ) -> (StatusCode, serde_json::Value) {
        let mut state = state.clone();
        state.config = std::sync::Arc::new(crate::config::ServerConfig {
            body_idle_timeout,
            ..(*state.config).clone()
        });
        let router = crate::build_router(state);
        let response = router.oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, body)
    }

    /// The same request with its body cut off half way.
    pub(crate) async fn send_stalled_to_server(
        state: &crate::state::AppState,
        request: Request<Body>,
    ) -> (StatusCode, serde_json::Value) {
        let (parts, body) = request.into_parts();
        let body = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        send_to_server(state, Request::from_parts(parts, stalled(body.to_vec()))).await
    }

    pub(crate) fn assert_body_timed_out(status: StatusCode, body: &serde_json::Value, path: &str) {
        assert_eq!(status, StatusCode::REQUEST_TIMEOUT);
        assert_eq!(body["status"], 408);
        assert_eq!(body["error"], "Request Timeout");
        assert_eq!(
            body["message"],
            "408 REQUEST_TIMEOUT \"Request body was not received in time\""
        );
        assert_eq!(body["path"], path);
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{stalled, with_pauses, SHORT_IDLE_TIMEOUT};
    use super::*;
    use axum::extract::Multipart;
    use axum::http::StatusCode;
    use axum::routing::post;
    use axum::Json;
    use tower::ServiceExt;

    async fn upload(mut multipart: Multipart) -> Result<String, ApiError> {
        use crate::http::body_limit::{file_bytes, multipart_error};
        let mut size = 0;
        while let Some(field) = multipart.next_field().await.map_err(multipart_error)? {
            size += file_bytes(field).await?.len();
        }
        Ok(size.to_string())
    }

    /// The three kinds of body the server reads, behind the limit and the layers of the
    /// real router that matter here: the blocking pool every request runs on and the
    /// middleware that fills in the path of an error.
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
                    "/slow",
                    post(|body: Bytes| async move {
                        tokio::time::sleep(2 * SHORT_IDLE_TIMEOUT).await;
                        body.len().to_string()
                    }),
                )
                .route(
                    "/ignores-body",
                    post(|| async {
                        tokio::time::sleep(2 * SHORT_IDLE_TIMEOUT).await;
                        "done"
                    }),
                )
                .route(
                    "/carries-on",
                    post(
                        |body: Result<Bytes, axum::extract::rejection::BytesRejection>| async move {
                            match body {
                                Ok(_) => "read",
                                Err(_) => "done without the body",
                            }
                        },
                    ),
                ),
            SHORT_IDLE_TIMEOUT,
        )
        .layer(axum::middleware::from_fn(
            crate::http::error_path::error_path_middleware,
        ))
        .layer(axum::middleware::from_fn(
            crate::http::offload::offload_middleware,
        ))
    }

    async fn send(
        method: &str,
        path: &str,
        content_type: &str,
        body: Body,
    ) -> (StatusCode, serde_json::Value) {
        let request = axum::http::Request::builder()
            .method(method)
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

    const BOUNDARY: &str = "timeout-test-boundary";

    fn multipart_of(file_size: usize) -> (String, Vec<u8>) {
        let mut body = format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"cover.png\"\r\nContent-Type: image/png\r\n\r\n"
        )
        .into_bytes();
        body.extend(std::iter::repeat_n(b'x', file_size));
        body.extend(format!("\r\n--{BOUNDARY}--\r\n").into_bytes());
        (format!("multipart/form-data; boundary={BOUNDARY}"), body)
    }

    fn json_of(size: usize) -> Vec<u8> {
        format!("\"{}\"", "a".repeat(size - 2)).into_bytes()
    }

    /// Whichever extractor waits for the body, the answer is the same 408.
    #[tokio::test]
    async fn a_body_that_stops_arriving_answers_408_for_every_kind_of_body() {
        let (multipart, upload) = multipart_of(1000);
        for (path, content_type, body) in [
            ("/json", "application/json", json_of(1000)),
            ("/bytes", "application/octet-stream", vec![0; 1000]),
            ("/upload", multipart.as_str(), upload),
        ] {
            let (status, answer) = send("POST", path, content_type, stalled(body)).await;
            testing::assert_body_timed_out(status, &answer, path);
        }
    }

    /// The limit is on the time without data, not on the whole body: this one takes
    /// more than twice the limit and no pause reaches it.
    #[tokio::test]
    async fn a_slow_body_that_keeps_arriving_is_read_in_full() {
        let (content_type, upload) = multipart_of(1000);
        for (path, content_type, body) in [
            ("/json", "application/json", json_of(1000)),
            ("/bytes", "application/octet-stream", vec![0; 1000]),
            ("/upload", content_type.as_str(), upload),
        ] {
            let body = with_pauses(body, 12, SHORT_IDLE_TIMEOUT / 5);
            let (status, _) = send("POST", path, content_type, body).await;
            assert_eq!(status, StatusCode::OK, "{path}");
        }
    }

    /// Only the wait for the body counts: a handler that takes longer than the limit
    /// after its body has arrived is not cut.
    #[tokio::test]
    async fn the_handlers_own_time_is_not_limited() {
        let (status, answer) = send(
            "POST",
            "/slow",
            "application/octet-stream",
            Body::from(vec![0; 10]),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(answer, serde_json::json!(10));
    }

    /// A body nobody reads starts no wait, even when it never arrives.
    #[tokio::test]
    async fn a_body_that_is_not_read_is_not_waited_for() {
        let (status, answer) = send(
            "POST",
            "/ignores-body",
            "application/octet-stream",
            stalled(vec![0; 10]),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(answer, serde_json::json!("done"));
    }

    /// The 408 stands in for the error a failed body led to. A handler that carried on
    /// without the body keeps its own answer, so a client is never told that a request
    /// failed which was in fact carried out.
    #[tokio::test]
    async fn an_answer_given_without_the_body_is_not_replaced() {
        let body = stalled(vec![0; 10]);
        let (status, answer) = send("POST", "/carries-on", "application/octet-stream", body).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(answer, serde_json::json!("done without the body"));
    }

    /// The time counts only while a read is pending. A part that arrived while nobody
    /// was reading is handed out, however long ago the last read was.
    #[tokio::test]
    async fn what_has_arrived_is_handed_out_however_late_it_is_read() {
        use http_body_util::BodyExt;
        let idle = Duration::from_millis(50);
        let (parts, incoming) = futures_channel_of_frames();
        let timed_out = Arc::new(AtomicBool::new(false));
        let mut body = IdleTimeoutBody {
            body: incoming,
            idle,
            idle_timer: None,
            reading: false,
            timed_out: timed_out.clone(),
        };
        // a read that finds nothing starts the count
        assert!(tokio::time::timeout(idle / 5, body.frame()).await.is_err());
        parts.send(Bytes::from_static(b"late")).unwrap();
        tokio::time::sleep(4 * idle).await;
        let frame = body.frame().await.unwrap().unwrap();
        assert_eq!(frame.into_data().unwrap(), "late");
        assert!(!timed_out.load(Ordering::Relaxed));
        // and the next read gets the whole limit again, then fails
        tokio::time::sleep(4 * idle).await;
        let started = std::time::Instant::now();
        assert!(body.frame().await.unwrap().is_err());
        assert!(started.elapsed() >= idle);
        assert!(timed_out.load(Ordering::Relaxed));
    }

    /// A configured limit can be longer than a clock can count. It is never reached,
    /// and starting the wait again after a part does not overflow.
    #[tokio::test]
    async fn a_limit_too_long_to_count_is_never_reached() {
        use http_body_util::BodyExt;
        let (parts, incoming) = futures_channel_of_frames();
        let mut body = IdleTimeoutBody {
            body: incoming,
            idle: Duration::from_secs(i64::MAX as u64),
            idle_timer: None,
            reading: false,
            timed_out: Arc::new(AtomicBool::new(false)),
        };
        let pending = Duration::from_millis(20);
        assert!(tokio::time::timeout(pending, body.frame()).await.is_err());
        parts.send(Bytes::from_static(b"part")).unwrap();
        assert!(body.frame().await.unwrap().is_ok());
        assert!(tokio::time::timeout(pending, body.frame()).await.is_err());
    }

    /// A body fed by hand: what is sent arrives as one part.
    fn futures_channel_of_frames() -> (tokio::sync::mpsc::UnboundedSender<Bytes>, Body) {
        let (parts, incoming) = tokio::sync::mpsc::unbounded_channel::<Bytes>();
        let stream = futures_util::stream::unfold(incoming, |mut incoming| async move {
            let part = incoming.recv().await?;
            Some((Ok::<_, std::io::Error>(part), incoming))
        });
        (parts, Body::from_stream(stream))
    }
}
