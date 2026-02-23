use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
};
use base64::Engine as _;

use crate::state::AppState;

/// Axum middleware that requires a valid `Authorization` header on every request.
///
/// Accepts two formats:
///   - `Bearer <token>` — used by the nclav CLI and API clients
///   - `Basic base64(<user>:<token>)` — used by Terraform's HTTP state backend,
///     which sends the token as the Basic auth password (username is ignored)
///
/// Returns 401 for missing, malformed, or incorrect tokens.
/// Applied to all routes — no public endpoints.
pub async fn require_bearer_token(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let header = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());

    let token = header.and_then(|s| {
        if let Some(t) = s.strip_prefix("Bearer ") {
            return Some(t.to_string());
        }
        if let Some(encoded) = s.strip_prefix("Basic ") {
            if let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(encoded) {
                if let Ok(creds) = std::str::from_utf8(&decoded) {
                    // Basic auth format is "username:password"; token is the password
                    if let Some((_, password)) = creds.split_once(':') {
                        return Some(password.to_string());
                    }
                }
            }
        }
        None
    });

    match token {
        Some(t) if t == state.auth_token.as_str() => next.run(request).await,
        _ => (StatusCode::UNAUTHORIZED, "Unauthorized\n").into_response(),
    }
}
