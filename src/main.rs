//! ssht — a smart SSH session manager that auto-attaches to persistent tmux
//! sessions on the remote host.

mod cli;
mod config;
mod connect;
mod model;
mod picker;
mod ssh_config;
mod state;
mod tmux;
mod util;
mod vault;

use std::process::Command as ProcCommand;

use anyhow::{Context, Result, bail};
use clap::Parser;

use crate::cli::{Cli, Command, VaultAction};
use crate::config::Config;
use crate::model::{merge_hosts, Host};
use crate::state::State;
use crate::vault::{HostSettings, LazyVault, Vault};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let config = Config::load().context("loading ssht config")?;
    let state = State::open().context("opening state database")?;
    let mut vault = LazyVault::new();

    match cli.command {
        Some(Command::List) => cmd_list(&config, &state),
        Some(Command::Last) => {
            cmd_last(&config, &state, cli.layout.as_deref(), &cli.ssh_args, &mut vault)
        }
        Some(Command::Edit) => cmd_edit(),
        Some(Command::Vault { action }) => cmd_vault(action),
        None => match cli.host {
            Some(host) => connect::connect(
                &host,
                &config,
                &state,
                cli.layout.as_deref(),
                &cli.ssh_args,
                &mut vault,
            ),
            None => cmd_picker(&config, &state, cli.layout.as_deref(), &cli.ssh_args, &mut vault).await,
        },
    }
}

fn cmd_vault(action: VaultAction) -> Result<()> {
    match action {
        VaultAction::Init => {
            if Vault::exists()? {
                bail!("vault already exists at {}", Vault::vault_path()?.display());
            }
            let passphrase = vault::prompt_passphrase("New vault passphrase: ")?;
            let confirm = vault::prompt_passphrase("Confirm passphrase: ")?;
            if passphrase != confirm {
                bail!("passphrases do not match");
            }
            if passphrase.is_empty() {
                bail!("passphrase cannot be empty");
            }
            let _vault = Vault::init(&passphrase)?;
            eprintln!("Vault created at {}", Vault::vault_path()?.display());
            Ok(())
        }
        VaultAction::Set { host } => {
            let passphrase = vault::prompt_passphrase("Vault passphrase: ")?;
            let mut vault = Vault::open(&passphrase)?;
            let existing = vault.get_settings(&host).cloned().unwrap_or_default();
            let name = vault::prompt_field("Name", existing.name.as_deref().or(Some(&host)))?;
            let addr = vault::prompt_field("Address", existing.address.as_deref())?;
            let user = vault::prompt_field("Username", existing.username.as_deref())?;
            let password = vault::prompt_password_field("Password", existing.password.as_deref())?;
            let settings = HostSettings {
                name: Some(name).filter(|s| !s.is_empty()),
                address: Some(addr).filter(|s| !s.is_empty()),
                username: Some(user).filter(|s| !s.is_empty()),
                password: Some(password).filter(|s| !s.is_empty()),
            };
            vault.set_settings(&host, settings)?;
            eprintln!("Settings saved for {host}");
            Ok(())
        }
        VaultAction::Remove { host } => {
            let passphrase = vault::prompt_passphrase("Vault passphrase: ")?;
            let mut vault = Vault::open(&passphrase)?;
            if vault.get_settings(&host).is_some() {
                vault.remove(&host)?;
                eprintln!("Settings removed for {host}");
            } else {
                eprintln!("No settings stored for {host}");
            }
            Ok(())
        }
        VaultAction::List => {
            let passphrase = vault::prompt_passphrase("Vault passphrase: ")?;
            let vault = Vault::open(&passphrase)?;
            if vault.is_empty() {
                eprintln!("Vault is empty");
            } else {
                for alias in vault.list() {
                    let s = vault.get_settings(alias);
                    let info = s.and_then(|s| s.address.as_deref()).unwrap_or("-");
                    println!("{alias:20} address={info}");
                }
            }
            Ok(())
        }
        VaultAction::Status => {
            if Vault::exists()? {
                eprintln!("Vault exists at {}", Vault::vault_path()?.display());
                // We can't show entry count without decrypting, so just report existence
            } else {
                eprintln!("No vault found");
            }
            Ok(())
        }
        VaultAction::ChangePassphrase => {
            let old_passphrase = vault::prompt_passphrase("Current vault passphrase: ")?;
            let mut vault = Vault::open(&old_passphrase)?;
            let new_passphrase = vault::prompt_passphrase("New vault passphrase: ")?;
            let confirm = vault::prompt_passphrase("Confirm new passphrase: ")?;
            if new_passphrase != confirm {
                bail!("passphrases do not match");
            }
            if new_passphrase.is_empty() {
                bail!("passphrase cannot be empty");
            }
            vault.change_passphrase(&new_passphrase)?;
            eprintln!("Vault passphrase changed");
            Ok(())
        }
    }
}

/// Gather and merge hosts from all sources.
fn load_hosts(config: &Config, state: &State) -> Result<Vec<Host>> {
    let discovered = ssh_config::discover().context("discovering hosts")?;
    let states = state.all().context("reading state")?;
    Ok(merge_hosts(discovered, &config.hosts, &states))
}

fn cmd_list(config: &Config, state: &State) -> Result<()> {
    let hosts = load_hosts(config, state)?;
    for host in &hosts {
        println!("{}", host.alias);
    }
    Ok(())
}

fn cmd_last(
    config: &Config,
    state: &State,
    layout: Option<&str>,
    ssh_args: &[String],
    vault: &mut LazyVault,
) -> Result<()> {
    match state.last_host()? {
        Some(alias) => {
            eprintln!("Reconnecting to {alias}…");
            connect::connect(&alias, config, state, layout, ssh_args, vault)
        }
        None => {
            anyhow::bail!("no previous connection recorded yet");
        }
    }
}

fn cmd_edit() -> Result<()> {
    let path = config::ensure_config_file()?;
    let editor = std::env::var("EDITOR")
        .or_else(|_| std::env::var("VISUAL"))
        .unwrap_or_else(|_| "vi".to_string());
    let status = ProcCommand::new(&editor)
        .arg(&path)
        .status()
        .with_context(|| format!("launching editor {editor:?}"))?;
    if !status.success() {
        anyhow::bail!("editor {editor:?} exited with failure");
    }
    Ok(())
}

async fn cmd_picker(
    config: &Config,
    state: &State,
    layout: Option<&str>,
    ssh_args: &[String],
    vault: &mut LazyVault,
) -> Result<()> {
    let hosts = load_hosts(config, state)?;
    if hosts.is_empty() {
        eprintln!(
            "No hosts found. Add entries to ~/.ssh/config or ~/.config/ssht/config.toml."
        );
        return Ok(());
    }

    let aliases: Vec<String> = hosts.iter().map(|h| h.alias.clone()).collect();
    let rx = tmux::spawn_status_probes(aliases);

    let picker_hosts = hosts.clone();
    let selection = picker::run_picker(picker_hosts, rx, vault)
        .context("picker failed")?;

    match selection {
        Some(alias) => connect::connect(&alias, config, state, layout, ssh_args, vault),
        None => Ok(()),
    }
}
