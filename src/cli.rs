//! Command-line interface definition (clap).

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "ssht",
    version,
    about = "SSH + tmux session manager — connect and auto-attach to a persistent tmux session",
    args_conflicts_with_subcommands = true,
    subcommand_negates_reqs = true
)]
pub struct Cli {
    /// Host alias to connect to. Omit to open the interactive picker.
    pub host: Option<String>,

    /// Apply a named layout (from config) after attaching.
    #[arg(long, value_name = "NAME")]
    pub layout: Option<String>,

    /// Extra arguments passed directly to `ssh` (everything after `--`).
    #[arg(last = true, value_name = "SSH_ARGS")]
    pub ssh_args: Vec<String>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Print all known hosts (one per line) for scripting.
    List,
    /// Reconnect to the most recently used host.
    Last,
    /// Open the ssht config file in $EDITOR.
    Edit,
}
