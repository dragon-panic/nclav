mod cli;
mod commands;
mod output;

use anyhow::Result;
use cli::{Cli, Command, IacCommand};
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
        Command::Bootstrap {
            cloud,
            enable_cloud,
            ephemeral,
            rotate_token,
            store_path,
            gcp_parent,
            gcp_billing_account,
            gcp_default_region,
            gcp_project_prefix,
            port,
            bind,
        } => {
            commands::bootstrap(
                cloud,
                enable_cloud,
                cli.remote,
                ephemeral,
                rotate_token,
                store_path,
                gcp_parent,
                gcp_billing_account,
                gcp_default_region,
                gcp_project_prefix,
                port,
                bind,
            )
            .await
        }
        Command::Apply { enclaves_dir } => {
            commands::apply(enclaves_dir, cli.remote, cli.token).await
        }
        Command::Diff { enclaves_dir } => {
            commands::diff(enclaves_dir, cli.remote, cli.token).await
        }
        Command::Status => commands::status(cli.remote, cli.token).await,
        Command::Graph { output, enclave } => {
            commands::graph(output, enclave, cli.remote, cli.token).await
        }
        Command::Destroy { enclave_ids, all } => {
            commands::destroy(enclave_ids, all, cli.remote, cli.token).await
        }
        Command::Iac { command } => match command {
            IacCommand::Runs { enclave_id, partition_id } => {
                commands::iac_runs(enclave_id, partition_id, cli.remote, cli.token).await
            }
            IacCommand::Logs { enclave_id, partition_id, run_id } => {
                commands::iac_logs(enclave_id, partition_id, run_id, cli.remote, cli.token).await
            }
        },
    }
}
