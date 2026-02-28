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
    /// All non-serve commands talk to this server. Env: NCLAV_URL
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
    /// Start the nclav API server.
    Serve {
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
        /// Ignored when --ephemeral or --postgres-url is set. Env: NCLAV_STORE_PATH
        #[arg(long, env = "NCLAV_STORE_PATH")]
        store_path: Option<String>,

        /// PostgreSQL connection URL for persistent state.
        /// When set, takes precedence over --store-path and --ephemeral.
        /// Example: postgres://user:pass@localhost:5432/nclav
        /// Cloud SQL socket: postgres://user:pass@/db?host=/cloudsql/proj:region:inst
        /// Env: NCLAV_POSTGRES_URL
        #[arg(long, env = "NCLAV_POSTGRES_URL")]
        postgres_url: Option<String>,

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

        // ── Azure flags ───────────────────────────────────────────────────────

        /// Azure tenant ID (GUID). Required when azure is the default or an additional driver.
        /// Env: NCLAV_AZURE_TENANT_ID
        #[arg(long, env = "NCLAV_AZURE_TENANT_ID")]
        azure_tenant_id: Option<String>,

        /// Azure management group ID where new subscription enclaves will be placed.
        /// Required when azure is the default or an additional driver.
        /// Env: NCLAV_AZURE_MANAGEMENT_GROUP_ID
        #[arg(long, env = "NCLAV_AZURE_MANAGEMENT_GROUP_ID")]
        azure_management_group_id: Option<String>,

        /// MCA billing account name (long GUID form).
        /// Required when azure is the default or an additional driver.
        /// Env: NCLAV_AZURE_BILLING_ACCOUNT_NAME
        #[arg(long, env = "NCLAV_AZURE_BILLING_ACCOUNT_NAME")]
        azure_billing_account_name: Option<String>,

        /// MCA billing profile name.
        /// Required when azure is the default or an additional driver.
        /// Env: NCLAV_AZURE_BILLING_PROFILE_NAME
        #[arg(long, env = "NCLAV_AZURE_BILLING_PROFILE_NAME")]
        azure_billing_profile_name: Option<String>,

        /// MCA invoice section name.
        /// Required when azure is the default or an additional driver.
        /// Env: NCLAV_AZURE_INVOICE_SECTION_NAME
        #[arg(long, env = "NCLAV_AZURE_INVOICE_SECTION_NAME")]
        azure_invoice_section_name: Option<String>,

        /// Default Azure region for new resources (e.g. "eastus2").
        /// Env: NCLAV_AZURE_DEFAULT_LOCATION
        #[arg(long, env = "NCLAV_AZURE_DEFAULT_LOCATION", default_value = "eastus2")]
        azure_default_location: String,

        /// Optional prefix prepended to every subscription alias.
        /// Env: NCLAV_AZURE_SUBSCRIPTION_PREFIX
        #[arg(long, env = "NCLAV_AZURE_SUBSCRIPTION_PREFIX")]
        azure_subscription_prefix: Option<String>,

        /// Azure service principal client ID (optional; falls back to managed identity / Azure CLI).
        /// Env: NCLAV_AZURE_CLIENT_ID
        #[arg(long, env = "NCLAV_AZURE_CLIENT_ID")]
        azure_client_id: Option<String>,

        /// Azure service principal client secret (optional; falls back to managed identity / Azure CLI).
        /// Env: NCLAV_AZURE_CLIENT_SECRET
        #[arg(long, env = "NCLAV_AZURE_CLIENT_SECRET")]
        azure_client_secret: Option<String>,

        // ── AWS flags ─────────────────────────────────────────────────────────

        /// AWS Organizations OU ID where new accounts are placed (e.g. "ou-xxxx-yyyyyyyy").
        /// Required when aws is the default or an additional driver.
        /// Env: NCLAV_AWS_ORG_UNIT_ID
        #[arg(long, env = "NCLAV_AWS_ORG_UNIT_ID")]
        aws_org_unit_id: Option<String>,

        /// Email domain for new account registration (e.g. "myorg.com").
        /// New accounts get address: aws+{name}@{domain}.
        /// Required when aws is the default or an additional driver.
        /// Env: NCLAV_AWS_EMAIL_DOMAIN
        #[arg(long, env = "NCLAV_AWS_EMAIL_DOMAIN")]
        aws_email_domain: Option<String>,

        /// Default AWS region for new resources. Env: NCLAV_AWS_DEFAULT_REGION
        #[arg(long, env = "NCLAV_AWS_DEFAULT_REGION", default_value = "us-east-1")]
        aws_default_region: String,

        /// Optional prefix prepended to every AWS account name.
        /// Env: NCLAV_AWS_ACCOUNT_PREFIX
        #[arg(long, env = "NCLAV_AWS_ACCOUNT_PREFIX")]
        aws_account_prefix: Option<String>,

        /// IAM role name to assume in each enclave account.
        /// Env: NCLAV_AWS_CROSS_ACCOUNT_ROLE
        #[arg(
            long, env = "NCLAV_AWS_CROSS_ACCOUNT_ROLE",
            default_value = "OrganizationAccountAccessRole"
        )]
        aws_cross_account_role: String,

        /// ARN of an IAM role to assume for management API calls (optional).
        /// Env: NCLAV_AWS_ROLE_ARN
        #[arg(long, env = "NCLAV_AWS_ROLE_ARN")]
        aws_role_arn: Option<String>,

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

        /// Tear down resources inside cloud projects but do not delete the projects
        /// themselves. Useful for stopping costs without losing project config.
        #[arg(long)]
        resources_only: bool,
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

        /// Tear down resources inside cloud projects but do not delete the projects
        /// themselves. Useful for stopping costs without losing project config.
        #[arg(long)]
        resources_only: bool,
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
    Aws,
}

#[derive(Debug, Clone, ValueEnum)]
pub enum GraphOutput {
    Text,
    Json,
    Dot,
}
