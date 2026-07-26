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

    /// Exit when the connection drops instead of reconnecting.
    #[arg(long)]
    pub no_reconnect: bool,

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
    /// List the tmux sessions running on a host.
    Sessions {
        /// Host alias to query.
        host: String,
    },
    /// Kill a tmux session on a host.
    Kill {
        /// Host alias.
        host: String,
        /// Session to kill. Defaults to the host's configured session.
        session: Option<String>,
    },
    /// Rename a tmux session on a host.
    Rename {
        /// Host alias.
        host: String,
        /// New session name.
        new_name: String,
        /// Session to rename. Defaults to the host's configured session.
        #[arg(long, value_name = "NAME")]
        session: Option<String>,
    },
    /// Manage the encrypted settings vault (address, name, username, password).
    Vault {
        #[command(subcommand)]
        action: VaultAction,
    },
}

#[derive(Subcommand, Debug)]
pub enum VaultAction {
    /// Create a new encrypted credential vault.
    Init,
    /// Store settings for a host (prompts for name, address, username, password).
    Set {
        /// Host alias to store settings for.
        host: String,
    },
    /// Remove stored settings for a host.
    Remove {
        /// Host alias to remove.
        host: String,
    },
    /// List all hosts that have stored settings.
    List,
    /// Show vault status (exists, entry count).
    Status,
    /// Change the vault passphrase.
    ChangePassphrase,
}
