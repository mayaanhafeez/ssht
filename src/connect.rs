//! Performing the actual SSH + tmux connection by shelling out to `ssh`.

use std::process::Command;

use anyhow::{Context, Result};

use crate::config::Config;
use crate::state::State;
use crate::tmux;

/// Connect to `alias`: ssh in and attach/create the tmux session, applying an
/// optional layout. `ssh_passthrough` are extra args forwarded to `ssh`
/// (everything after `--`). Records the connection in the state DB.
pub fn connect(
    alias: &str,
    config: &Config,
    state: &State,
    layout_override: Option<&str>,
    ssh_passthrough: &[String],
) -> Result<()> {
    let session = config.session_for(alias);
    let layout = config.resolve_layout(alias, layout_override);

    if layout_override.is_some() && layout.is_none() {
        // A layout was explicitly requested but not found — fail loudly rather
        // than silently connecting without it.
        anyhow::bail!(
            "layout {:?} not found in config",
            layout_override.unwrap_or("")
        );
    }

    let remote = tmux::build_remote_command(&session, layout);

    // Record before launching so the timestamp reflects connect intent even for
    // long-lived sessions.
    state
        .record_connection(alias)
        .with_context(|| format!("recording connection to {alias}"))?;

    let mut cmd = Command::new("ssh");
    cmd.arg("-t"); // force a pseudo-tty for the interactive tmux session
    cmd.args(ssh_passthrough);
    cmd.arg(alias);
    cmd.arg(&remote);

    let status = cmd
        .status()
        .with_context(|| format!("launching ssh to {alias} (is ssh installed?)"))?;

    if !status.success() {
        if let Some(code) = status.code() {
            // 255 is ssh's own error code; anything else is the remote command.
            if code == 255 {
                anyhow::bail!("ssh connection to {alias} failed");
            }
        }
    }
    Ok(())
}
