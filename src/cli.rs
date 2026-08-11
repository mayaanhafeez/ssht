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

    /// Edit shell lines locally for instant echo on high-latency links.
    #[arg(long, conflicts_with = "mosh")]
    pub local_echo: bool,

    /// Connect with Mosh for roaming and predictive local echo.
    #[arg(long, conflicts_with = "local_echo")]
    pub mosh: bool,

    /// Local port forward, as `ssh -L`. Repeatable; adds to configured ones.
    #[arg(short = 'L', value_name = "SPEC")]
    pub local_forward: Vec<String>,

    /// Remote port forward, as `ssh -R`. Repeatable; adds to configured ones.
    #[arg(short = 'R', value_name = "SPEC")]
    pub remote_forward: Vec<String>,

    /// Dynamic SOCKS forward, as `ssh -D`. Repeatable; adds to configured ones.
    #[arg(short = 'D', value_name = "SPEC")]
    pub dynamic_forward: Vec<String>,
    /// Attach as a viewer: see the session but send no input to it.
    #[arg(long)]
    pub read_only: bool,
    /// Replay this many lines of history on attach, overriding the config.
    #[arg(long, value_name = "N")]
    pub scrollback: Option<u32>,

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
    /// Copy files to or from a host over the existing connection.
    ///
    /// Exactly one of SOURCE and DEST names a host, as `host:path`.
    Cp {
        /// Source path, local or `host:path`.
        source: String,
        /// Destination path, local or `host:path`.
        dest: String,
        /// Copy directories recursively.
        #[arg(short, long)]
        recursive: bool,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mosh_and_local_echo_conflict() {
        assert!(Cli::try_parse_from(["ssht", "--mosh", "--local-echo", "host"]).is_err());
    }

    #[test]
    fn mosh_can_be_selected_for_a_host() {
        let cli = Cli::try_parse_from(["ssht", "--mosh", "host"]).unwrap();
        assert!(cli.mosh);
        assert_eq!(cli.host.as_deref(), Some("host"));
    }
}
