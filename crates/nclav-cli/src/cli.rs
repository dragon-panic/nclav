use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "nclav",
    about = "Cloud infrastructure orchestration via YAML-driven enclave reconciliation",
    version
)]
pub struct Cli {
    /// Connect to a remote nclav server instead of running in-process.
    #[arg(long, env = "NCLAV_URL", global = true)]
    pub remote: Option<String>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Initialize and start the nclav server (local only for now).
    Bootstrap {
        /// Cloud target to initialise for.
        #[arg(long, default_value = "local")]
        cloud: CloudArg,
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

    /// Render the dependency graph.
    Graph {
        /// Path to the enclaves directory.
        enclaves_dir: PathBuf,

        /// Output format.
        #[arg(long, default_value = "text")]
        output: GraphOutput,

        /// Filter to a specific enclave.
        #[arg(long)]
        enclave: Option<String>,
    },
}

#[derive(Debug, Clone, ValueEnum)]
pub enum CloudArg {
    Local,
    Azure,
}

#[derive(Debug, Clone, ValueEnum)]
pub enum GraphOutput {
    Text,
    Json,
    Dot,
}
