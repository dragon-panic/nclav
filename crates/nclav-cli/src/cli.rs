use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "nclav",
    about = "Cloud infrastructure orchestration via YAML-driven enclave reconciliation",
    version
)]
pub struct Cli {
    /// nclav server URL (default: http://localhost:8080).
    /// All non-bootstrap commands talk to this server. Env: NCLAV_URL
    #[arg(long, env = "NCLAV_URL", global = true)]
    pub remote: Option<String>,

    /// API bearer token. Falls back to reading ~/.nclav/token. Env: NCLAV_TOKEN
    #[arg(long, env = "NCLAV_TOKEN", global = true)]
    pub token: Option<String>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Initialize and start the nclav server.
    Bootstrap {
        /// Default cloud for enclaves that omit `cloud:` in their YAML.
        /// The driver for this cloud is automatically registered.
        #[arg(long, default_value = "local")]
        cloud: CloudArg,

        /// Register an additional cloud driver without changing the default.
        /// Repeat to enable multiple clouds:
        ///   --cloud local --enable-cloud gcp --gcp-parent folders/123 ...
        /// Each enabled cloud must have its required flags present.
        #[arg(long = "enable-cloud", value_name = "CLOUD")]
        enable_cloud: Vec<CloudArg>,

        /// Use an in-memory (ephemeral) store instead of persisting to disk.
        /// State is lost when the server stops.
        #[arg(long)]
        ephemeral: bool,

        /// Force generation of a new API token, replacing any existing one.
        /// By default the existing token is reused so restarts don't invalidate clients.
        #[arg(long)]
        rotate_token: bool,

        /// Path to the redb state file. Defaults to ~/.nclav/state.redb.
        /// Ignored when --ephemeral is set. Env: NCLAV_STORE_PATH
        #[arg(long, env = "NCLAV_STORE_PATH")]
        store_path: Option<String>,

        /// GCP parent resource ("folders/123" or "organizations/456").
        /// Required when gcp is the default (--cloud gcp) or an additional
        /// driver (--enable-cloud gcp). Env: NCLAV_GCP_PARENT
        #[arg(long, env = "NCLAV_GCP_PARENT")]
        gcp_parent: Option<String>,

        /// GCP billing account ("billingAccounts/XXXX-YYYY-ZZZZ").
        /// Required when gcp is the default or an additional driver.
        /// Env: NCLAV_GCP_BILLING_ACCOUNT
        #[arg(long, env = "NCLAV_GCP_BILLING_ACCOUNT")]
        gcp_billing_account: Option<String>,

        /// Default GCP region. Env: NCLAV_GCP_DEFAULT_REGION
        #[arg(long, env = "NCLAV_GCP_DEFAULT_REGION", default_value = "us-central1")]
        gcp_default_region: String,

        /// Prefix prepended to every GCP project ID (e.g. "acme" → "acme-product-a-dev").
        /// Avoids global project ID collisions without changing enclave YAML IDs.
        /// Env: NCLAV_GCP_PROJECT_PREFIX
        #[arg(long, env = "NCLAV_GCP_PROJECT_PREFIX")]
        gcp_project_prefix: Option<String>,

        /// TCP port to bind the HTTP API server on. Env: NCLAV_PORT
        #[arg(long, env = "NCLAV_PORT", default_value = "8080")]
        port: u16,

        /// Address to bind the HTTP API server on. Defaults to 127.0.0.1 (loopback only).
        /// Use 0.0.0.0 to expose on all interfaces. Env: NCLAV_BIND
        #[arg(long, env = "NCLAV_BIND", default_value = "127.0.0.1")]
        bind: String,
    },

    /// Reconcile and apply all changes.
    Apply {
        /// Path to the enclaves directory.
        enclaves_dir: PathBuf,
    },

    /// Show what would change without applying.
    Diff {
        /// Path to the enclaves directory.
        enclaves_dir: PathBuf,
    },

    /// Show enclave health summary.
    Status,

    /// Render the dependency graph from the running server.
    Graph {
        /// Output format.
        #[arg(long, default_value = "text")]
        output: GraphOutput,

        /// Filter to a specific enclave.
        #[arg(long)]
        enclave: Option<String>,
    },

    /// Inspect IaC (Terraform/OpenTofu) run logs for a partition.
    Iac {
        #[command(subcommand)]
        command: IacCommand,
    },

    /// Scan GCP enclave projects for resources belonging to destroyed or unknown partitions.
    ///
    /// Queries Cloud Asset Inventory for resources labeled `nclav-managed=true` whose
    /// `nclav-partition` label does not match any active partition in nclav state.
    /// Exits 0 if no orphans found; exits 1 if any are reported (CI-friendly).
    Orphans {
        /// Filter to a specific enclave.
        #[arg(long)]
        enclave: Option<String>,
    },

    /// Destroy one or more enclaves, tearing down all their infrastructure.
    ///
    /// Runs terraform destroy for IaC partitions, then tears down the enclave
    /// itself. State is removed from the server. Use --all to nuke everything
    /// (handy for resetting a test environment).
    ///
    /// Use --partition to destroy a single partition within an enclave instead
    /// of the whole enclave (e.g. to clean up and recreate a bad Cloud SQL
    /// instance without deleting the GCP project).
    Destroy {
        /// Enclave IDs to destroy. Required unless --all is given.
        #[arg(required_unless_present = "all")]
        enclave_ids: Vec<String>,

        /// Destroy every enclave known to the server. Skips confirmation prompts.
        #[arg(long)]
        all: bool,

        /// Destroy a single partition within the enclave rather than the whole
        /// enclave. Requires exactly one enclave ID. Cannot be combined with --all.
        #[arg(long, conflicts_with = "all")]
        partition: Option<String>,

        /// Skip the confirmation prompt. Useful for automation and scripts.
        #[arg(long, short = 'y')]
        yes: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum IacCommand {
    /// List IaC runs for a partition (newest first).
    Runs {
        /// Enclave ID.
        enclave_id: String,
        /// Partition ID.
        partition_id: String,
    },

    /// Print the full log from an IaC run.
    ///
    /// If no run ID is given, prints the most recent run's log.
    Logs {
        /// Enclave ID.
        enclave_id: String,
        /// Partition ID.
        partition_id: String,
        /// Specific run ID (UUID). Omit to use the latest run.
        run_id: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, ValueEnum)]
pub enum CloudArg {
    Local,
    Gcp,
    Azure,
}

#[derive(Debug, Clone, ValueEnum)]
pub enum GraphOutput {
    Text,
    Json,
    Dot,
}
