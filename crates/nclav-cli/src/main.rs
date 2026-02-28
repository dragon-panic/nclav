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
        Command::Serve {
            cloud,
            enable_cloud,
            ephemeral,
            rotate_token,
            store_path,
            postgres_url,
            gcp_parent,
            gcp_billing_account,
            gcp_default_region,
            gcp_project_prefix,
            azure_tenant_id,
            azure_management_group_id,
            azure_billing_account_name,
            azure_billing_profile_name,
            azure_invoice_section_name,
            azure_default_location,
            azure_subscription_prefix,
            azure_client_id,
            azure_client_secret,
            aws_org_unit_id,
            aws_email_domain,
            aws_default_region,
            aws_account_prefix,
            aws_cross_account_role,
            aws_role_arn,
            port,
            bind,
        } => {
            commands::serve(
                cloud,
                enable_cloud,
                cli.remote,
                ephemeral,
                rotate_token,
                store_path,
                postgres_url,
                gcp_parent,
                gcp_billing_account,
                gcp_default_region,
                gcp_project_prefix,
                azure_tenant_id,
                azure_management_group_id,
                azure_billing_account_name,
                azure_billing_profile_name,
                azure_invoice_section_name,
                azure_default_location,
                azure_subscription_prefix,
                azure_client_id,
                azure_client_secret,
                aws_org_unit_id,
                aws_email_domain,
                aws_default_region,
                aws_account_prefix,
                aws_cross_account_role,
                aws_role_arn,
                port,
                bind,
            )
            .await
        }
        Command::Apply { enclaves_dir, resources_only } => {
            commands::apply(enclaves_dir, resources_only, cli.remote, cli.token).await
        }
        Command::Diff { enclaves_dir } => {
            commands::diff(enclaves_dir, cli.remote, cli.token).await
        }
        Command::Status => commands::status(cli.remote, cli.token).await,
        Command::Graph { output, enclave } => {
            commands::graph(output, enclave, cli.remote, cli.token).await
        }
        Command::Orphans { enclave } => {
            commands::orphans(enclave, cli.remote, cli.token).await
        }
        Command::Destroy { enclave_ids, all, partition, yes, resources_only } => {
            commands::destroy(enclave_ids, all, partition, yes, resources_only, cli.remote, cli.token).await
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
