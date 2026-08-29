use anyhow::{Context, Result};
use backup_core::config::load_config;
use backup_core::events::{
    AppResult, CancelFlag, Event, LogLevel, new_event_stream, spawn_aggregator,
};
use backup_core::{BackupOptions, backup};
use clap::{Parser, Subcommand};
use std::io::Write;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "backup-tool", version, about = "Cross-platform app backup tool")]
struct Cli {
    /// Path to config file (default: platform config dir)
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run a backup
    Backup {
        /// Backup specific app id(s)
        #[arg(long, value_name = "APP_ID")]
        app: Vec<String>,
        /// Backup all enabled apps
        #[arg(long)]
        all: bool,
        /// Override backup format (zip|tar.gz|dir)
        #[arg(long)]
        format: Option<String>,
    },
    /// List backup history
    History {
        /// Filter by app id
        app: Option<String>,
    },
    /// List configured apps
    Apps {
        #[command(subcommand)]
        action: AppsAction,
    },
    /// Validate configuration
    Check,
}

#[derive(Subcommand)]
enum AppsAction {
    List,
    Add { id: String, name: String, path: String },
    Remove { id: String },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let (config, warnings) = load_config(cli.config.as_deref())
        .with_context(|| "failed to load config")?;

    for w in &warnings {
        eprintln!("warning: {w}");
    }

    match cli.command {
        Command::Backup { app, all, format } => cmd_backup(config, app, all, format),
        Command::History { app } => cmd_history(&config, app.as_deref()),
        Command::Apps { action } => cmd_apps(&config, action),
        Command::Check => cmd_check(&config),
    }
}

fn cmd_backup(
    config: backup_core::Config,
    app_ids: Vec<String>,
    all: bool,
    format: Option<String>,
) -> Result<()> {
    let mut config = config;
    if let Some(fmt) = format {
        config.backup.format = match fmt.as_str() {
            "zip" => backup_core::Format::Zip,
            "tar.gz" => backup_core::Format::TarGz,
            "dir" => backup_core::Format::Dir,
            other => anyhow::bail!("unknown format: {other}"),
        };
    }

    let mut options = BackupOptions {
        app_ids: app_ids.clone(),
    };
    if app_ids.is_empty() && all {
        options.app_ids = vec![];
    }

    let (tx, raw_rx) = new_event_stream();
    let apps_total = if options.app_ids.is_empty() {
        config.apps.iter().filter(|a| a.enabled).count()
    } else {
        options.app_ids.len()
    };
    let rx = spawn_aggregator(raw_rx, apps_total);

    let cancel = CancelFlag::new();
    let started = std::time::Instant::now();
    let report = {
        let tx = tx;
        backup(&config, &options, tx, &cancel)?
    };

    // Drain remaining events for final overall progress.
    for ev in rx.try_iter() {
        print_event(&ev);
    }

    println!(
        "\nDone in {:.1}s. dest: {}",
        started.elapsed().as_secs_f64(),
        report.dest.display()
    );
    for o in &report.outcomes {
        let mark = match o.result {
            AppResult::Ok => "✓",
            AppResult::Skipped => "–",
            AppResult::Failed => "✗",
            AppResult::Cancelled => "×",
        };
        println!("  {mark} {}  {}", o.app_id, o.detail);
    }
    println!(
        "  ok={} failed={} skipped={} cancelled={}",
        report.ok(),
        report.failed(),
        report.skipped(),
        report.cancelled()
    );
    if report.failed() > 0 {
        std::process::exit(1);
    }
    Ok(())
}

fn print_event(ev: &Event) {
    match ev {
        Event::Log { level, msg } => {
            let p = match level {
                LogLevel::Debug => "debug",
                LogLevel::Info => "info",
                LogLevel::Warn => "warn",
                LogLevel::Error => "error",
            };
            eprintln!("[{p}] {msg}");
        }
        Event::AppStarted { app_id } => {
            println!("▶ started {app_id}");
        }
        Event::AppFinished {
            app_id,
            result,
            detail,
            ..
        } => {
            let mark = match result {
                AppResult::Ok => "✓",
                AppResult::Skipped => "–",
                AppResult::Failed => "✗",
                AppResult::Cancelled => "×",
            };
            println!("{mark} {app_id}: {detail}");
        }
        Event::OverallProgress {
            apps_done,
            apps_total,
            bytes_done,
            bytes_total,
            ..
        } => {
            let pct = if *bytes_total == 0 {
                0
            } else {
                ((*bytes_done as f64 / *bytes_total as f64) * 100.0) as u32
            };
            print!(
                "\r⏳ apps {apps_done}/{apps_total}  bytes {bytes_done}/{bytes_total} ({pct}%)   "
            );
            let _ = std::io::stdout().flush();
        }
        _ => {}
    }
}

fn cmd_history(config: &backup_core::Config, app: Option<&str>) -> Result<()> {
    let dest = config.resolved_dest().context("resolve backup dest")?;
    let history = backup_core::history::load_history(&dest);
    let mut entries = history.entries.iter().collect::<Vec<_>>();
    if let Some(app) = app {
        entries.retain(|e| &e.app_id == app);
    }
    for e in entries {
        println!(
            "{}\t{}\t{}\t{}",
            e.app_id, e.file, e.size, e.status
        );
    }
    Ok(())
}

fn cmd_apps(config: &backup_core::Config, action: AppsAction) -> Result<()> {
    match action {
        AppsAction::List => {
            for app in &config.apps {
                let enabled = if app.enabled { "enabled" } else { "disabled" };
                println!("{} ({}) [{}] - {}", app.id, app.name, enabled, app.excludes.join(","));
                for (platform, paths) in &app.paths {
                    for p in paths {
                        println!("    {platform}: {p}");
                    }
                }
            }
        }
        AppsAction::Add { id, name, path } => {
            println!(
                "note: app add only validated syntax; editing config at {} directly is required to persist",
                config.source.as_deref().map(|p| p.display().to_string()).unwrap_or_default()
            );
            let _ = (id, name, path);
        }
        AppsAction::Remove { id } => {
            let _ = id;
            println!("note: removal requires editing the config file directly (not yet wired to save)");
        }
    }
    Ok(())
}

fn cmd_check(config: &backup_core::Config) -> Result<()> {
    let dest = config.resolved_dest().context("resolve backup dest")?;
    println!("config: {}", config.source.as_deref().map(|p| p.display().to_string()).unwrap_or_default());
    println!("dest:   {}", dest.display());
    println!("format: {:?}  parallel: {}  retention: {}", config.backup.format, config.backup.parallel, config.backup.retention);
    println!("apps:   {}", config.apps.len());
    for app in &config.apps {
        let platform = backup_core::Platform::current();
        let paths = config.paths_for(app, platform);
        for raw in paths {
            match backup_core::pathres::expand_path(raw, platform, config.config_dir().as_deref()) {
                backup_core::pathres::PathState::Resolved { path } => {
                    let exists = path.exists();
                    println!("  {} -> {} [{}]", app.id, path.display(), if exists { "ok" } else { "missing" });
                }
                backup_core::pathres::PathState::UndefinedVar { var } => {
                    println!("  {} -> UNDEFINED VAR {var}", app.id);
                }
                backup_core::pathres::PathState::OtherPlatform { .. } => {
                    println!("  {} -> other platform, ignored", app.id);
                }
            }
        }
    }
    Ok(())
}