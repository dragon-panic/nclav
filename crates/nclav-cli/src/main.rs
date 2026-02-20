mod cli;
mod commands;
mod output;

use anyhow::Result;
use cli::{Cli, Command};
use clap::Parser;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_target(false)
        .init();

    let cli = Cli::parse();

    match cli.command {
        Command::Bootstrap { cloud, gcp_parent, gcp_billing_account, gcp_default_region, port } => {
            commands::bootstrap(cloud, cli.remote, gcp_parent, gcp_billing_account, gcp_default_region, port).await
        }
        Command::Apply { enclaves_dir } => commands::apply(enclaves_dir, cli.remote).await,
        Command::Diff { enclaves_dir } => commands::diff(enclaves_dir, cli.remote).await,
        Command::Status => commands::status(cli.remote).await,
        Command::Graph { output, enclave } => {
            commands::graph(output, enclave, cli.remote).await
        }
    }
}
