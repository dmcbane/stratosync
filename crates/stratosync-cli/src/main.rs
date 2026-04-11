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
            println!("pin: {} (not yet implemented)", path.display());
        }
        Command::Unpin { path } => {
            println!("unpin: {} (not yet implemented)", path.display());
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
        }
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
