use std::path::PathBuf;
use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::ArgValueCompleter;
use tracing_subscriber::EnvFilter;

mod commands;
mod completions;
mod config_io;

#[derive(Parser)]
#[command(name = "stratosync",
          about = "Linux cloud sync — on-demand FUSE filesystem backed by rclone",
          version = env!("CARGO_PKG_VERSION"))]
struct Cli {
    #[arg(long, env = "STRATOSYNC_CONFIG")]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show sync status across all configured mounts
    Status,
    /// List contents of a remote path
    Ls {
        path: Option<PathBuf>,
        #[arg(short, long)]
        all: bool,
    },
    /// Configuration management
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Pin a file/directory for offline availability
    Pin { path: PathBuf },
    /// Remove an offline pin
    Unpin { path: PathBuf },
    /// List and manage conflict files
    Conflicts {
        #[command(subcommand)]
        action: Option<ConflictAction>,
    },
    /// Live dashboard of daemon state via the IPC socket
    Dashboard {
        /// Render a single plain-text snapshot and exit (no TUI)
        #[arg(long)]
        once: bool,
    },
    /// Control the stratosyncd user service (wraps `systemctl --user`)
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
    /// File version history — list and restore prior versions of a file
    Versions {
        #[command(subcommand)]
        action: VersionsAction,
    },
    /// Print shell completion setup instructions
    Completions,
    /// Print version
    Version,
}

#[derive(Subcommand)]
enum ConfigAction {
    Show,
    Test,
    Edit,
}

#[derive(Subcommand)]
enum DaemonAction {
    /// Start the daemon (systemctl --user start stratosyncd)
    Start,
    /// Stop the daemon (systemctl --user stop stratosyncd)
    Stop,
    /// Restart the daemon (systemctl --user restart stratosyncd)
    Restart,
    /// Reload the daemon if supported, otherwise restart
    Reload,
    /// Show systemd unit status (systemctl --user status stratosyncd)
    Status,
    /// Enable the daemon to start on login
    Enable {
        /// Also start the daemon now
        #[arg(long)]
        now: bool,
    },
    /// Disable autostart on login
    Disable {
        /// Also stop the daemon now
        #[arg(long)]
        now: bool,
    },
    /// Show daemon logs (journalctl --user -u stratosyncd)
    Logs {
        /// Follow the journal in real time
        #[arg(short, long)]
        follow: bool,
        /// Number of trailing lines to show (default 200 unless --follow)
        #[arg(short = 'n', long)]
        lines: Option<u32>,
    },
}

#[derive(Subcommand)]
enum VersionsAction {
    /// List recorded versions for a file (newest first)
    List { path: PathBuf },
    /// Restore a recorded version into the cache (marks file Dirty for re-upload)
    Restore {
        path: PathBuf,
        /// Which historical version to restore (0 = most recent)
        #[arg(long)]
        index: usize,
    },
}

#[derive(Subcommand)]
enum ConflictAction {
    /// Upload local version, discard remote conflict file
    KeepLocal {
        #[arg(add = ArgValueCompleter::new(completions::complete_conflict_path))]
        path: PathBuf,
    },
    /// Download remote version, discard local changes
    KeepRemote {
        #[arg(add = ArgValueCompleter::new(completions::complete_conflict_path))]
        path: PathBuf,
    },
    /// Attempt 3-way merge using base version
    Merge {
        #[arg(add = ArgValueCompleter::new(completions::complete_conflict_path))]
        path: PathBuf,
    },
    /// Show unified diff between local and remote versions
    Diff {
        #[arg(add = ArgValueCompleter::new(completions::complete_conflict_path))]
        path: PathBuf,
    },
    /// Remove conflict files whose content is identical to the canonical sibling
    Cleanup {
        /// Report what would be removed without modifying anything
        #[arg(long)]
        dry_run: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    clap_complete::CompleteEnv::with_factory(Cli::command).complete();

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_target(false)
        .init();

    let cli = Cli::parse();
    let config_path = cli.config
        .unwrap_or_else(stratosync_core::config::default_config_path);

    match cli.command {
        Command::Version => {
            println!("stratosync {}", env!("CARGO_PKG_VERSION"));
        }
        Command::Status => {
            commands::status::run(&config_path).await?;
        }
        Command::Ls { path, all } => {
            commands::ls::run(&config_path, path.as_deref(), all).await?;
        }
        Command::Config { action } => match action {
            ConfigAction::Show => commands::config::show(&config_path)?,
            ConfigAction::Test => commands::config::test(&config_path).await?,
            ConfigAction::Edit => commands::config::edit(&config_path)?,
        },
        Command::Pin { path } => {
            commands::pin::pin(&config_path, &path).await?;
        }
        Command::Unpin { path } => {
            commands::pin::unpin(&config_path, &path).await?;
        }
        Command::Conflicts { action } => match action {
            None => commands::conflicts::list(&config_path).await?,
            Some(ConflictAction::KeepLocal { path }) =>
                commands::conflicts::keep_local(&config_path, &path).await?,
            Some(ConflictAction::KeepRemote { path }) =>
                commands::conflicts::keep_remote(&config_path, &path).await?,
            Some(ConflictAction::Merge { path }) =>
                commands::conflicts::merge(&config_path, &path).await?,
            Some(ConflictAction::Diff { path }) =>
                commands::conflicts::diff(&config_path, &path).await?,
            Some(ConflictAction::Cleanup { dry_run }) =>
                commands::conflicts::cleanup(&config_path, dry_run).await?,
        }
        Command::Dashboard { once } => {
            commands::dashboard::run(&config_path, once).await?;
        }
        Command::Daemon { action } => match action {
            DaemonAction::Start            => commands::daemon::start()?,
            DaemonAction::Stop             => commands::daemon::stop()?,
            DaemonAction::Restart          => commands::daemon::restart()?,
            DaemonAction::Reload           => commands::daemon::reload()?,
            DaemonAction::Status           => commands::daemon::status()?,
            DaemonAction::Enable { now }   => commands::daemon::enable(now)?,
            DaemonAction::Disable { now }  => commands::daemon::disable(now)?,
            DaemonAction::Logs { follow, lines } =>
                commands::daemon::logs(follow, lines)?,
        }
        Command::Versions { action } => match action {
            VersionsAction::List { path } =>
                commands::versions::list(&config_path, &path).await?,
            VersionsAction::Restore { path, index } =>
                commands::versions::restore(&config_path, &path, index).await?,
        },
        Command::Completions => {
            println!("Add one of the following to your shell config:\n");
            println!("  Bash (~/.bashrc):");
            println!("    source <(COMPLETE=bash stratosync)\n");
            println!("  Zsh (~/.zshrc):");
            println!("    source <(COMPLETE=zsh stratosync)\n");
            println!("  Fish (~/.config/fish/config.fish):");
            println!("    COMPLETE=fish stratosync | source\n");
            println!("Then restart your shell or source the config file.");
        }
    }
    Ok(())
}
