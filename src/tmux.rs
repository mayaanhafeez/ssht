//! Building the remote tmux bootstrap command, and async checks for whether a
//! tmux session is currently running on a host.

use std::sync::Arc;

use tokio::sync::{mpsc, Semaphore};

use crate::config::Layout;

/// Quote a string for safe inclusion in a POSIX shell command.
fn sh_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// Replay the session's scrollback to stdout before attaching, so the *local*
/// terminal owns that history and native scrolling works.
///
/// This is the part mosh gets criticised for: it runs a screen-sized emulator
/// and eats the terminal's own scrollback, leaving nothing to scroll back
/// through. Writing the history out as plain output hands it to iTerm/kitty/
/// Ghostty, which already know how to store and scroll it.
///
/// Two details matter:
///
/// * `-E -1` stops the capture one line above the visible screen. Attaching
///   redraws that screen anyway, so capturing it too would print everything
///   on it twice.
/// * The `alternate_on` guard skips the dump entirely while a full-screen
///   application (vim, htop, less) is in the alternate screen. There the pane
///   holds an editor frame rather than history, and replaying it would fill
///   scrollback with a snapshot of a UI.
fn scrollback_replay(session: &str, lines: u32) -> String {
    if lines == 0 {
        return String::new();
    }
    let s = sh_quote(session);
    let alt = sh_quote("#{alternate_on}");
    format!(
        "if tmux has-session -t {s} 2>/dev/null && \
         [ \"$(tmux display-message -p -t {s} {alt})\" = 0 ]; then \
         tmux capture-pane -p -e -S -{lines} -E -1 -t {s}; fi; "
    )
}

/// Build the remote command run over ssh that attaches to (or creates) the tmux
/// session, optionally applying a layout on first creation and replaying up to
/// `scrollback_lines` of history first.
pub fn build_remote_command(
    session: &str,
    layout: Option<&Layout>,
    scrollback_lines: u32,
) -> String {
    let s = sh_quote(session);
    let replay = scrollback_replay(session, scrollback_lines);

    // No layout (or empty layout): the canonical attach-or-create one-liner.
    let layout = match layout {
        Some(l) if !l.windows.is_empty() => l,
        _ => return format!("{replay}tmux new-session -A -s {s}"),
    };

    // Build the create branch: a detached session with the configured windows,
    // run only when the session does not already exist.
    let mut create = String::new();
    for (i, win) in layout.windows.iter().enumerate() {
        let name = sh_quote(&win.name);
        if i == 0 {
            create.push_str(&format!("tmux new-session -d -s {s} -n {name}; "));
        } else {
            create.push_str(&format!("tmux new-window -t {s} -n {name}; "));
        }
        if let Some(cmd) = &win.command {
            let target = sh_quote(&format!("{session}:{i}"));
            let keys = sh_quote(cmd);
            create.push_str(&format!("tmux send-keys -t {target} {keys} C-m; "));
        }
    }
    // Focus the first window before attaching.
    let first = sh_quote(&format!("{session}:0"));
    create.push_str(&format!("tmux select-window -t {first}; "));

    format!(
        "{replay}if tmux has-session -t {s} 2>/dev/null; then tmux attach -t {s}; \
         else {create}tmux attach -t {s}; fi"
    )
}

/// Result of a background tmux status probe.
#[derive(Debug, Clone)]
pub struct TmuxStatus {
    pub alias: String,
    /// `Some(true)` if a session is active, `Some(false)` if not, `None` if the
    /// host was unreachable / probe failed.
    pub active: Option<bool>,
}

/// Probe a single host for an active tmux server (non-interactive, time-bounded).
async fn probe(alias: String) -> TmuxStatus {
    let output = tokio::process::Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=4",
            "-o",
            "StrictHostKeyChecking=accept-new",
            &alias,
            "tmux ls 2>/dev/null",
        ])
        .output()
        .await;

    let active = match output {
        Ok(out) if out.status.success() => Some(!out.stdout.is_empty()),
        // ssh connected but tmux returned non-zero (no server) → no session.
        Ok(out) if !out.stdout.is_empty() => Some(true),
        Ok(_) => None,
        Err(_) => None,
    };

    TmuxStatus { alias, active }
}

/// Spawn background probes for all given aliases, returning a receiver that
/// yields statuses as they complete. Concurrency is bounded.
pub fn spawn_status_probes(aliases: Vec<String>) -> mpsc::UnboundedReceiver<TmuxStatus> {
    let (tx, rx) = mpsc::unbounded_channel();
    let sem = Arc::new(Semaphore::new(8));

    for alias in aliases {
        let tx = tx.clone();
        let sem = sem.clone();
        tokio::spawn(async move {
            // If the semaphore is closed something is very wrong; just bail.
            let _permit = match sem.acquire().await {
                Ok(p) => p,
                Err(_) => return,
            };
            let status = probe(alias).await;
            let _ = tx.send(status);
        });
    }

    rx
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Window;

    #[test]
    fn quotes_safely() {
        assert_eq!(sh_quote("main"), "'main'");
        assert_eq!(sh_quote("a b"), "'a b'");
        assert_eq!(sh_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn plain_session_uses_attach_or_create() {
        let cmd = build_remote_command("main", None, 0);
        assert_eq!(cmd, "tmux new-session -A -s 'main'");
    }

    #[test]
    fn empty_layout_falls_back_to_plain() {
        let layout = Layout { windows: vec![] };
        let cmd = build_remote_command("main", Some(&layout), 0);
        assert_eq!(cmd, "tmux new-session -A -s 'main'");
    }

    #[test]
    fn zero_lines_replays_nothing() {
        assert_eq!(scrollback_replay("main", 0), "");
        assert!(!build_remote_command("main", None, 0).contains("capture-pane"));
    }

    #[test]
    fn replay_captures_history_above_the_visible_screen() {
        let cmd = build_remote_command("main", None, 500);
        // -E -1 stops above the screen that attaching will redraw anyway.
        assert!(cmd.contains("tmux capture-pane -p -e -S -500 -E -1 -t 'main'"));
        // and the replay has to happen before the attach, not after
        let replay_at = cmd.find("capture-pane").unwrap();
        let attach_at = cmd.find("new-session").unwrap();
        assert!(replay_at < attach_at);
    }

    #[test]
    fn replay_skips_the_alternate_screen() {
        let cmd = build_remote_command("main", None, 500);
        assert!(cmd.contains("'#{alternate_on}'"));
    }

    #[test]
    fn replay_precedes_a_layout_bootstrap_too() {
        let layout = Layout {
            windows: vec![Window { name: "editor".into(), command: None }],
        };
        let cmd = build_remote_command("dev", Some(&layout), 200);
        assert!(cmd.starts_with("if tmux has-session -t 'dev' 2>/dev/null && "));
        assert!(cmd.contains("capture-pane"));
        assert!(cmd.contains("new-session -d -s 'dev'"));
    }

    #[test]
    fn layout_creates_windows_only_when_absent() {
        let layout = Layout {
            windows: vec![
                Window { name: "editor".into(), command: Some("nvim".into()) },
                Window { name: "shell".into(), command: None },
            ],
        };
        let cmd = build_remote_command("dev", Some(&layout), 0);
        assert!(cmd.starts_with("if tmux has-session -t 'dev'"));
        assert!(cmd.contains("tmux new-session -d -s 'dev' -n 'editor'"));
        assert!(cmd.contains("tmux send-keys -t 'dev:0' 'nvim' C-m"));
        assert!(cmd.contains("tmux new-window -t 'dev' -n 'shell'"));
        assert!(cmd.contains("tmux attach -t 'dev'"));
    }
}
