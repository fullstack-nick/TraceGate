use std::path::PathBuf;

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use tracegate_replay::{ReplayOptions, ReplayOutcome, ReplaySelector};
use tracegate_storage::{ListFilters, RequestDetails, RequestSummary, Storage, now_ms};

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
    Requests {
        #[command(subcommand)]
        command: RequestsCommand,
    },
    Replay {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        id: Option<String>,
        #[arg(long)]
        last_failed: bool,
        #[arg(long)]
        target: String,
        #[arg(long)]
        confirm_side_effects: bool,
        #[arg(long)]
        json: bool,
    },
    Storage {
        #[command(subcommand)]
        command: StorageCommand,
    },
    Plugins {
        #[command(subcommand)]
        command: PluginsCommand,
    },
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    Check {
        #[arg(long)]
        config: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
enum RequestsCommand {
    List {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        failed: bool,
        #[arg(long)]
        slow: bool,
        #[arg(long)]
        route: Option<String>,
        #[arg(long)]
        since: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: u32,
        #[arg(long)]
        json: bool,
    },
    Show {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        id: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum StorageCommand {
    Migrate {
        #[arg(long)]
        config: PathBuf,
    },
    Prune {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        json: bool,
    },
    Backup {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        output: Option<PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
enum PluginsCommand {
    Inspect {
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Serve { config } => {
            let config_path = config;
            let config = tracegate_config::load_config(&config_path)
                .with_context(|| format!("failed to load {}", config_path.display()))?;
            let observability = tracegate_observability::init(&config.observability)?;
            tracegate_proxy::serve_with_config_path(config_path, config, observability.telemetry())
                .await?;
            observability.shutdown();
        }
        Command::Config {
            command: ConfigCommand::Check { config },
        } => {
            let config = tracegate_config::load_config(&config)
                .with_context(|| format!("failed to load {}", config.display()))?;
            println!(
                "config ok: mode={}, listen={}, admin_listen={}, admin_auth={}, storage_driver={}, storage={}, retention_days={}, routes={}, prometheus={}",
                config.mode,
                config.listen,
                config.admin_listen,
                config.admin.token.is_some(),
                config.storage.driver,
                config.storage.url,
                config.storage.retention_days,
                config.routes.len(),
                config.observability.prometheus_enabled
            );
        }
        Command::Requests { command } => match command {
            RequestsCommand::List {
                config,
                failed,
                slow,
                route,
                since,
                limit,
                json,
            } => {
                let storage = open_storage(&config).await?;
                let since_created_at_ms = match since {
                    Some(value) => Some(now_ms().saturating_sub(parse_duration_ms(&value)?)),
                    None => None,
                };
                let rows = storage
                    .list_requests(ListFilters {
                        failed,
                        slow,
                        route_id: route,
                        since_created_at_ms,
                        limit,
                    })
                    .await?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&rows)?);
                } else {
                    print_request_list(&rows);
                }
            }
            RequestsCommand::Show { config, id, json } => {
                let storage = open_storage(&config).await?;
                let Some(details) = storage.show_request(&id).await? else {
                    bail!("request `{id}` not found");
                };
                if json {
                    println!("{}", serde_json::to_string_pretty(&details)?);
                } else {
                    print_request_details(&details);
                }
            }
        },
        Command::Replay {
            config,
            id,
            last_failed,
            target,
            confirm_side_effects,
            json,
        } => {
            let selector = replay_selector(id, last_failed)?;
            let storage = open_storage(&config).await?;
            let outcome = tracegate_replay::replay(
                &storage,
                ReplayOptions {
                    selector,
                    target,
                    confirm_side_effects,
                },
            )
            .await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&outcome)?);
            } else {
                print_replay_outcome(&outcome);
            }
        }
        Command::Storage { command } => match command {
            StorageCommand::Migrate { config } => {
                let storage = open_storage(&config).await?;
                storage.migrate().await?;
                println!("storage migrated");
            }
            StorageCommand::Prune { config, json } => {
                let storage = open_storage(&config).await?;
                let outcome = storage.run_retention().await?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&outcome)?);
                } else {
                    println!(
                        "storage pruned: deleted_requests={}, evicted_captures={}",
                        outcome.deleted_requests, outcome.evicted_captures
                    );
                }
            }
            StorageCommand::Backup { config, output } => {
                let storage = open_storage(&config).await?;
                let output = output
                    .unwrap_or_else(|| PathBuf::from(format!("tracegate-backup-{}.db", now_ms())));
                storage.backup_to(&output).await?;
                println!("storage backup written: {}", output.display());
            }
        },
        Command::Plugins { command } => match command {
            PluginsCommand::Inspect { path, json } => {
                let inspection = tracegate_wasm::PolicyEngine::inspect(&path)
                    .with_context(|| format!("failed to inspect {}", path.display()))?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&inspection)?);
                } else {
                    println!("path: {}", inspection.path);
                    println!("compatible: {}", inspection.compatible);
                    println!("contract: {}", inspection.contract);
                    println!("imports:");
                    if inspection.imports.is_empty() {
                        println!("  none");
                    } else {
                        for import in &inspection.imports {
                            println!("  {import}");
                        }
                    }
                    println!("exports:");
                    if inspection.exports.is_empty() {
                        println!("  none");
                    } else {
                        for export in &inspection.exports {
                            println!("  {export}");
                        }
                    }
                }
            }
        },
    }

    Ok(())
}

fn replay_selector(id: Option<String>, last_failed: bool) -> anyhow::Result<ReplaySelector> {
    match (id, last_failed) {
        (Some(id), false) => Ok(ReplaySelector::Id(id)),
        (None, true) => Ok(ReplaySelector::LastFailed),
        (Some(_), true) => bail!("use either --id or --last-failed, not both"),
        (None, false) => bail!("one of --id or --last-failed is required"),
    }
}

async fn open_storage(config: &PathBuf) -> anyhow::Result<Storage> {
    let config = tracegate_config::load_config(config)
        .with_context(|| format!("failed to load {}", config.display()))?;
    let storage = Storage::connect(&config.storage).await?;
    storage.migrate().await?;
    Ok(storage)
}

fn parse_duration_ms(value: &str) -> anyhow::Result<i64> {
    let value = value.trim();
    if value.is_empty() {
        bail!("duration cannot be empty");
    }

    let (number, multiplier) = match value.chars().last().unwrap() {
        's' | 'S' => (&value[..value.len() - 1], 1_000_i64),
        'm' | 'M' => (&value[..value.len() - 1], 60_000_i64),
        'h' | 'H' => (&value[..value.len() - 1], 60 * 60 * 1_000_i64),
        'd' | 'D' => (&value[..value.len() - 1], 24 * 60 * 60 * 1_000_i64),
        _ => (value, 1_000_i64),
    };

    let number = number
        .parse::<i64>()
        .with_context(|| format!("invalid duration `{value}`"))?;
    if number <= 0 {
        bail!("duration must be greater than zero");
    }

    number
        .checked_mul(multiplier)
        .context("duration is too large")
}

fn print_request_list(rows: &[RequestSummary]) {
    println!("created_at_ms status route method flags request_id path");
    for row in rows {
        let flags = match (row.is_error, row.is_slow) {
            (true, true) => "error,slow",
            (true, false) => "error",
            (false, true) => "slow",
            (false, false) => "-",
        };
        println!(
            "{} {} {} {} {} {} {}",
            row.created_at_ms,
            row.status,
            row.route_id.as_deref().unwrap_or("none"),
            row.method,
            flags,
            row.request_id,
            display_path(row)
        );
    }
}

fn print_request_details(details: &RequestDetails) {
    let request = &details.request;
    println!("request_id: {}", request.request_id);
    println!("created_at_ms: {}", request.created_at_ms);
    println!(
        "route_id: {}",
        request.route_id.as_deref().unwrap_or("none")
    );
    println!("method: {}", request.method);
    println!("path: {}", display_path(request));
    println!("status: {}", request.status);
    println!("latency_ms: {}", request.latency_ms);
    println!(
        "upstream: {}",
        request.upstream.as_deref().unwrap_or("none")
    );
    println!("is_error: {}", request.is_error);
    println!("is_slow: {}", request.is_slow);
    println!("capture_policy: {}", request.capture_policy);
    println!(
        "query_hash: {}",
        request.query_hash.as_deref().unwrap_or("none")
    );

    println!("request_headers:");
    for header in &details.request_headers {
        println!("  {}: {}", header.name, header.value);
    }

    println!("response_headers:");
    for header in &details.response_headers {
        println!("  {}: {}", header.name, header.value);
    }

    println!("plugin_decisions:");
    if details.plugin_decisions.is_empty() {
        println!("  none");
    } else {
        for decision in &details.plugin_decisions {
            println!("  plugin_id: {}", decision.plugin_id);
            println!("    route_id: {}", decision.route_id);
            println!("    action: {}", decision.action);
            println!(
                "    deny_status: {}",
                decision
                    .deny_status
                    .map(|status| status.to_string())
                    .unwrap_or_else(|| "none".to_owned())
            );
            println!("    set_headers: {}", display_list(&decision.set_headers));
            println!(
                "    remove_headers: {}",
                display_list(&decision.remove_headers)
            );
            println!("    timed_out: {}", decision.timed_out);
            println!("    error: {}", decision.error.as_deref().unwrap_or("none"));
            println!("    duration_ms: {}", decision.duration_ms);
            println!(
                "    events: {}",
                decision
                    .events
                    .iter()
                    .map(|event| match event.code.as_deref() {
                        Some(code) => format!("{}:{code}", event.name),
                        None => event.name.clone(),
                    })
                    .collect::<Vec<_>>()
                    .join(",")
            );
        }
    }

    if let Some(capture) = &details.capture {
        println!("capture:");
        println!(
            "  request_content_type: {}",
            capture.request_content_type.as_deref().unwrap_or("none")
        );
        println!(
            "  response_content_type: {}",
            capture.response_content_type.as_deref().unwrap_or("none")
        );
        println!(
            "  request_body_bytes: {}",
            capture.request_body.as_ref().map(Vec::len).unwrap_or(0)
        );
        println!(
            "  response_body_bytes: {}",
            capture.response_body.as_ref().map(Vec::len).unwrap_or(0)
        );
        println!(
            "  request_body_truncated: {}",
            capture.request_body_truncated
        );
        println!(
            "  response_body_truncated: {}",
            capture.response_body_truncated
        );
        println!("  body_evicted: {}", capture.body_evicted);
        println!(
            "  request_body_sha256: {}",
            capture.request_body_sha256.as_deref().unwrap_or("none")
        );
        println!(
            "  response_body_sha256: {}",
            capture.response_body_sha256.as_deref().unwrap_or("none")
        );
    } else {
        println!("capture: none");
    }

    println!("replay_runs:");
    if details.replay_runs.is_empty() {
        println!("  none");
    } else {
        for run in &details.replay_runs {
            println!("  replay_id: {}", run.replay_id);
            println!("    created_at_ms: {}", run.created_at_ms);
            println!("    replay_request_id: {}", run.replay_request_id);
            println!("    target: {}", run.target);
            println!("    method: {}", run.method);
            println!("    path: {}", run.path);
            println!(
                "    status: {}",
                run.status
                    .map(|status| status.to_string())
                    .unwrap_or_else(|| "none".to_owned())
            );
            println!("    latency_ms: {}", run.latency_ms);
            println!("    error: {}", run.error.as_deref().unwrap_or("none"));
            println!(
                "    diff_summary: {}",
                run.diff_summary.as_deref().unwrap_or("none")
            );
        }
    }
}

fn print_replay_outcome(outcome: &ReplayOutcome) {
    println!("replay_id: {}", outcome.replay_id);
    println!("original_request_id: {}", outcome.original_request_id);
    println!("replay_request_id: {}", outcome.replay_request_id);
    println!("target: {}", outcome.target);
    println!("method: {}", outcome.method);
    println!("path: {}", outcome.path);
    println!("status: {}", outcome.status);
    println!("latency_ms: {}", outcome.latency_ms);
    println!("response_body_bytes: {}", outcome.response_body_bytes);
    println!("diff_summary: {}", outcome.diff_summary);
}

fn display_path(row: &RequestSummary) -> String {
    match row.redacted_query.as_deref() {
        Some(query) => format!("{}?{query}", row.path),
        None => row.path.clone(),
    }
}

fn display_list(values: &[String]) -> String {
    if values.is_empty() {
        "none".to_owned()
    } else {
        values.join(",")
    }
}
