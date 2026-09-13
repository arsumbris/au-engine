use std::process::ExitCode;

#[cfg(unix)]
use std::path::PathBuf;

#[cfg(unix)]
use clap::{Parser, Subcommand};

/// The `au` binary: the per-repo daemon and its management commands. Consumers
/// start a daemon and speak the wire (`crates/au-engine/WIRE.md`) themselves.
#[cfg(unix)]
#[derive(Parser)]
#[command(name = "au", version, about = "Ars Umbris engine daemon")]
struct Cli {
    #[command(subcommand)]
    command: TopCommand,
}

#[cfg(unix)]
#[derive(Subcommand)]
enum TopCommand {
    /// Run and manage the per-repo daemon.
    Daemon {
        #[command(subcommand)]
        cmd: DaemonCommand,
    },
    /// Open a named workspace: resolve it through the per-user
    /// `~/.arsumbris/au-engine/config/workspaces.yaml` index to its workspace-repo folder,
    /// then serve it in the foreground like `daemon start <folder>`.
    Open {
        /// The workspace name, an entry in `workspaces.yaml` (an explicit `name`,
        /// else the folder's basename).
        name: String,
    },
}

/// Per-workspace daemon lifecycle. The daemon serves the engine's reads and
/// subscriptions over a Unix socket at a hashed path under `~/.arsumbris/au-engine/run/`.
#[cfg(unix)]
#[derive(Subcommand)]
enum DaemonCommand {
    /// Boot a daemon over the entry and serve in the foreground until stopped.
    Start {
        /// The entry: a folder-repo directory (a repo carrying
        /// `.arsumbris/repo.yaml`, composed by its optional
        /// `.arsumbris/workspace.yaml`). Defaults to the current directory.
        #[arg(default_value = ".")]
        entry: PathBuf,
    },
    /// Ask the daemon serving the entry to shut down.
    Stop {
        /// The entry: a folder-repo directory. Defaults to the current directory.
        #[arg(default_value = ".")]
        entry: PathBuf,
        /// Also reclaim a booting or wedged daemon that has not bound its
        /// socket, by signalling its recorded pid (a graceful window, then
        /// SIGTERM, then SIGKILL). Plain `stop` reaches only a serving daemon
        /// over the socket.
        #[arg(long)]
        force: bool,
    },
    /// Report whether a daemon is serving the entry.
    Status {
        /// The entry: a folder-repo directory. Defaults to the current directory.
        #[arg(default_value = ".")]
        entry: PathBuf,
    },
}

#[cfg(unix)]
fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        TopCommand::Daemon { cmd } => match cmd {
            DaemonCommand::Start { entry } => au_cli::daemon::start(&entry),
            DaemonCommand::Stop { entry, force } => au_cli::daemon::stop(&entry, force),
            DaemonCommand::Status { entry } => au_cli::daemon::status(&entry),
        },
        TopCommand::Open { name } => au_cli::daemon::open(&name),
    }
}

#[cfg(not(unix))]
fn main() -> ExitCode {
    eprintln!("au: only Unix is supported — the daemon serves over a Unix domain socket");
    ExitCode::FAILURE
}
