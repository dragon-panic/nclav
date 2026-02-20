use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use nclav_config::load_enclaves;
use nclav_driver::LocalDriver;
use nclav_graph::validate;
use nclav_driver::Driver;
use nclav_reconciler::{reconcile, ReconcileRequest};
use nclav_store::{EnclaveState, InMemoryStore, StateStore};

use crate::cli::{CloudArg, GraphOutput};
use crate::output;

// ── Bootstrap ─────────────────────────────────────────────────────────────────

pub async fn bootstrap(cloud: CloudArg, remote: Option<String>) -> Result<()> {
    if remote.is_some() {
        anyhow::bail!("bootstrap does not support --remote; run the server locally");
    }

    match cloud {
        CloudArg::Azure => {
            anyhow::bail!("Azure bootstrap not yet implemented");
        }
        CloudArg::Local => {
            println!("Starting nclav API server on http://0.0.0.0:8080 (in-memory store)");
            let store = Arc::new(InMemoryStore::new());
            let driver = Arc::new(LocalDriver::new());
            let app = nclav_api::build_app(store, driver);

            let listener = tokio::net::TcpListener::bind("0.0.0.0:8080")
                .await
                .context("Failed to bind to port 8080")?;

            println!("Listening on http://0.0.0.0:8080");
            axum::serve(listener, app).await.context("Server error")?;
        }
    }

    Ok(())
}

// ── Apply ─────────────────────────────────────────────────────────────────────

pub async fn apply(enclaves_dir: PathBuf, remote: Option<String>) -> Result<()> {
    if let Some(url) = remote {
        remote_reconcile(&url, &enclaves_dir, false).await
    } else {
        let (store, report) = in_process_reconcile_with_store(&enclaves_dir, false).await?;
        print!("{}", output::render_changes(&report.changes));
        println!("Applied {} change(s).", report.changes.len());
        if !report.errors.is_empty() {
            eprintln!("\n{} error(s) during apply:", report.errors.len());
            for e in &report.errors {
                eprintln!("  ! {}", e);
            }
        }
        let states = store.list_enclaves().await.context("Failed to read store")?;
        println!("\nStatus after apply:");
        print!("{}", output::render_status(&states));
        Ok(())
    }
}

// ── Diff ──────────────────────────────────────────────────────────────────────

pub async fn diff(enclaves_dir: PathBuf, remote: Option<String>) -> Result<()> {
    if let Some(url) = remote {
        remote_reconcile(&url, &enclaves_dir, true).await
    } else {
        let report = in_process_reconcile(&enclaves_dir, true).await?;
        print!("{}", output::render_changes(&report.changes));
        if report.changes.is_empty() {
            println!("No changes detected.");
        } else {
            println!("{} change(s) would be applied.", report.changes.len());
        }
        Ok(())
    }
}

// ── Status ────────────────────────────────────────────────────────────────────

pub async fn status(remote: Option<String>) -> Result<()> {
    if let Some(url) = remote {
        let client = reqwest::Client::new();
        let resp = client
            .get(format!("{}/status", url.trim_end_matches('/')))
            .send()
            .await
            .context("Failed to reach remote server")?;
        let body: serde_json::Value = resp.json().await?;
        println!("{}", serde_json::to_string_pretty(&body)?);
    } else {
        println!("No running in-process server. Use --remote or run `nclav bootstrap` first.");
    }
    Ok(())
}

// ── Graph ─────────────────────────────────────────────────────────────────────

pub async fn graph(
    enclaves_dir: PathBuf,
    output_format: GraphOutput,
    filter_enclave: Option<String>,
    remote: Option<String>,
) -> Result<()> {
    if let Some(url) = remote {
        let client = reqwest::Client::new();
        let filter = filter_enclave.as_deref();

        match output_format {
            GraphOutput::Json => {
                // Fetch the system-level graph JSON as-is
                let path = if let Some(enc) = filter {
                    format!("{}/enclaves/{}/graph", url.trim_end_matches('/'), enc)
                } else {
                    format!("{}/graph", url.trim_end_matches('/'))
                };
                let body: serde_json::Value = client
                    .get(&path)
                    .send()
                    .await
                    .context("Failed to reach remote server")?
                    .json()
                    .await?;
                println!("{}", serde_json::to_string_pretty(&body)?);
            }
            GraphOutput::Text | GraphOutput::Dot => {
                // Fetch full enclave states and render with live functions
                let states: Vec<EnclaveState> = client
                    .get(format!("{}/enclaves", url.trim_end_matches('/')))
                    .send()
                    .await
                    .context("Failed to reach remote server")?
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
        return Ok(());
    }

    let enclaves = load_enclaves(&enclaves_dir).context("Failed to load enclaves")?;
    // Validate the graph
    validate(&enclaves).context("Graph validation failed")?;

    let filter = filter_enclave.as_deref();

    match output_format {
        GraphOutput::Text => {
            print!("{}", output::render_graph_text(&enclaves, filter));
        }
        GraphOutput::Dot => {
            println!("{}", output::render_dot(&enclaves, filter));
        }
        GraphOutput::Json => {
            let filtered: Vec<_> = enclaves
                .iter()
                .filter(|e| filter.map_or(true, |f| e.id.as_str() == f))
                .collect();
            println!("{}", serde_json::to_string_pretty(&filtered)?);
        }
    }

    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Run reconcile in-process, returning the live store alongside the report.
async fn in_process_reconcile_with_store(
    enclaves_dir: &PathBuf,
    dry_run: bool,
) -> Result<(Arc<dyn StateStore>, nclav_reconciler::ReconcileReport)> {
    let store: Arc<dyn StateStore> = Arc::new(InMemoryStore::new());
    let driver: Arc<dyn Driver> = Arc::new(LocalDriver::new());
    let req = ReconcileRequest {
        enclaves_dir: enclaves_dir.clone(),
        dry_run,
    };
    let report = reconcile(req, Arc::clone(&store), driver)
        .await
        .context("Reconcile failed")?;
    Ok((store, report))
}

async fn in_process_reconcile(
    enclaves_dir: &PathBuf,
    dry_run: bool,
) -> Result<nclav_reconciler::ReconcileReport> {
    let (_, report) = in_process_reconcile_with_store(enclaves_dir, dry_run).await?;
    Ok(report)
}

async fn remote_reconcile(url: &str, enclaves_dir: &PathBuf, dry_run: bool) -> Result<()> {
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
        .context("Failed to reach remote server")?;

    let report: serde_json::Value = resp.json().await?;

    if let Some(changes) = report.get("changes").and_then(|c| c.as_array()) {
        for c in changes {
            println!("{}", c);
        }
    }

    println!(
        "{} change(s){}.",
        report
            .get("changes")
            .and_then(|c| c.as_array())
            .map(|a| a.len())
            .unwrap_or(0),
        if dry_run { " (dry run)" } else { " applied" }
    );

    Ok(())
}
