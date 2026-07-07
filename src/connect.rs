//! Performing the actual SSH + tmux connection by shelling out to `ssh`.

use std::process::Command;

use anyhow::{Context, Result};

use crate::config::Config;
use crate::state::State;
use crate::tmux;
use crate::vault::{self, LazyVault};

/// Connect to `alias`: ssh in and attach/create the tmux session.
/// If the vault contains settings for `alias` (address, username, password),
/// those override the alias and are passed to `ssh`.
pub fn connect(
    alias: &str,
    config: &Config,
    state: &State,
    layout_override: Option<&str>,
    ssh_passthrough: &[String],
    vault: &mut LazyVault,
) -> Result<()> {
    let session = config.session_for(alias);
    let layout = config.resolve_layout(alias, layout_override);

    if layout_override.is_some() && layout.is_none() {
        anyhow::bail!(
            "layout {:?} not found in config",
            layout_override.unwrap_or("")
        );
    }

    let remote = tmux::build_remote_command(&session, layout);

    state
        .record_connection(alias)
        .with_context(|| format!("recording connection to {alias}"))?;

    if vault.might_have_settings(alias)? {
        vault.ensure_unlocked().context("unlocking vault")?;
    }
    let settings = vault.get_settings(alias)?;

    let mut cmd = Command::new("ssh");
    cmd.arg("-t");
    cmd.args(ssh_passthrough);

    let _askpass_cleanup = settings.as_ref()
        .and_then(|s| s.password.as_deref())
        .map(|password| vault::setup_ssh_askpass(&mut cmd, password))
        .transpose()?;

    if let Some(ref s) = settings {
        if let Some(ref user) = s.username {
            cmd.arg("-l");
            cmd.arg(user);
        }
    }

    let target = settings
        .as_ref()
        .and_then(|s| s.address.as_deref())
        .unwrap_or(alias);
    cmd.arg(target);
    cmd.arg(&remote);

    let status = cmd
        .status()
        .with_context(|| format!("launching ssh to {target} (is ssh installed?)"))?;

    if !status.success() {
        if let Some(code) = status.code() {
            if code == 255 {
                anyhow::bail!("ssh connection to {alias} failed");
            }
        }
    }
    Ok(())
}
