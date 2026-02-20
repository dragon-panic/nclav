use std::sync::Arc;

use axum::routing::{get, post};
use axum::Router;
use nclav_driver::Driver;
use nclav_store::StateStore;
use tower_http::trace::TraceLayer;

use crate::handlers;
use crate::state::AppState;

pub fn build_app(store: Arc<dyn StateStore>, driver: Arc<dyn Driver>) -> Router {
    let state = AppState { store, driver };

    Router::new()
        // Health
        .route("/health", get(handlers::health))
        .route("/ready", get(handlers::ready))
        // Reconcile
        .route("/reconcile", post(handlers::post_reconcile))
        .route("/reconcile/dry-run", post(handlers::post_reconcile_dry_run))
        // Enclaves
        .route("/enclaves", get(handlers::list_enclaves))
        .route("/enclaves/:id", get(handlers::get_enclave))
        .route("/enclaves/:id/graph", get(handlers::get_enclave_graph))
        // Graphs
        .route("/graph", get(handlers::get_system_graph))
        // Events
        .route("/events", get(handlers::list_events))
        // Status
        .route("/status", get(handlers::status))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode};
    use nclav_driver::LocalDriver;
    use nclav_store::InMemoryStore;
    use tower::util::ServiceExt;

    fn test_app() -> Router {
        let store = Arc::new(InMemoryStore::new());
        let driver = Arc::new(LocalDriver::new());
        build_app(store, driver)
    }

    #[tokio::test]
    async fn health_returns_200() {
        let app = test_app();
        let resp = app
            .oneshot(Request::builder().uri("/health").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn ready_returns_200_with_empty_store() {
        let app = test_app();
        let resp = app
            .oneshot(Request::builder().uri("/ready").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn enclaves_empty_list() {
        let app = test_app();
        let resp = app
            .oneshot(Request::builder().uri("/enclaves").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn enclave_not_found_returns_404() {
        let app = test_app();
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/enclaves/nonexistent")
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
            .oneshot(Request::builder().uri("/status").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn graph_returns_200() {
        let app = test_app();
        let resp = app
            .oneshot(Request::builder().uri("/graph").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn events_returns_200() {
        let app = test_app();
        let resp = app
            .oneshot(Request::builder().uri("/events").body(Body::empty()).unwrap())
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
                Request::builder()
                    .method(Method::POST)
                    .uri("/reconcile")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        // Should be 4xx or 5xx — directory doesn't exist
        assert!(resp.status().is_client_error() || resp.status().is_server_error());
    }
}
