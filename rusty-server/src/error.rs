//! HTTP error type: a status code plus a JSON `{error, message}` body.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde_json::{Value, json};

/// The reason a request was refused admission.
///
/// Carried verbatim in the response body per `contracts:gateway-protocol`.
#[derive(Debug, Clone, Serialize)]
pub enum AdmissionReason {
    /// The caller lacks the required scope or credential.
    #[serde(rename = "Unauthorized")]
    Unauthorized,
}

impl AdmissionReason {
    /// Render as an RFC 9457 Problem Details response.
    pub fn into_response(self, status: StatusCode) -> Response {
        let body = json!({
            "type": "https://rusty.dev/problems/admission-refused",
            "title": "Admission refused",
            "status": status.as_u16(),
            "reason": self,
        });
        (status, Json(body)).into_response()
    }
}

/// An API error rendered as `{ "error": <kind>, "message": <detail> }` with
/// the matching HTTP status.
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    body: Value,
}

impl ApiError {
    pub fn new(status: StatusCode, kind: &str, message: String) -> Self {
        Self {
            status,
            body: json!({ "error": kind, "message": message }),
        }
    }

    /// 404 — unknown thread, run, or other resource.
    pub fn not_found(message: String) -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", message)
    }

    /// 409 — multitask `reject` hit an active run, queue full, duplicate id.
    pub fn conflict(message: String) -> Self {
        Self::new(StatusCode::CONFLICT, "conflict", message)
    }

    /// 403 — authorization refused the request (R0.9 wave 2: the Cedar
    /// plane forbade an admission, a grant, or an overlay). Distinct from
    /// 422: the payload is well-formed and the state is not conflicting —
    /// the tenant's standing policy simply does not permit this.
    pub fn forbidden(message: String) -> Self {
        Self::new(StatusCode::FORBIDDEN, "forbidden", message)
    }

    /// 429 — a tenant quota rejected the submission (R0.6 wave 3). 429, not
    /// 409 or 503: the request is well-formed and the state is not
    /// conflicting — the tenant is simply over its allowance, and standard
    /// retry-after-backoff client behavior is exactly right.
    pub fn too_many_requests(message: String) -> Self {
        Self::new(StatusCode::TOO_MANY_REQUESTS, "quota_exceeded", message)
    }

    /// 400 — malformed payload, unknown graph, bad strategy, non-object input.
    pub fn bad_request(message: String) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "bad_request", message)
    }

    /// 422 — the request is well-formed but cannot be processed: replaying a
    /// run whose graph is not registered in this process, or whose journal
    /// carries evidence server-side replay cannot re-drive.
    pub fn unprocessable(message: String) -> Self {
        Self::new(StatusCode::UNPROCESSABLE_ENTITY, "unprocessable", message)
    }

    /// 500 — checkpointer IO failures and other internal errors.
    pub fn internal(message: String) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal_error", message)
    }

    /// 503 — the server is draining (graceful shutdown) and rejects new
    /// work. Distinct from 500 so a load balancer's retry lands on a pod
    /// that is still serving.
    pub fn shutting_down(message: String) -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, "shutting_down", message)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(self.body)).into_response()
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.status, self.body)
    }
}

impl std::error::Error for ApiError {}
