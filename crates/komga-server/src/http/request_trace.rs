//! What the request trace logs as a failure. Every `5xx` response is one, as with
//! `TraceLayer::new_for_http`, except a refusal the server issues on purpose to shed load:
//! under a flood those come once per request, and the place that refuses reports them in
//! one line per interval instead.

use axum::http::Response;
use tower_http::classify::{
    ClassifiedResponse, ClassifyResponse, NeverClassifyEos, ServerErrorsAsFailures,
    ServerErrorsFailureClass,
};

/// Response extension that marks a deliberate refusal, see the module docs.
#[derive(Clone, Copy)]
pub struct DeliberateRefusal;

#[derive(Clone, Copy, Default)]
pub struct FailuresExceptDeliberateRefusals;

impl ClassifyResponse for FailuresExceptDeliberateRefusals {
    type FailureClass = ServerErrorsFailureClass;
    type ClassifyEos = NeverClassifyEos<ServerErrorsFailureClass>;

    fn classify_response<B>(
        self,
        res: &Response<B>,
    ) -> ClassifiedResponse<Self::FailureClass, Self::ClassifyEos> {
        if res.extensions().get::<DeliberateRefusal>().is_some() {
            ClassifiedResponse::Ready(Ok(()))
        } else {
            ServerErrorsAsFailures::new().classify_response(res)
        }
    }

    fn classify_error<E>(self, error: &E) -> Self::FailureClass
    where
        E: std::fmt::Display + 'static,
    {
        ServerErrorsAsFailures::new().classify_error(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;

    fn is_failure(response: &Response<()>) -> bool {
        matches!(
            FailuresExceptDeliberateRefusals.classify_response(response),
            ClassifiedResponse::Ready(Err(_))
        )
    }

    #[test]
    fn server_errors_are_failures_unless_marked_deliberate() {
        let status = |status: StatusCode| Response::builder().status(status).body(()).unwrap();
        assert!(!is_failure(&status(StatusCode::OK)));
        assert!(!is_failure(&status(StatusCode::UNAUTHORIZED)));
        assert!(is_failure(&status(StatusCode::INTERNAL_SERVER_ERROR)));
        assert!(is_failure(&status(StatusCode::SERVICE_UNAVAILABLE)));

        let mut refusal = status(StatusCode::SERVICE_UNAVAILABLE);
        refusal.extensions_mut().insert(DeliberateRefusal);
        assert!(!is_failure(&refusal));
    }
}
