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

use std::process::Command as ProcCommand;

use anyhow::{Context, Result};
use clap::Parser;

use crate::cli::{Cli, Command};
use crate::config::Config;
use crate::model::{merge_hosts, Host};
use crate::state::State;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let config = Config::load().context("loading ssht config")?;
    let state = State::open().context("opening state database")?;

    match cli.command {
        Some(Command::List) => cmd_list(&config, &state),
        Some(Command::Last) => cmd_last(&config, &state, cli.layout.as_deref(), &cli.ssh_args),
        Some(Command::Edit) => cmd_edit(),
        None => match cli.host {
            Some(host) => connect::connect(
                &host,
                &config,
                &state,
                cli.layout.as_deref(),
                &cli.ssh_args,
            ),
            None => cmd_picker(&config, &state, cli.layout.as_deref(), &cli.ssh_args).await,
        },
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
) -> Result<()> {
    match state.last_host()? {
        Some(alias) => {
            eprintln!("Reconnecting to {alias}…");
            connect::connect(&alias, config, state, layout, ssh_args)
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
) -> Result<()> {
    let hosts = load_hosts(config, state)?;
    if hosts.is_empty() {
        eprintln!(
            "No hosts found. Add entries to ~/.ssh/config or ~/.config/ssht/config.toml."
        );
        return Ok(());
    }

    // Kick off background tmux probes, then run the picker on a blocking thread
    // so the async probes can make progress on the runtime's worker threads.
    let aliases: Vec<String> = hosts.iter().map(|h| h.alias.clone()).collect();
    let rx = tmux::spawn_status_probes(aliases);

    let picker_hosts = hosts.clone();
    let selection = tokio::task::spawn_blocking(move || picker::run_picker(picker_hosts, rx))
        .await
        .context("picker task panicked")??;

    match selection {
        Some(alias) => connect::connect(&alias, config, state, layout, ssh_args),
        None => Ok(()),
    }
}
