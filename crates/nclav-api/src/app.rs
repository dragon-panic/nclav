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

    // ── Terraform HTTP state backend ──────────────────────────────────────────
    //
    // Verifies that the /terraform/state/:enc/:part routes implement the
    // Terraform HTTP backend protocol correctly:
    //   GET  → 204 No Content (no state yet) or 200 OK (with blob)
    //   POST → 200 OK (store blob)
    //   DEL  → 200 OK (clear blob)
    //   POST /lock  → 200 OK (acquired) or 409 Conflict (already locked)
    //   DEL  /lock  → 200 OK (released)

    const STATE_URL: &str = "/terraform/state/enc/part";
    const LOCK_URL:  &str = "/terraform/state/enc/part/lock";

    fn tf_state_blob() -> serde_json::Value {
        serde_json::json!({ "version": 4, "serial": 1, "lineage": "abc" })
    }

    fn lock_info(id: &str) -> serde_json::Value {
        serde_json::json!({ "ID": id, "Operation": "OperationTypeApply", "Who": "tester" })
    }

    #[tokio::test]
    async fn tf_state_get_returns_no_content_when_empty() {
        let app = test_app();
        let resp = app
            .oneshot(authed(Request::builder().uri(STATE_URL)).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn tf_state_post_stores_blob_and_get_retrieves_it() {
        let app = test_app();
        let blob = tf_state_blob().to_string();

        // POST to store.
        let post_resp = app
            .clone()
            .oneshot(
                authed(
                    Request::builder()
                        .method(Method::POST)
                        .uri(STATE_URL)
                        .header("content-type", "application/json"),
                )
                .body(Body::from(blob.clone()))
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(post_resp.status(), StatusCode::OK);

        // GET to retrieve.
        let get_resp = app
            .oneshot(authed(Request::builder().uri(STATE_URL)).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(get_resp.status(), StatusCode::OK);
        let body_bytes = axum::body::to_bytes(get_resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(body_bytes, blob.as_bytes());
    }

    #[tokio::test]
    async fn tf_state_delete_clears_stored_state() {
        let app = test_app();

        // Store something first.
        app.clone()
            .oneshot(
                authed(
                    Request::builder()
                        .method(Method::POST)
                        .uri(STATE_URL)
                        .header("content-type", "application/json"),
                )
                .body(Body::from(tf_state_blob().to_string()))
                .unwrap(),
            )
            .await
            .unwrap();

        // Delete it.
        let del_resp = app
            .clone()
            .oneshot(
                authed(Request::builder().method(Method::DELETE).uri(STATE_URL))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(del_resp.status(), StatusCode::OK);

        // GET should now be 204.
        let get_resp = app
            .oneshot(authed(Request::builder().uri(STATE_URL)).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(get_resp.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn tf_state_lock_returns_200_on_first_acquire() {
        let app = test_app();
        let info = lock_info("lock-id-1").to_string();

        let resp = app
            .oneshot(
                authed(
                    Request::builder()
                        .method(Method::POST)
                        .uri(LOCK_URL)
                        .header("content-type", "application/json"),
                )
                .body(Body::from(info))
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn tf_state_lock_conflict_returns_409_with_holder_info() {
        let app = test_app();
        let info = lock_info("lock-id-1").to_string();

        // First acquire — should succeed.
        app.clone()
            .oneshot(
                authed(
                    Request::builder()
                        .method(Method::POST)
                        .uri(LOCK_URL)
                        .header("content-type", "application/json"),
                )
                .body(Body::from(info.clone()))
                .unwrap(),
            )
            .await
            .unwrap();

        // Second acquire — should conflict.
        let conflict = app
            .oneshot(
                authed(
                    Request::builder()
                        .method(Method::POST)
                        .uri(LOCK_URL)
                        .header("content-type", "application/json"),
                )
                .body(Body::from(lock_info("lock-id-2").to_string()))
                .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(conflict.status(), StatusCode::CONFLICT);

        // Body should contain the existing lock holder info so the operator
        // knows who holds the lock (this is what Terraform shows on conflict).
        let body_bytes = axum::body::to_bytes(conflict.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        assert_eq!(body["ID"], "lock-id-1");
    }

    #[tokio::test]
    async fn tf_state_unlock_releases_lock_and_allows_reacquire() {
        let app = test_app();
        let info = lock_info("lock-id-1").to_string();

        // Acquire.
        app.clone()
            .oneshot(
                authed(
                    Request::builder()
                        .method(Method::POST)
                        .uri(LOCK_URL)
                        .header("content-type", "application/json"),
                )
                .body(Body::from(info.clone()))
                .unwrap(),
            )
            .await
            .unwrap();

        // Release via DELETE /lock with lock ID in body.
        let unlock_body = serde_json::json!({ "ID": "lock-id-1" }).to_string();
        let unlock_resp = app
            .clone()
            .oneshot(
                authed(Request::builder().method(Method::DELETE).uri(LOCK_URL))
                    .body(Body::from(unlock_body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unlock_resp.status(), StatusCode::OK);

        // Re-acquire should now succeed (no 409).
        let relock = app
            .oneshot(
                authed(
                    Request::builder()
                        .method(Method::POST)
                        .uri(LOCK_URL)
                        .header("content-type", "application/json"),
                )
                .body(Body::from(lock_info("lock-id-2").to_string()))
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(relock.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn tf_state_independent_keys_do_not_share_state() {
        let app = test_app();
        let blob = tf_state_blob().to_string();

        // POST to enc/part.
        app.clone()
            .oneshot(
                authed(
                    Request::builder()
                        .method(Method::POST)
                        .uri(STATE_URL)
                        .header("content-type", "application/json"),
                )
                .body(Body::from(blob.clone()))
                .unwrap(),
            )
            .await
            .unwrap();

        // A different partition key should still be empty.
        let other_resp = app
            .oneshot(
                authed(Request::builder().uri("/terraform/state/enc/other-part"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(other_resp.status(), StatusCode::NO_CONTENT);
    }
}
