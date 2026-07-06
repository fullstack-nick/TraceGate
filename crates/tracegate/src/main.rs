use std::path::PathBuf;

use anyhow::Context;
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "tracegate", version, about = "Rust observability API gateway")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Serve {
        #[arg(long)]
        config: PathBuf,
    },
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    Check {
        #[arg(long)]
        config: PathBuf,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Serve { config } => {
            let config = tracegate_config::load_config(&config)
                .with_context(|| format!("failed to load {}", config.display()))?;
            let observability = tracegate_observability::init(&config.observability)?;
            tracegate_proxy::serve(config, observability.telemetry()).await?;
            observability.shutdown();
        }
        Command::Config {
            command: ConfigCommand::Check { config },
        } => {
            let config = tracegate_config::load_config(&config)
                .with_context(|| format!("failed to load {}", config.display()))?;
            println!(
                "config ok: listen={}, admin_listen={}, routes={}, prometheus={}",
                config.listen,
                config.admin_listen,
                config.routes.len(),
                config.observability.prometheus_enabled
            );
        }
    }

    Ok(())
}
