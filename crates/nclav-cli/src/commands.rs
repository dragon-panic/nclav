use std::io::{self, BufRead, Write as IoWrite};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use nclav_domain::CloudTarget;
use nclav_driver::{DriverRegistry, GcpDriver, GcpDriverConfig, LocalDriver};
use nclav_store::{EnclaveState, InMemoryStore, RedbStore, StateStore};
use uuid::Uuid;

use crate::cli::{CloudArg, GraphOutput};
use crate::output;

// ── Bootstrap ─────────────────────────────────────────────────────────────────

pub async fn bootstrap(
    cloud: CloudArg,
    enable_cloud: Vec<CloudArg>,
    remote: Option<String>,
    ephemeral: bool,
    rotate_token: bool,
    store_path: Option<String>,
    mut gcp_parent: Option<String>,
    mut gcp_billing_account: Option<String>,
    gcp_default_region: String,
    gcp_project_prefix: Option<String>,
    port: u16,
    bind: String,
) -> Result<()> {
    if remote.is_some() {
        anyhow::bail!("bootstrap does not support --remote; run the server locally");
    }

    // Reuse existing token unless rotation is explicitly requested.
    // This means server restarts don't invalidate client configurations.
    let token_path = default_token_path();
    let token = if !rotate_token {
        if let Ok(existing) = std::fs::read_to_string(&token_path).map(|s| s.trim().to_string()) {
            if !existing.is_empty() {
                println!("Reusing existing token from {}", token_path.display());
                existing
            } else {
                let t = generate_token();
                write_token(&token_path, &t)?;
                println!("Generated new token (written to {})", token_path.display());
                t
            }
        } else {
            let t = generate_token();
            write_token(&token_path, &t)?;
            println!("Generated new token (written to {})", token_path.display());
            t
        }
    } else {
        let t = generate_token();
        write_token(&token_path, &t)?;
        println!("Rotated token (written to {})", token_path.display());
        println!("New token: {}", t);
        t
    };

    let store: Arc<dyn StateStore> = if ephemeral {
        println!("Using in-memory (ephemeral) store — state will be lost on server stop");
        Arc::new(InMemoryStore::new())
    } else {
        let path = resolve_store_path(store_path);
        println!("Using persistent store at {}", path.display());
        Arc::new(
            RedbStore::open(&path)
                .with_context(|| format!("Failed to open store at {}", path.display()))?,
        )
    };

    // Build the ordered, deduplicated list of clouds to register.
    // The default cloud comes first; --enable-cloud entries follow.
    let mut clouds: Vec<CloudArg> = vec![cloud.clone()];
    for c in enable_cloud {
        if !clouds.contains(&c) {
            clouds.push(c);
        }
    }

    let default_target = cloud_arg_to_target(&cloud);
    let mut registry = DriverRegistry::new(default_target.clone());

    for c in clouds {
        match c {
            CloudArg::Local => {
                registry.register(CloudTarget::Local, Arc::new(LocalDriver::new()));
            }
            CloudArg::Gcp => {
                let parent = gcp_parent.take()
                    .context("--gcp-parent (or NCLAV_GCP_PARENT) is required for the gcp driver")?;
                let billing_account = gcp_billing_account.take()
                    .context("--gcp-billing-account (or NCLAV_GCP_BILLING_ACCOUNT) is required for the gcp driver")?;
                let config = GcpDriverConfig {
                    parent,
                    billing_account,
                    default_region: gcp_default_region.clone(),
                    project_prefix: gcp_project_prefix.clone(),
                };

                // Use a SA key file if one was written by `provision_platform`
                // (production/Cloud Run setup), otherwise fall back to ADC.
                // For local development ADC is sufficient: the operator is project
                // owner on all enclave projects so Terraform uses ADC directly
                // without needing impersonation.
                let creds_path = default_gcp_credentials_path();
                let driver = if creds_path.exists() {
                    println!("Using GCP service account credentials from {}", creds_path.display());
                    GcpDriver::from_key_file(config, creds_path)
                        .context("Failed to load GCP SA key")?
                } else {
                    println!("Initialising GCP driver (ADC)…");
                    GcpDriver::from_adc(config)
                        .await
                        .context("Failed to initialise GCP driver")?
                };
                registry.register(CloudTarget::Gcp, Arc::new(driver));
            }
            CloudArg::Azure => {
                anyhow::bail!("Azure driver not yet implemented");
            }
        }
    }

    let active: Vec<String> = registry.active_clouds().iter().map(|c| c.to_string()).collect();
    let registry = Arc::new(registry);

    let addr = format!("{bind}:{port}");
    println!(
        "Starting nclav API server on http://{addr} (default: {default_target}, drivers: {drivers})",
        default_target = default_target,
        drivers = active.join(", "),
    );

    let api_base = format!("http://{addr}");
    let app = nclav_api::build_app(store, registry, Arc::new(token), api_base);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("Failed to bind to {addr}"))?;
    axum::serve(listener, app).await.context("Server error")?;

    Ok(())
}

fn cloud_arg_to_target(arg: &CloudArg) -> CloudTarget {
    match arg {
        CloudArg::Local => CloudTarget::Local,
        CloudArg::Gcp => CloudTarget::Gcp,
        CloudArg::Azure => CloudTarget::Azure,
    }
}

// ── Apply ─────────────────────────────────────────────────────────────────────

pub async fn apply(
    enclaves_dir: PathBuf,
    remote: Option<String>,
    token: Option<String>,
) -> Result<()> {
    let token = resolve_token(token)?;
    api_reconcile(&server_url(remote), &enclaves_dir, false, &token).await
}

// ── Diff ──────────────────────────────────────────────────────────────────────

pub async fn diff(
    enclaves_dir: PathBuf,
    remote: Option<String>,
    token: Option<String>,
) -> Result<()> {
    let token = resolve_token(token)?;
    api_reconcile(&server_url(remote), &enclaves_dir, true, &token).await
}

// ── Status ────────────────────────────────────────────────────────────────────

pub async fn status(remote: Option<String>, token: Option<String>) -> Result<()> {
    let token = resolve_token(token)?;
    let url = server_url(remote);
    let body: serde_json::Value = authed_client(&token)
        .get(format!("{}/status", url.trim_end_matches('/')))
        .send()
        .await
        .with_context(|| format!("Failed to reach server at {url}"))?
        .json()
        .await?;

    if let Some(count) = body.get("enclave_count").and_then(|v| v.as_u64()) {
        println!("Enclaves: {}", count);
    }
    if let Some(cloud) = body.get("default_cloud").and_then(|v| v.as_str()) {
        println!("Default cloud: {}", cloud);
    }
    if let Some(drivers) = body.get("active_drivers").and_then(|v| v.as_array()) {
        let names: Vec<&str> = drivers.iter().filter_map(|d| d.as_str()).collect();
        println!("Active drivers: {}", names.join(", "));
    }
    println!("{}", serde_json::to_string_pretty(&body)?);
    Ok(())
}

// ── Graph ─────────────────────────────────────────────────────────────────────

pub async fn graph(
    output_format: GraphOutput,
    filter_enclave: Option<String>,
    remote: Option<String>,
    token: Option<String>,
) -> Result<()> {
    let token = resolve_token(token)?;
    let url = server_url(remote);
    let client = authed_client(&token);
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

// ── Destroy ───────────────────────────────────────────────────────────────────

/// Prompt the user to type `expected` to confirm a destructive action.
/// Returns Ok if confirmed, Err if they typed something else or hit EOF.
fn confirm_destructive(label: &str, expected: &str) -> Result<()> {
    print!("  Type '{}' to confirm: ", expected);
    io::stdout().flush().context("flush stdout")?;
    let line = io::stdin()
        .lock()
        .lines()
        .next()
        .ok_or_else(|| anyhow::anyhow!("no input"))??;
    if line.trim() != expected {
        anyhow::bail!("aborted: {} not destroyed", label);
    }
    Ok(())
}

pub async fn destroy(
    enclave_ids: Vec<String>,
    all: bool,
    partition: Option<String>,
    yes: bool,
    remote: Option<String>,
    token: Option<String>,
) -> Result<()> {
    let token  = resolve_token(token)?;
    let url    = server_url(remote);
    let client = authed_client(&token);
    let base   = url.trim_end_matches('/');

    // ── Partition destroy ─────────────────────────────────────────────────────
    if let Some(ref part_id) = partition {
        if enclave_ids.len() != 1 {
            anyhow::bail!("--partition requires exactly one enclave ID");
        }
        let enc_id = &enclave_ids[0];

        if !yes {
            println!("This will destroy partition '{}/{}' and remove it from the server state.", enc_id, part_id);
            println!("A subsequent 'apply' will re-provision it from scratch.");
            confirm_destructive(&format!("{}/{}", enc_id, part_id), part_id)?;
        }

        print!("Destroying {}/{}… ", enc_id, part_id);
        let resp = client
            .delete(format!("{}/enclaves/{}/partitions/{}", base, enc_id, part_id))
            .send()
            .await
            .with_context(|| format!("Failed to reach server at {url}"))?;

        let status = resp.status();
        let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);

        if status.is_success() {
            let errors = body["errors"].as_array().cloned().unwrap_or_default();
            if errors.is_empty() {
                println!("done.");
            } else {
                println!("done (with errors):");
                for e in &errors {
                    println!("  ! {}", e.as_str().unwrap_or(&e.to_string()));
                }
                anyhow::bail!("partition destroy completed with errors");
            }
        } else {
            let msg = body["error"].as_str().unwrap_or("unknown error");
            println!("failed: {} — {}", status, msg);
            anyhow::bail!("partition destroy failed");
        }
        return Ok(());
    }

    // ── Enclave destroy ───────────────────────────────────────────────────────
    let ids: Vec<String> = if all {
        let states: Vec<serde_json::Value> = client
            .get(format!("{}/enclaves", base))
            .send()
            .await
            .with_context(|| format!("Failed to reach server at {url}"))?
            .json()
            .await
            .context("Failed to parse enclave list")?;

        let ids: Vec<String> = states
            .iter()
            .filter_map(|s| {
                s.get("desired")
                    .and_then(|d| d.get("id"))
                    .and_then(|v| v.as_str())
                    .map(String::from)
            })
            .collect();

        if ids.is_empty() {
            println!("No enclaves to destroy.");
            return Ok(());
        }
        ids
    } else {
        enclave_ids
    };

    let mut any_error = false;
    for id in &ids {
        if !yes && !all {
            println!("This will destroy enclave '{}' and delete its GCP project (30-day hold).", id);
            if let Err(e) = confirm_destructive(id, id) {
                println!("{}", e);
                any_error = true;
                continue;
            }
        }

        print!("Destroying {}… ", id);
        let resp = client
            .delete(format!("{}/enclaves/{}", base, id))
            .send()
            .await
            .with_context(|| format!("Failed to reach server at {url}"))?;

        let status = resp.status();
        let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);

        if status.is_success() {
            let errors = body.get("errors").and_then(|v| v.as_array()).cloned().unwrap_or_default();
            if errors.is_empty() {
                println!("done.");
            } else {
                println!("done (with errors):");
                for e in &errors {
                    println!("  ! {}", e.as_str().unwrap_or(&e.to_string()));
                }
                any_error = true;
            }
        } else {
            let msg = body.get("error").and_then(|v| v.as_str()).unwrap_or("unknown error");
            println!("failed: {} — {}", status, msg);
            any_error = true;
        }
    }

    if any_error {
        anyhow::bail!("one or more enclave destroys failed");
    }
    Ok(())
}

// ── Iac ───────────────────────────────────────────────────────────────────────

pub async fn iac_runs(
    enclave_id: String,
    partition_id: String,
    remote: Option<String>,
    token: Option<String>,
) -> Result<()> {
    let token = resolve_token(token)?;
    let url = server_url(remote);
    let endpoint = format!(
        "{}/enclaves/{}/partitions/{}/iac/runs",
        url.trim_end_matches('/'),
        enclave_id,
        partition_id,
    );
    let runs: serde_json::Value = authed_client(&token)
        .get(&endpoint)
        .send()
        .await
        .with_context(|| format!("Failed to reach server at {url}"))?
        .json()
        .await
        .context("Failed to parse IaC runs response")?;

    let runs = runs.as_array().cloned().unwrap_or_default();
    if runs.is_empty() {
        println!("No IaC runs found for {}/{}", enclave_id, partition_id);
        return Ok(());
    }

    // Sort newest first by started_at (API returns chronological order)
    let mut runs = runs;
    runs.sort_by(|a, b| {
        b.get("started_at")
            .and_then(|v| v.as_str())
            .cmp(&a.get("started_at").and_then(|v| v.as_str()))
    });

    // Table header
    println!(
        "{:<38} {:<12} {:<12} {:<22} {}",
        "ID", "OPERATION", "STATUS", "STARTED", "EXIT"
    );
    println!("{}", "-".repeat(90));

    for run in &runs {
        let id = run.get("id").and_then(|v| v.as_str()).unwrap_or("-");
        let op = run.get("operation").and_then(|v| v.as_str()).unwrap_or("-");
        let status = run.get("status").and_then(|v| v.as_str()).unwrap_or("-");
        let started = run
            .get("started_at")
            .and_then(|v| v.as_str())
            .unwrap_or("-");
        // Trim to first 19 chars (2024-01-15T10:30:00) for display
        let started_short = if started.len() >= 19 { &started[..19] } else { started };
        let exit = run
            .get("exit_code")
            .map(|v| v.to_string())
            .unwrap_or_else(|| "-".into());

        println!(
            "{:<38} {:<12} {:<12} {:<22} {}",
            id, op, status, started_short, exit
        );
    }

    Ok(())
}

pub async fn iac_logs(
    enclave_id: String,
    partition_id: String,
    run_id: Option<String>,
    remote: Option<String>,
    token: Option<String>,
) -> Result<()> {
    let token = resolve_token(token)?;
    let url = server_url(remote);
    let base = url.trim_end_matches('/');

    let endpoint = match &run_id {
        Some(id) => format!(
            "{}/enclaves/{}/partitions/{}/iac/runs/{}",
            base, enclave_id, partition_id, id
        ),
        None => format!(
            "{}/enclaves/{}/partitions/{}/iac/runs/latest",
            base, enclave_id, partition_id
        ),
    };

    let resp = authed_client(&token)
        .get(&endpoint)
        .send()
        .await
        .with_context(|| format!("Failed to reach server at {url}"))?;

    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        anyhow::bail!("No IaC run found for {}/{}", enclave_id, partition_id);
    }

    let run: serde_json::Value = resp.json().await.context("Failed to parse IaC run")?;

    // Print metadata header
    let id = run.get("id").and_then(|v| v.as_str()).unwrap_or("-");
    let op = run.get("operation").and_then(|v| v.as_str()).unwrap_or("-");
    let status = run.get("status").and_then(|v| v.as_str()).unwrap_or("-");
    let started = run.get("started_at").and_then(|v| v.as_str()).unwrap_or("-");
    let exit = run
        .get("exit_code")
        .map(|v| v.to_string())
        .unwrap_or_else(|| "-".into());

    println!("Run:       {}", id);
    println!("Operation: {}  Status: {}  Exit: {}", op, status, exit);
    println!("Started:   {}", started);
    println!("{}", "─".repeat(60));

    // Print the log
    let log = run.get("log").and_then(|v| v.as_str()).unwrap_or("");
    print!("{}", log);
    if !log.ends_with('\n') {
        println!();
    }

    Ok(())
}

// ── Token helpers ─────────────────────────────────────────────────────────────

/// Generate a cryptographically random token as a 64-character hex string.
fn generate_token() -> String {
    let a = Uuid::new_v4().to_string().replace('-', "");
    let b = Uuid::new_v4().to_string().replace('-', "");
    format!("{}{}", a, b)
}

/// Resolve the token to use for API calls.
///
/// Priority: explicit value (from --token / NCLAV_TOKEN) → ~/.nclav/token file
fn resolve_token(explicit: Option<String>) -> Result<String> {
    if let Some(t) = explicit {
        return Ok(t);
    }
    let path = default_token_path();
    std::fs::read_to_string(&path)
        .map(|s| s.trim().to_string())
        .with_context(|| {
            format!(
                "No token provided and could not read token file at {}. \
                 Use --token, NCLAV_TOKEN, or run `nclav bootstrap` first.",
                path.display()
            )
        })
}

/// Write the token to the token file with owner-only permissions.
fn write_token(path: &PathBuf, token: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory {}", parent.display()))?;
    }
    std::fs::write(path, token)
        .with_context(|| format!("Failed to write token to {}", path.display()))?;

    // Set owner-only read/write permissions (unix only)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("Failed to set permissions on {}", path.display()))?;
    }

    Ok(())
}

/// Default path for the token file.
fn default_token_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".nclav").join("token")
}

/// Default path for the GCP service account key file written by `provision_platform`.
fn default_gcp_credentials_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".nclav").join("gcp-credentials.json")
}

/// Build a reqwest Client with the Authorization header pre-configured.
fn authed_client(token: &str) -> reqwest::Client {
    let mut headers = reqwest::header::HeaderMap::new();
    let bearer = format!("Bearer {}", token);
    headers.insert(
        reqwest::header::AUTHORIZATION,
        reqwest::header::HeaderValue::from_str(&bearer)
            .expect("token contains invalid header characters"),
    );
    reqwest::Client::builder()
        .default_headers(headers)
        .build()
        .expect("failed to build HTTP client")
}

// ── Other helpers ─────────────────────────────────────────────────────────────

fn server_url(remote: Option<String>) -> String {
    remote.unwrap_or_else(|| "http://localhost:8080".into())
}

fn resolve_store_path(store_path: Option<String>) -> PathBuf {
    if let Some(p) = store_path {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".nclav").join("state.redb")
}

async fn api_reconcile(
    url: &str,
    enclaves_dir: &PathBuf,
    dry_run: bool,
    token: &str,
) -> Result<()> {
    let endpoint = if dry_run {
        format!("{}/reconcile/dry-run", url.trim_end_matches('/'))
    } else {
        format!("{}/reconcile", url.trim_end_matches('/'))
    };

    let body = serde_json::json!({
        "enclaves_dir": enclaves_dir.display().to_string(),
    });

    let report: serde_json::Value = authed_client(token)
        .post(&endpoint)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("Failed to reach server at {url}"))?
        .json()
        .await?;

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
