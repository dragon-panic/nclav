use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use nclav_driver::{Driver, GcpDriver, GcpDriverConfig, LocalDriver};
use nclav_store::{EnclaveState, InMemoryStore};

use crate::cli::{CloudArg, GraphOutput};
use crate::output;

// ── Bootstrap ─────────────────────────────────────────────────────────────────

pub async fn bootstrap(
    cloud: CloudArg,
    remote: Option<String>,
    gcp_parent: Option<String>,
    gcp_billing_account: Option<String>,
    gcp_default_region: String,
    gcp_project_prefix: Option<String>,
    port: u16,
) -> Result<()> {
    if remote.is_some() {
        anyhow::bail!("bootstrap does not support --remote; run the server locally");
    }

    match cloud {
        CloudArg::Azure => {
            anyhow::bail!("Azure bootstrap not yet implemented");
        }
        CloudArg::Local => {
            let addr = format!("0.0.0.0:{port}");
            println!("Starting nclav API server on http://{addr} (in-memory store)");
            let store = Arc::new(InMemoryStore::new());
            let driver: Arc<dyn Driver> = Arc::new(LocalDriver::new());
            let app = nclav_api::build_app(store, driver);

            let listener = tokio::net::TcpListener::bind(&addr)
                .await
                .with_context(|| format!("Failed to bind to port {port}"))?;

            println!("Listening on http://{addr}");
            axum::serve(listener, app).await.context("Server error")?;
        }
        CloudArg::Gcp => {
            let parent = gcp_parent
                .context("--gcp-parent (or NCLAV_GCP_PARENT) is required for --cloud gcp")?;
            let billing_account = gcp_billing_account.context(
                "--gcp-billing-account (or NCLAV_GCP_BILLING_ACCOUNT) is required for --cloud gcp",
            )?;

            let config = GcpDriverConfig {
                parent,
                billing_account,
                default_region: gcp_default_region,
                project_prefix: gcp_project_prefix,
            };

            println!("Initialising GCP driver (ADC)…");
            let driver: Arc<dyn Driver> = Arc::new(
                GcpDriver::from_adc(config)
                    .await
                    .context("Failed to initialise GCP driver")?,
            );

            let store = Arc::new(InMemoryStore::new());
            let app = nclav_api::build_app(store, driver);

            let addr = format!("0.0.0.0:{port}");
            println!("Starting nclav API server on http://{addr} (GCP driver)");
            let listener = tokio::net::TcpListener::bind(&addr)
                .await
                .with_context(|| format!("Failed to bind to port {port}"))?;
            println!("Listening on http://{addr}");
            axum::serve(listener, app).await.context("Server error")?;
        }
    }

    Ok(())
}

// ── Apply ─────────────────────────────────────────────────────────────────────

pub async fn apply(enclaves_dir: PathBuf, remote: Option<String>) -> Result<()> {
    api_reconcile(&server_url(remote), &enclaves_dir, false).await
}

// ── Diff ──────────────────────────────────────────────────────────────────────

pub async fn diff(enclaves_dir: PathBuf, remote: Option<String>) -> Result<()> {
    api_reconcile(&server_url(remote), &enclaves_dir, true).await
}

// ── Status ────────────────────────────────────────────────────────────────────

pub async fn status(remote: Option<String>) -> Result<()> {
    let url = server_url(remote);
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{}/status", url.trim_end_matches('/')))
        .send()
        .await
        .with_context(|| format!("Failed to reach server at {url}"))?;
    let body: serde_json::Value = resp.json().await?;
    println!("{}", serde_json::to_string_pretty(&body)?);
    Ok(())
}

// ── Graph ─────────────────────────────────────────────────────────────────────

pub async fn graph(
    output_format: GraphOutput,
    filter_enclave: Option<String>,
    remote: Option<String>,
) -> Result<()> {
    let url = server_url(remote);
    let client = reqwest::Client::new();
    let filter = filter_enclave.as_deref();

    match output_format {
        GraphOutput::Json => {
            let path = if let Some(enc) = filter {
                format!("{}/enclaves/{}/graph", url.trim_end_matches('/'), enc)
            } else {
                format!("{}/graph", url.trim_end_matches('/'))
            };
            let body: serde_json::Value = client
                .get(&path)
                .send()
                .await
                .with_context(|| format!("Failed to reach server at {url}"))?
                .json()
                .await?;
            println!("{}", serde_json::to_string_pretty(&body)?);
        }
        GraphOutput::Text | GraphOutput::Dot => {
            let states: Vec<EnclaveState> = client
                .get(format!("{}/enclaves", url.trim_end_matches('/')))
                .send()
                .await
                .with_context(|| format!("Failed to reach server at {url}"))?
                .json()
                .await
                .context("Failed to deserialize enclave states")?;
            match output_format {
                GraphOutput::Text => print!("{}", output::render_graph_text_live(&states, filter)),
                GraphOutput::Dot => println!("{}", output::render_dot_live(&states, filter)),
                GraphOutput::Json => unreachable!(),
            }
        }
    }

    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Resolve the server URL: explicit --remote / NCLAV_URL, or the local default.
fn server_url(remote: Option<String>) -> String {
    remote.unwrap_or_else(|| "http://localhost:8080".into())
}

async fn api_reconcile(url: &str, enclaves_dir: &PathBuf, dry_run: bool) -> Result<()> {
    let endpoint = if dry_run {
        format!("{}/reconcile/dry-run", url.trim_end_matches('/'))
    } else {
        format!("{}/reconcile", url.trim_end_matches('/'))
    };

    let client = reqwest::Client::new();
    let body = serde_json::json!({
        "enclaves_dir": enclaves_dir.display().to_string(),
    });

    let resp = client
        .post(&endpoint)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("Failed to reach server at {url}"))?;

    let report: serde_json::Value = resp.json().await?;

    if let Some(changes) = report.get("changes").and_then(|c| c.as_array()) {
        for c in changes {
            println!("{}", c);
        }
    }

    let n_changes = report
        .get("changes")
        .and_then(|c| c.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    println!(
        "{} change(s){}.",
        n_changes,
        if dry_run { " (dry run)" } else { " applied" }
    );

    if let Some(errors) = report.get("errors").and_then(|e| e.as_array()) {
        if !errors.is_empty() {
            eprintln!("\n{} error(s):", errors.len());
            for e in errors {
                eprintln!("  ! {}", e);
            }
        }
    }

    Ok(())
}
