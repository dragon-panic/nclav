use std::sync::Arc;

use axum::middleware;
use axum::routing::{delete, get, post};
use axum::Router;
use nclav_driver::DriverRegistry;
use nclav_store::StateStore;
use tower_http::trace::TraceLayer;

use crate::auth::require_bearer_token;
use crate::handlers;
use crate::state::AppState;

pub fn build_app(
    store: Arc<dyn StateStore>,
    registry: Arc<DriverRegistry>,
    auth_token: Arc<String>,
    api_base: String,
) -> Router {
    let state = AppState { store, registry, auth_token, api_base: Arc::new(api_base) };

    Router::new()
        // Health
        .route("/health", get(handlers::health))
        .route("/ready", get(handlers::ready))
        // Reconcile
        .route("/reconcile", post(handlers::post_reconcile))
        .route("/reconcile/dry-run", post(handlers::post_reconcile_dry_run))
        // Enclaves
        .route("/enclaves", get(handlers::list_enclaves))
        .route(
            "/enclaves/:id",
            get(handlers::get_enclave).delete(handlers::delete_enclave),
        )
        .route("/enclaves/:id/graph", get(handlers::get_enclave_graph))
        // Partition destroy
        .route("/enclaves/:id/partitions/:part", delete(handlers::delete_partition))
        // IaC run logs
        .route("/enclaves/:id/partitions/:part/iac/runs", get(handlers::list_iac_runs))
        .route("/enclaves/:id/partitions/:part/iac/runs/latest", get(handlers::get_latest_iac_run))
        .route("/enclaves/:id/partitions/:part/iac/runs/:run_id", get(handlers::get_iac_run))
        // Terraform HTTP state backend
        .route(
            "/terraform/state/:enc/:part",
            get(handlers::get_tf_state)
                .post(handlers::put_tf_state)
                .delete(handlers::delete_tf_state),
        )
        .route(
            "/terraform/state/:enc/:part/lock",
            post(handlers::lock_tf_state).delete(handlers::unlock_tf_state),
        )
        // Graphs
        .route("/graph", get(handlers::get_system_graph))
        // Events
        .route("/events", get(handlers::list_events))
        // Status
        .route("/status", get(handlers::status))
        // Orphan detection
        .route("/orphans", get(handlers::list_orphans))
        // Auth middleware applies to all routes above
        .route_layer(middleware::from_fn_with_state(state.clone(), require_bearer_token))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode};
    use base64::Engine as _;
    use nclav_domain::CloudTarget;
    use nclav_driver::LocalDriver;
    use nclav_store::InMemoryStore;
    use tower::util::ServiceExt;

    const TEST_TOKEN: &str = "test-token";

    fn test_app() -> Router {
        let store = Arc::new(InMemoryStore::new());
        let driver = Arc::new(LocalDriver::new());
        let mut registry = DriverRegistry::new(CloudTarget::Local);
        registry.register(CloudTarget::Local, driver);
        let registry = Arc::new(registry);
        build_app(store, registry, Arc::new(TEST_TOKEN.to_string()), "http://127.0.0.1:8080".into())
    }

    fn authed(req: axum::http::request::Builder) -> axum::http::request::Builder {
        req.header("Authorization", format!("Bearer {}", TEST_TOKEN))
    }

    #[tokio::test]
    async fn unauthenticated_request_returns_401() {
        let app = test_app();
        let resp = app
            .oneshot(Request::builder().uri("/health").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn wrong_token_returns_401() {
        let app = test_app();
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .header("Authorization", "Bearer wrong-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn basic_auth_with_correct_token_returns_200() {
        // Terraform's HTTP backend sends the token as the Basic auth password.
        let app = test_app();
        let credentials = base64::engine::general_purpose::STANDARD
            .encode(format!("nclav:{}", TEST_TOKEN));
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .header("Authorization", format!("Basic {}", credentials))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn basic_auth_with_wrong_token_returns_401() {
        let app = test_app();
        let credentials = base64::engine::general_purpose::STANDARD.encode("nclav:wrong-token");
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .header("Authorization", format!("Basic {}", credentials))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn health_returns_200() {
        let app = test_app();
        let resp = app
            .oneshot(authed(Request::builder().uri("/health")).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn ready_returns_200_with_empty_store() {
        let app = test_app();
        let resp = app
            .oneshot(authed(Request::builder().uri("/ready")).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn enclaves_empty_list() {
        let app = test_app();
        let resp = app
            .oneshot(authed(Request::builder().uri("/enclaves")).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn enclave_not_found_returns_404() {
        let app = test_app();
        let resp = app
            .oneshot(
                authed(Request::builder().uri("/enclaves/nonexistent"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn status_returns_200() {
        let app = test_app();
        let resp = app
            .oneshot(authed(Request::builder().uri("/status")).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn graph_returns_200() {
        let app = test_app();
        let resp = app
            .oneshot(authed(Request::builder().uri("/graph")).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn events_returns_200() {
        let app = test_app();
        let resp = app
            .oneshot(authed(Request::builder().uri("/events")).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn reconcile_invalid_dir_returns_error() {
        let app = test_app();
        let body = serde_json::json!({ "enclaves_dir": "/no/such/path" });
        let resp = app
            .oneshot(
                authed(
                    Request::builder()
                        .method(Method::POST)
                        .uri("/reconcile")
                        .header("content-type", "application/json"),
                )
                .body(Body::from(body.to_string()))
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(resp.status().is_client_error() || resp.status().is_server_error());
    }
}
