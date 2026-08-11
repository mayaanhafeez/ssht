//! Install the active local terminal description for remote SSH clients.

use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};

use crate::tmux::sh_quote;
use crate::vault;

const FALLBACK_TERM: &str = "xterm-256color";
const NO_REMOTE_TIC: i32 = 42;

/// Ensure the remote account can describe the terminal inherited by SSH.
/// Returns a conservative TERM override when installation is not possible.
pub fn ensure_remote(
    target: &str,
    username: Option<&str>,
    password: Option<&str>,
    mux_args: &[String],
    ssh_passthrough: &[String],
) -> Result<Option<String>> {
    let Some(requested_term) = active_term() else {
        return Ok(None);
    };

    let (term, source, forced_override) = match export(&requested_term) {
        Some(source) => (requested_term, source, false),
        _ => {
            eprintln!(
                "ssht: local terminfo for {requested_term} is unavailable; using {FALLBACK_TERM} remotely"
            );
            let source = export(FALLBACK_TERM).with_context(|| {
                format!("local terminfo for fallback {FALLBACK_TERM} is unavailable")
            })?;
            (FALLBACK_TERM.to_string(), source, true)
        }
    };

    let remote = install_command(&term);
    let mut cmd = Command::new("ssh");
    cmd.arg("-T");
    cmd.args(mux_args);
    cmd.args(ssh_passthrough);
    if let Some(user) = username {
        cmd.args(["-l", user]);
    }
    cmd.arg(target);
    cmd.arg(remote);
    cmd.stdin(Stdio::piped());

    let _askpass_cleanup = password
        .map(|password| vault::setup_ssh_askpass(&mut cmd, password))
        .transpose()?;
    let mut child = cmd
        .spawn()
        .with_context(|| format!("checking terminfo for {term} on {target}"))?;
    let write_result = child
        .stdin
        .take()
        .expect("piped SSH stdin")
        .write_all(&source);
    let status = child
        .wait()
        .with_context(|| format!("waiting for terminfo setup on {target}"))?;

    if status.success() {
        write_result.context("sending local terminfo to the remote host")?;
        return Ok(forced_override.then_some(term));
    }
    if status.code() == Some(NO_REMOTE_TIC) {
        eprintln!("ssht: remote tic is unavailable for {term}; using {FALLBACK_TERM} remotely");
        return Ok(Some(FALLBACK_TERM.to_string()));
    }
    if status.code() == Some(255) {
        anyhow::bail!("SSH failed while checking terminfo on {target}");
    }

    eprintln!("ssht: could not install remote terminfo for {term}; using {FALLBACK_TERM} remotely");
    Ok(Some(FALLBACK_TERM.to_string()))
}

fn active_term() -> Option<String> {
    std::env::var("TERM")
        .ok()
        .filter(|term| !term.is_empty() && term != "dumb")
}

fn export(term: &str) -> Option<Vec<u8>> {
    Command::new("infocmp")
        .args(["-x", term])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| output.stdout)
}

fn install_command(term: &str) -> String {
    let term = sh_quote(term);
    format!(
        "term={term}; \
         if command -v infocmp >/dev/null 2>&1 && infocmp -x \"$term\" >/dev/null 2>&1; then \
         cat >/dev/null; exit 0; fi; \
         if ! command -v tic >/dev/null 2>&1; then cat >/dev/null; exit {NO_REMOTE_TIC}; fi; \
         mkdir -p \"$HOME/.terminfo\" && tic -x -o \"$HOME/.terminfo\" -"
    )
}

pub fn apply_override(command: &str, term: Option<&str>) -> String {
    match term {
        Some(term) => format!("export TERM={}; {command}", sh_quote(term)),
        None => command.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_script_is_valid_shell_and_quotes_term() {
        let script = install_command("odd term'value");
        assert!(script.contains("'odd term'\\''value'"));

        #[cfg(unix)]
        assert!(
            Command::new("sh")
                .args(["-n", "-c", &script])
                .status()
                .expect("run shell syntax check")
                .success()
        );
    }

    #[test]
    fn term_override_is_scoped_to_remote_command() {
        assert_eq!(
            apply_override("tmux attach", Some("xterm-kitty")),
            "export TERM='xterm-kitty'; tmux attach"
        );
        assert_eq!(apply_override("tmux attach", None), "tmux attach");
    }

    #[test]
    fn install_script_compiles_into_user_terminfo() {
        if Command::new("tic").arg("-V").output().is_err() {
            return;
        }

        let home = tempfile::tempdir().unwrap();
        let term = "ssht-test-terminal-entry";
        let source = format!("{term}|ssht test terminal,\n\tuse=xterm-256color,\n");
        let mut child = Command::new("sh")
            .args(["-c", &install_command(term)])
            .env("HOME", home.path())
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(source.as_bytes())
            .unwrap();
        assert!(child.wait().unwrap().success());
        assert!(
            Command::new("infocmp")
                .args(["-A", home.path().join(".terminfo").to_str().unwrap(), term])
                .stdout(Stdio::null())
                .status()
                .unwrap()
                .success()
        );
    }
}
