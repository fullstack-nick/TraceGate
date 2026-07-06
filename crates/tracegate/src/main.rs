use std::path::PathBuf;

use anyhow::Context;
use clap::{Parser, Subcommand};
use tracing_subscriber::{EnvFilter, fmt};

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
            init_logging(config.json_logs);
            tracegate_proxy::serve(config).await?;
        }
        Command::Config {
            command: ConfigCommand::Check { config },
        } => {
            let config = tracegate_config::load_config(&config)
                .with_context(|| format!("failed to load {}", config.display()))?;
            println!(
                "config ok: listen={}, routes={}",
                config.listen,
                config.routes.len()
            );
        }
    }

    Ok(())
}

fn init_logging(json: bool) {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    if json {
        fmt()
            .with_env_filter(env_filter)
            .json()
            .flatten_event(true)
            .init();
    } else {
        fmt().with_env_filter(env_filter).init();
    }
}
