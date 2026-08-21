//! Managing the tmux sessions on a remote host from the client side:
//! list them, rename them, kill them — without attaching first.
//!
//! Every operation is a one-shot `ssh <host> tmux <verb>`, which means it
//! inherits the same auth story as connecting: whatever lets you `ssht` into a
//! box lets you manage its sessions.

use std::process::{Command, Output};

use anyhow::{Context, Result};

use crate::tmux::sh_quote;
use crate::vault::{self, LazyVault};

/// One tmux session as reported by the remote server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub name: String,
    pub windows: u32,
    pub clients: u32,
}

/// Tab-separated, one line per session. Explicit format rather than parsing
/// tmux's human-readable default, which varies across versions.
const LIST_FORMAT: &str = "#{session_name}\t#{session_windows}\t#{session_attached}";

/// Parse `tmux ls -F` output. Malformed lines are skipped rather than failing
/// the whole listing — a session with a tab in its name shouldn't hide the rest.
fn parse_sessions(stdout: &str) -> Vec<SessionInfo> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let name = fields.next()?.trim();
            if name.is_empty() {
                return None;
            }
            let windows = fields.next()?.trim().parse().ok()?;
            let clients = fields.next()?.trim().parse().ok()?;
            Some(SessionInfo {
                name: name.to_string(),
                windows,
                clients,
            })
        })
        .collect()
}

/// Run a command on the remote over ssh, applying vault overrides for address,
/// username and password exactly as connecting does.
fn run_remote(alias: &str, vault: &mut LazyVault, remote: &str) -> Result<Output> {
    if vault.might_have_settings(alias)? {
        vault.ensure_unlocked().context("unlocking vault")?;
    }
    let settings = vault.get_settings(alias)?;

    let mut cmd = Command::new("ssh");

    let _askpass_cleanup = settings
        .as_ref()
        .and_then(|s| s.password.as_deref())
        .map(|password| vault::setup_ssh_askpass(&mut cmd, password))
        .transpose()?;

    if let Some(user) = settings.as_ref().and_then(|s| s.username.as_deref()) {
        cmd.arg("-l");
        cmd.arg(user);
    }

    let target = settings
        .as_ref()
        .and_then(|s| s.address.as_deref())
        .unwrap_or(alias);

    cmd.arg(target);
    cmd.arg(remote);

    cmd.output()
        .with_context(|| format!("launching ssh to {target} (is ssh installed?)"))
}

/// List the tmux sessions on `alias`.
///
/// A host with no tmux server running is not an error — it reports no sessions.
/// tmux signals that with a non-zero exit and a message on stderr, which is
/// indistinguishable from success-with-no-output for our purposes.
pub fn list(alias: &str, vault: &mut LazyVault) -> Result<Vec<SessionInfo>> {
    let remote = format!("tmux ls -F {} 2>/dev/null", sh_quote(LIST_FORMAT));
    let out = run_remote(alias, vault, &remote)?;

    // ssh itself failing (unreachable, auth) must surface; tmux exiting
    // non-zero because there's no server must not.
    if out.status.code() == Some(255) {
        let stderr = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("ssh connection to {alias} failed: {}", stderr.trim());
    }

    Ok(parse_sessions(&String::from_utf8_lossy(&out.stdout)))
}

/// Kill one tmux session on `alias`.
pub fn kill(alias: &str, session: &str, vault: &mut LazyVault) -> Result<()> {
    let remote = format!("tmux kill-session -t {}", sh_quote(session));
    let out = run_remote(alias, vault, &remote)?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("killing session {session:?} on {alias}: {}", stderr.trim());
    }
    Ok(())
}

/// Rename a tmux session on `alias`.
pub fn rename(alias: &str, from: &str, to: &str, vault: &mut LazyVault) -> Result<()> {
    let remote = format!("tmux rename-session -t {} {}", sh_quote(from), sh_quote(to));
    let out = run_remote(alias, vault, &remote)?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("renaming session {from:?} on {alias}: {}", stderr.trim());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_listing() {
        let out = "main\t3\t1\nbuild\t1\t0\n";
        assert_eq!(
            parse_sessions(out),
            vec![
                SessionInfo {
                    name: "main".into(),
                    windows: 3,
                    clients: 1
                },
                SessionInfo {
                    name: "build".into(),
                    windows: 1,
                    clients: 0
                },
            ]
        );
    }

    #[test]
    fn empty_output_is_no_sessions() {
        assert!(parse_sessions("").is_empty());
        assert!(parse_sessions("\n").is_empty());
    }

    #[test]
    fn skips_malformed_lines_but_keeps_the_rest() {
        let out = "main\t3\t1\ngarbage\nbuild\tnotanumber\t0\nlogs\t2\t0\n";
        let sessions = parse_sessions(out);
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].name, "main");
        assert_eq!(sessions[1].name, "logs");
    }

    #[test]
    fn preserves_attached_client_count() {
        let sessions = parse_sessions("main\t1\t2\n");
        assert_eq!(sessions[0].clients, 2);
    }
}
