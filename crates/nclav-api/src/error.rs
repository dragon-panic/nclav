use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
}

impl ApiError {
    pub fn bad_request(msg: impl Into<String>) -> Self {
        ApiError { status: StatusCode::BAD_REQUEST, message: msg.into() }
    }

    pub fn unprocessable(msg: impl Into<String>) -> Self {
        ApiError { status: StatusCode::UNPROCESSABLE_ENTITY, message: msg.into() }
    }

    pub fn not_found(msg: impl Into<String>) -> Self {
        ApiError { status: StatusCode::NOT_FOUND, message: msg.into() }
    }

    pub fn internal(msg: impl Into<String>) -> Self {
        ApiError { status: StatusCode::INTERNAL_SERVER_ERROR, message: msg.into() }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = Json(json!({ "error": self.message }));
        (self.status, body).into_response()
    }
}

impl From<nclav_reconciler::ReconcileError> for ApiError {
    fn from(e: nclav_reconciler::ReconcileError) -> Self {
        match e {
            nclav_reconciler::ReconcileError::Graph(_) |
            nclav_reconciler::ReconcileError::Config(_) => ApiError::unprocessable(e.to_string()),
            _ => ApiError::internal(e.to_string()),
        }
    }
}

impl From<nclav_store::StoreError> for ApiError {
    fn from(e: nclav_store::StoreError) -> Self {
        ApiError::internal(e.to_string())
    }
}
