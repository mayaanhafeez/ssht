//! Performing the actual SSH + tmux connection by shelling out to `ssh`.

use std::process::{Command, ExitStatus};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::config::{Config, Forwards};
use crate::mux;
use crate::state::State;
use crate::tmux;
use crate::vault::{self, LazyVault};

/// The exit code `ssh` uses for its own failures — connection refused, host
/// unreachable, auth rejected, connection dropped mid-session. Anything else is
/// the *remote command's* exit status, which must not be treated as a transport
/// problem.
const SSH_FAILURE: i32 = 255;

/// A session that stayed up at least this long counts as "established". If one
/// of those drops it's a network flap rather than a host that won't have us, so
/// the attempt counter resets and the user gets a full retry budget again.
const ESTABLISHED_AFTER: Duration = Duration::from_secs(30);

/// How long to wait, and how many times, before giving up on a dropped session.
#[derive(Debug, Clone, Copy)]
struct ReconnectPolicy {
    enabled: bool,
    max_attempts: u32,
    initial_delay: Duration,
    max_delay: Duration,
}

impl ReconnectPolicy {
    fn from_config(config: &Config, disabled_by_flag: bool) -> Self {
        let s = &config.settings;
        ReconnectPolicy {
            enabled: s.reconnect && !disabled_by_flag,
            max_attempts: s.reconnect_max_attempts,
            initial_delay: Duration::from_secs(s.reconnect_delay_secs),
            max_delay: Duration::from_secs(s.reconnect_max_delay_secs),
        }
    }

    /// Exponential backoff, capped at `max_delay`. `attempt` is 1-based.
    fn delay_for(&self, attempt: u32) -> Duration {
        // Shift rather than pow, and saturate throughout: a large configured
        // attempt count must not wrap around into a tiny delay.
        let factor = 1u64
            .checked_shl(attempt.saturating_sub(1))
            .unwrap_or(u64::MAX);
        let secs = self.initial_delay.as_secs().saturating_mul(factor);
        Duration::from_secs(secs).min(self.max_delay)
    }
}

/// Everything the caller can vary about a single connection. Collected into a
/// struct because these arrive from several independent places — flags, config,
/// the picker — and threading them as positional parameters made every call
/// site a wall of bare booleans.
#[derive(Debug, Default, Clone)]
pub struct ConnectOptions {
    /// Exit when the transport drops instead of re-attaching.
    pub no_reconnect: bool,
    /// Port forwards from the command line, merged with configured ones.
    pub forwards: Forwards,
    /// Attach as a viewer: mirror the session but send it no input.
    pub read_only: bool,
}

/// Connect to `alias`: ssh in and attach/create the tmux session.
/// If the vault contains settings for `alias` (address, username, password),
/// those override the alias and are passed to `ssh`.
///
/// When the transport drops — laptop sleep, network switch, VPN flap — this
/// re-runs `ssh` until it comes back. That works because tmux is already
/// holding the session on the remote, so reattaching lands in exactly the same
/// place, running jobs and all.
pub fn connect(
    alias: &str,
    config: &Config,
    state: &State,
    layout_override: Option<&str>,
    ssh_passthrough: &[String],
    vault: &mut LazyVault,
    opts: &ConnectOptions,
) -> Result<()> {
    let session = config.session_for(alias);
    let layout = config.resolve_layout(alias, layout_override);

    if layout_override.is_some() && layout.is_none() {
        anyhow::bail!(
            "layout {:?} not found in config",
            layout_override.unwrap_or("")
        );
    }

    let remote = tmux::build_remote_command(&session, layout, opts.read_only);
    let policy = ReconnectPolicy::from_config(config, opts.no_reconnect);
    let forward_args = config.forward_args(alias, &opts.forwards)?;
    let mux_args = mux::ssh_args(config)?;

    state
        .record_connection(alias)
        .with_context(|| format!("recording connection to {alias}"))?;

    if vault.might_have_settings(alias)? {
        vault.ensure_unlocked().context("unlocking vault")?;
    }
    let settings = vault.get_settings(alias)?;

    let target = settings
        .as_ref()
        .and_then(|s| s.address.as_deref())
        .unwrap_or(alias)
        .to_string();
    let username = settings.as_ref().and_then(|s| s.username.clone());
    let password = settings.as_ref().and_then(|s| s.password.clone());

    let mut attempt: u32 = 0;
    loop {
        let started = Instant::now();
        let status = run_ssh(
            &target,
            username.as_deref(),
            password.as_deref(),
            &mux_args,
            &forward_args,
            ssh_passthrough,
            &remote,
        )?;
        let elapsed = started.elapsed();

        // A clean exit, or a failure from the remote command itself: either way
        // the user is done with this session and must not be dragged back in.
        if status.success() || status.code() != Some(SSH_FAILURE) {
            return Ok(());
        }

        if !policy.enabled {
            anyhow::bail!("ssh connection to {alias} failed");
        }

        // A session that ran for a while and then dropped earns a fresh budget.
        if elapsed >= ESTABLISHED_AFTER {
            attempt = 0;
        }

        attempt += 1;
        if attempt > policy.max_attempts {
            anyhow::bail!(
                "ssh connection to {alias} failed after {} reconnect attempts",
                policy.max_attempts
            );
        }

        let delay = policy.delay_for(attempt);
        eprintln!(
            "Connection to {alias} lost — reconnecting in {}s (attempt {}/{})…",
            delay.as_secs(),
            attempt,
            policy.max_attempts
        );
        std::thread::sleep(delay);
    }
}

/// Spawn one `ssh` invocation and wait for it. Built fresh per attempt so the
/// askpass scratch directory is recreated and torn down around each try.
fn run_ssh(
    target: &str,
    username: Option<&str>,
    password: Option<&str>,
    mux_args: &[String],
    forward_args: &[String],
    ssh_passthrough: &[String],
    remote: &str,
) -> Result<ExitStatus> {
    let mut cmd = Command::new("ssh");
    cmd.arg("-t");
    // Become the control master, so `ssht cp` to this host reuses the
    // connection instead of opening (and authenticating) a second one. Re-set
    // on each attempt so a reconnect re-establishes the master too.
    cmd.args(mux_args);
    // Forwards are rebuilt on every attempt, which is what re-establishes them
    // after a drop. They go on before user passthrough, so an explicit
    // `-- -L ...` still has the last word if ssh sees a conflicting bind.
    cmd.args(forward_args);
    cmd.args(ssh_passthrough);

    let _askpass_cleanup = password
        .map(|password| vault::setup_ssh_askpass(&mut cmd, password))
        .transpose()?;

    if let Some(user) = username {
        cmd.arg("-l");
        cmd.arg(user);
    }

    cmd.arg(target);
    cmd.arg(remote);

    cmd.status()
        .with_context(|| format!("launching ssh to {target} (is ssh installed?)"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> ReconnectPolicy {
        ReconnectPolicy {
            enabled: true,
            max_attempts: 10,
            initial_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(30),
        }
    }

    #[test]
    fn backoff_doubles_then_caps() {
        let p = policy();
        assert_eq!(p.delay_for(1), Duration::from_secs(1));
        assert_eq!(p.delay_for(2), Duration::from_secs(2));
        assert_eq!(p.delay_for(3), Duration::from_secs(4));
        assert_eq!(p.delay_for(4), Duration::from_secs(8));
        assert_eq!(p.delay_for(5), Duration::from_secs(16));
        // capped from here on
        assert_eq!(p.delay_for(6), Duration::from_secs(30));
        assert_eq!(p.delay_for(50), Duration::from_secs(30));
    }

    #[test]
    fn backoff_does_not_overflow_on_absurd_attempt_counts() {
        let p = policy();
        assert_eq!(p.delay_for(u32::MAX), Duration::from_secs(30));
    }

    #[test]
    fn flag_overrides_config_default() {
        let config = Config::default();
        assert!(ReconnectPolicy::from_config(&config, false).enabled);
        assert!(!ReconnectPolicy::from_config(&config, true).enabled);
    }
}
