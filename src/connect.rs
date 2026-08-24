//! Performing the actual SSH + tmux connection by shelling out to `ssh`.

#[cfg(unix)]
use std::fs::File;
#[cfg(unix)]
use std::io::{ErrorKind, IsTerminal, Read, Write};
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
#[cfg(unix)]
use std::process::{Child, Stdio};
use std::process::{Command, ExitStatus};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::config::{Config, Forwards};
use crate::local_echo::LocalEcho;
use crate::mux;
use crate::state::State;
use crate::terminfo;
use crate::tmux;
use crate::vault::{self, LazyVault};

/// The exit code `ssh` uses for its own failures — connection refused, host
/// unreachable, auth rejected, connection dropped mid-session. Anything else is
/// the *remote command's* exit status, which must not be treated as a transport
/// problem.
const SSH_FAILURE: i32 = 255;
const SERVER_ALIVE_INTERVAL_SECS: u32 = 15;
const SERVER_ALIVE_COUNT_MAX: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConnectionOutcome {
    Complete,
    Reconnect,
    RemoteFailure,
}

fn connection_outcome(status: ExitStatus, read_only: bool) -> ConnectionOutcome {
    if status.success() || (!read_only && status.code() != Some(SSH_FAILURE)) {
        ConnectionOutcome::Complete
    } else if status.code() == Some(SSH_FAILURE) {
        ConnectionOutcome::Reconnect
    } else {
        ConnectionOutcome::RemoteFailure
    }
}

const MOSH_SERVER_BOOTSTRAP: &str = "\
if command -v mosh-server >/dev/null 2>&1; then exit 0; fi; \
printf 'ssht: mosh-server not found; installing mosh...\\n' >&2; \
if command -v brew >/dev/null 2>&1; then brew install mosh; exit $?; fi; \
if [ \"$(id -u)\" -eq 0 ]; then SUDO=; \
elif command -v sudo >/dev/null 2>&1; then SUDO=sudo; \
else printf 'ssht: installing mosh requires root or sudo\\n' >&2; exit 1; fi; \
if command -v apt-get >/dev/null 2>&1; then $SUDO apt-get update && $SUDO apt-get install -y mosh; \
elif command -v dnf >/dev/null 2>&1; then $SUDO dnf install -y mosh; \
elif command -v yum >/dev/null 2>&1; then $SUDO yum install -y mosh; \
elif command -v pacman >/dev/null 2>&1; then $SUDO pacman -Sy --noconfirm mosh; \
elif command -v apk >/dev/null 2>&1; then $SUDO apk add mosh; \
elif command -v zypper >/dev/null 2>&1; then $SUDO zypper --non-interactive install mosh; \
else printf 'ssht: no supported package manager found for installing mosh\\n' >&2; exit 1; fi; \
command -v mosh-server >/dev/null 2>&1";

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

enum SshRun {
    Exited(ExitStatus),
    FailedAfterSpawn(anyhow::Error),
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
#[derive(Debug, Clone)]
pub struct ConnectOptions {
    /// Edit ordinary shell lines locally, outside the alternate screen.
    pub local_echo: bool,
    /// Use Mosh instead of SSH for the interactive terminal transport.
    pub mosh: bool,
    /// Exit when the transport drops instead of re-attaching.
    pub no_reconnect: bool,
    /// Port forwards from the command line, merged with configured ones.
    pub forwards: Forwards,
    /// Attach as a viewer: mirror the session but send it no input.
    pub read_only: bool,
    /// Lines of history to replay on attach, overriding the configured value.
    pub scrollback: Option<u32>,
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

    let configured_scrollback = opts.scrollback.unwrap_or(config.settings.scrollback_lines);
    let scrollback_lines = if opts.mosh { 0 } else { configured_scrollback };
    let policy = ReconnectPolicy::from_config(config, opts.no_reconnect);
    let forward_args = config.forward_args(alias, &opts.forwards)?;
    let mux_args = mux::ssh_args(config);

    // Replay history on the first attach only. On a reconnect the local
    // terminal still holds everything printed before the drop, so replaying
    // again would just duplicate it -- and with an aggressive retry loop that
    // duplication compounds on every attempt.
    let local_echo = local_echo_available(opts.local_echo && !opts.read_only && !opts.mosh);
    let first_attach = tmux::build_remote_command(
        &session,
        layout,
        opts.read_only,
        scrollback_lines,
        local_echo,
    );
    let reattach = tmux::build_remote_command(&session, layout, opts.read_only, 0, local_echo);
    let first_attach = if local_echo {
        tmux::with_alt_screen_watcher(&session, &first_attach)
    } else {
        first_attach
    };
    let reattach = if local_echo {
        tmux::with_alt_screen_watcher(&session, &reattach)
    } else {
        reattach
    };

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
    let term_override = terminfo::ensure_remote(
        &target,
        username.as_deref(),
        password.as_deref(),
        &mux_args,
        ssh_passthrough,
    )?;
    let first_attach = terminfo::apply_override(&first_attach, term_override.as_deref());
    let reattach = terminfo::apply_override(&reattach, term_override.as_deref());

    if opts.mosh {
        if !forward_args.is_empty() {
            anyhow::bail!(
                "--mosh does not support SSH port forwarding; remove configured and command-line -L/-R/-D forwards"
            );
        }
        if configured_scrollback > 0 {
            eprintln!("ssht: --mosh disables tmux scrollback replay");
        }
        ensure_remote_mosh(
            &target,
            username.as_deref(),
            password.as_deref(),
            &mux_args,
            ssh_passthrough,
        )?;
        let status = run_mosh(
            &target,
            username.as_deref(),
            password.as_deref(),
            &mux_args,
            ssh_passthrough,
            &first_attach,
        )?;
        if !status.success() {
            anyhow::bail!("mosh connection to {alias} failed with {status}");
        }
        return Ok(());
    }

    supervise_ssh(
        alias,
        policy,
        opts.read_only,
        |first| {
            let started = Instant::now();
            let remote = if first { &first_attach } else { &reattach };
            let outcome = run_ssh(
                &target,
                username.as_deref(),
                password.as_deref(),
                &mux_args,
                &forward_args,
                ssh_passthrough,
                remote,
                local_echo,
            )?;
            Ok((outcome, started.elapsed()))
        },
        std::thread::sleep,
    )
}

fn supervise_ssh<R, S>(
    alias: &str,
    policy: ReconnectPolicy,
    read_only: bool,
    mut run: R,
    mut sleep: S,
) -> Result<()>
where
    R: FnMut(bool) -> Result<(SshRun, Duration)>,
    S: FnMut(Duration),
{
    let mut attempt: u32 = 0;
    let mut first = true;
    loop {
        let (outcome, elapsed) = run(first)?;
        let post_spawn_error = match outcome {
            SshRun::Exited(status) => match connection_outcome(status, read_only) {
                // Interactive shell exit codes are not actionable, but read-only
                // mode uses a non-zero status to report a missing target session.
                ConnectionOutcome::Complete => return Ok(()),
                ConnectionOutcome::RemoteFailure => {
                    anyhow::bail!("read-only connection to {alias} failed with {status}");
                }
                ConnectionOutcome::Reconnect => None,
            },
            SshRun::FailedAfterSpawn(error) => Some(error),
        };

        if !policy.enabled {
            if let Some(error) = post_spawn_error {
                return Err(error).context(format!("ssh connection to {alias} failed"));
            }
            anyhow::bail!("ssh connection to {alias} failed");
        }

        // A session that ran for a while and then dropped earns a fresh budget.
        if elapsed >= ESTABLISHED_AFTER {
            attempt = 0;
        }

        attempt += 1;
        if attempt > policy.max_attempts {
            if let Some(error) = post_spawn_error {
                return Err(error).context(format!(
                    "ssh connection to {alias} failed after {} reconnect attempts",
                    policy.max_attempts
                ));
            }
            anyhow::bail!(
                "ssh connection to {alias} failed after {} reconnect attempts",
                policy.max_attempts
            );
        }

        first = false;
        let delay = policy.delay_for(attempt);
        eprintln!(
            "Connection to {alias} lost — reconnecting in {}s (attempt {}/{})…",
            delay.as_secs(),
            attempt,
            policy.max_attempts
        );
        sleep(delay);
    }
}

fn ensure_remote_mosh(
    target: &str,
    username: Option<&str>,
    password: Option<&str>,
    mux_args: &[String],
    ssh_passthrough: &[String],
) -> Result<()> {
    let mut cmd = Command::new("ssh");
    // A PTY lets sudo prompt when installation is required.
    cmd.arg("-t");
    cmd.args(mux_args);
    cmd.args(ssh_passthrough);
    if let Some(user) = username {
        cmd.args(["-l", user]);
    }
    cmd.arg(target);
    cmd.arg(MOSH_SERVER_BOOTSTRAP);

    let _askpass_cleanup = password
        .map(|password| vault::setup_ssh_askpass(&mut cmd, password))
        .transpose()?;
    let status = cmd
        .status()
        .with_context(|| format!("checking mosh-server on {target}"))?;
    if !status.success() {
        anyhow::bail!("could not install mosh-server on {target}");
    }
    Ok(())
}

fn run_mosh(
    target: &str,
    username: Option<&str>,
    password: Option<&str>,
    mux_args: &[String],
    ssh_passthrough: &[String],
    remote: &str,
) -> Result<ExitStatus> {
    let ssh_command = mosh_ssh_command(username, mux_args, ssh_passthrough);
    let mut cmd = Command::new("mosh");
    cmd.arg(format!("--ssh={ssh_command}"));
    cmd.arg("--");
    cmd.arg(target);
    // The tmux bootstrap is a compound shell command, not one executable.
    cmd.args(["sh", "-lc", remote]);

    let _askpass_cleanup = password
        .map(|password| vault::setup_ssh_askpass(&mut cmd, password))
        .transpose()?;

    cmd.status()
        .with_context(|| "launching mosh (is mosh installed locally?)")
}

fn mosh_ssh_command(
    username: Option<&str>,
    mux_args: &[String],
    ssh_passthrough: &[String],
) -> String {
    let mut parts = vec!["ssh".to_string()];
    parts.extend_from_slice(mux_args);
    parts.extend_from_slice(ssh_passthrough);
    if let Some(user) = username {
        parts.push("-l".to_string());
        parts.push(user.to_string());
    }
    parts
        .iter()
        .map(|part| tmux::sh_quote(part))
        .collect::<Vec<_>>()
        .join(" ")
}

fn local_echo_available(requested: bool) -> bool {
    #[cfg(unix)]
    {
        requested && std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
    }
    #[cfg(not(unix))]
    {
        let _ = requested;
        false
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
    local_echo: bool,
) -> Result<SshRun> {
    let mut cmd = ssh_command(
        target,
        username,
        mux_args,
        forward_args,
        ssh_passthrough,
        remote,
    );

    let _askpass_cleanup = password
        .map(|password| vault::setup_ssh_askpass(&mut cmd, password))
        .transpose()?;

    #[cfg(unix)]
    if local_echo && std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        return run_with_local_echo(&mut cmd)
            .with_context(|| format!("launching ssh to {target} (is ssh installed?)"));
    }

    #[cfg(not(unix))]
    let _ = local_echo;

    cmd.status()
        .map(SshRun::Exited)
        .with_context(|| format!("launching ssh to {target} (is ssh installed?)"))
}

fn ssh_command(
    target: &str,
    username: Option<&str>,
    mux_args: &[String],
    forward_args: &[String],
    ssh_passthrough: &[String],
    remote: &str,
) -> Command {
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
    // Detect half-open links so ssh exits and the reconnect loop can take over.
    // User passthrough follows these defaults and can override either option.
    cmd.args([
        "-o",
        &format!("ServerAliveInterval={SERVER_ALIVE_INTERVAL_SECS}"),
        "-o",
        &format!("ServerAliveCountMax={SERVER_ALIVE_COUNT_MAX}"),
    ]);
    cmd.args(ssh_passthrough);

    if let Some(user) = username {
        cmd.arg("-l");
        cmd.arg(user);
    }

    cmd.arg(target);
    cmd.arg(remote);
    cmd
}

/// Run ssh behind a local PTY so its terminal behavior remains unchanged while
/// ssht relays and edits the byte stream.
#[cfg(unix)]
fn run_with_local_echo(cmd: &mut Command) -> Result<SshRun> {
    let mut winsize = terminal_winsize();
    let mut master_fd = -1;
    let mut slave_fd = -1;
    let rc = unsafe {
        libc::openpty(
            &mut master_fd,
            &mut slave_fd,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            winsize
                .as_mut()
                .map_or(std::ptr::null_mut(), |size| size as *mut libc::winsize),
        )
    };
    if rc == -1 {
        return Err(std::io::Error::last_os_error()).context("opening local PTY");
    }

    let mut master = unsafe { File::from_raw_fd(master_fd) };
    let slave = unsafe { File::from_raw_fd(slave_fd) };
    cmd.stdin(Stdio::from(slave.try_clone()?));
    cmd.stdout(Stdio::from(slave.try_clone()?));
    cmd.stderr(Stdio::from(slave));
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::ioctl(libc::STDIN_FILENO, libc::TIOCSCTTY as _, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let _raw = RawTerminal::new()?;
    let mut child = ChildGuard::new(cmd.spawn()?);
    run_local_echo_relay(&mut child, &mut master, winsize)
        .map(SshRun::Exited)
        .or_else(|error| Ok(SshRun::FailedAfterSpawn(error)))
}

#[cfg(unix)]
fn run_local_echo_relay(
    child: &mut ChildGuard,
    master: &mut File,
    winsize: Option<libc::winsize>,
) -> Result<ExitStatus> {
    let stdin = std::io::stdin();
    let mut stdin_lock = stdin.lock();
    let stdout = std::io::stdout();
    let mut stdout_lock = stdout.lock();
    let mut editor = LocalEcho::new(true);
    let mut last_size = winsize.map(|s| (s.ws_row, s.ws_col));
    let mut input_open = true;
    let mut buf = [0u8; 65_536];

    'relay: loop {
        let mut fds = [
            libc::pollfd {
                fd: if input_open { stdin.as_raw_fd() } else { -1 },
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: master.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, 100) };
        if ready < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == ErrorKind::Interrupted {
                continue;
            }
            return Err(err).context("polling SSH PTY");
        }

        if let Some(size) = terminal_winsize() {
            let dimensions = (size.ws_row, size.ws_col);
            if Some(dimensions) != last_size {
                unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ, &size) };
                unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGWINCH) };
                last_size = Some(dimensions);
            }
        }

        if fds[0].revents & libc::POLLIN != 0 {
            let n = stdin_lock.read(&mut buf)?;
            if n == 0 {
                input_open = false;
                master.write_all(&[0x04])?;
            } else {
                let action = editor.on_input(&buf[..n]);
                if !write_pty(master, &action.to_remote)? {
                    break 'relay;
                }
                stdout_lock.write_all(&action.to_terminal)?;
                stdout_lock.flush()?;
            }
        }

        if fds[1].revents & (libc::POLLIN | libc::POLLHUP) != 0 {
            match master.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let action = editor.on_output(&buf[..n]);
                    if !write_pty(master, &action.to_remote)? {
                        break 'relay;
                    }
                    stdout_lock.write_all(&action.to_terminal)?;
                    stdout_lock.flush()?;
                }
                Err(err) if err.raw_os_error() == Some(libc::EIO) => break,
                Err(err) => return Err(err).context("reading SSH PTY"),
            }
        }

        if fds[1].revents & libc::POLLNVAL != 0 {
            break;
        }
    }

    child.wait().context("waiting for ssh")
}

#[cfg(unix)]
fn write_pty(master: &mut File, data: &[u8]) -> std::io::Result<bool> {
    match master.write_all(data) {
        Ok(()) => Ok(true),
        Err(err)
            if err.kind() == ErrorKind::BrokenPipe || err.raw_os_error() == Some(libc::EIO) =>
        {
            Ok(false)
        }
        Err(err) => Err(err),
    }
}

#[cfg(unix)]
fn terminal_winsize() -> Option<libc::winsize> {
    let mut size = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let rc = unsafe { libc::ioctl(std::io::stdout().as_raw_fd(), libc::TIOCGWINSZ, &mut size) };
    (rc == 0).then_some(size)
}

#[cfg(unix)]
struct RawTerminal;

#[cfg(unix)]
impl RawTerminal {
    fn new() -> Result<Self> {
        crossterm::terminal::enable_raw_mode().context("enabling terminal raw mode")?;
        Ok(Self)
    }
}

#[cfg(unix)]
impl Drop for RawTerminal {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

#[cfg(unix)]
struct ChildGuard {
    child: Child,
    completed: bool,
}

#[cfg(unix)]
impl ChildGuard {
    fn new(child: Child) -> Self {
        Self {
            child,
            completed: false,
        }
    }

    fn wait(&mut self) -> std::io::Result<ExitStatus> {
        let status = self.child.wait()?;
        self.completed = true;
        Ok(status)
    }

    fn id(&self) -> u32 {
        self.child.id()
    }
}

#[cfg(unix)]
impl Drop for ChildGuard {
    fn drop(&mut self) {
        if !self.completed {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn exit_status(code: i32) -> ExitStatus {
        use std::os::unix::process::ExitStatusExt;

        ExitStatus::from_raw(code << 8)
    }

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
    fn reattach_command_drops_the_scrollback_replay() {
        // Integration invariant: the first attach replays history, a reconnect
        // must not -- the local terminal still holds everything from before the
        // drop, so replaying again would duplicate it once per retry.
        let first = tmux::build_remote_command("main", None, false, 500, false);
        let reattach = tmux::build_remote_command("main", None, false, 0, false);
        assert!(first.contains("capture-pane"));
        assert!(!reattach.contains("capture-pane"));
    }

    #[test]
    fn flag_overrides_config_default() {
        let config = Config::default();
        assert!(ReconnectPolicy::from_config(&config, false).enabled);
        assert!(!ReconnectPolicy::from_config(&config, true).enabled);
    }

    #[cfg(unix)]
    #[test]
    fn retries_transport_failures_with_backoff() {
        let mut outcomes = vec![0, SSH_FAILURE, SSH_FAILURE].into_iter().rev();
        let mut first_flags = Vec::new();
        let mut delays = Vec::new();
        supervise_ssh(
            "host",
            policy(),
            false,
            |first| {
                first_flags.push(first);
                Ok((
                    SshRun::Exited(exit_status(outcomes.next().unwrap())),
                    Duration::ZERO,
                ))
            },
            |delay| delays.push(delay),
        )
        .unwrap();
        assert_eq!(first_flags, [true, false, false]);
        assert_eq!(delays, [Duration::from_secs(1), Duration::from_secs(2)]);
    }

    #[cfg(unix)]
    #[test]
    fn interactive_remote_failure_completes_the_session() {
        assert_eq!(
            connection_outcome(exit_status(1), false),
            ConnectionOutcome::Complete
        );
    }

    #[cfg(unix)]
    #[test]
    fn retries_failures_after_ssh_has_spawned() {
        let mut calls = 0;
        let mut delays = Vec::new();
        supervise_ssh(
            "host",
            policy(),
            false,
            |_| {
                calls += 1;
                let outcome = if calls == 1 {
                    SshRun::FailedAfterSpawn(anyhow::anyhow!("relay failed"))
                } else {
                    SshRun::Exited(exit_status(0))
                };
                Ok((outcome, Duration::ZERO))
            },
            |delay| delays.push(delay),
        )
        .unwrap();
        assert_eq!(calls, 2);
        assert_eq!(delays, [Duration::from_secs(1)]);
    }

    #[test]
    fn pre_spawn_errors_are_not_retried() {
        let mut calls = 0;
        let mut delays = Vec::new();
        let error = supervise_ssh(
            "host",
            policy(),
            false,
            |_| {
                calls += 1;
                anyhow::bail!("ssh executable not found")
            },
            |delay| delays.push(delay),
        )
        .unwrap_err();
        assert_eq!(calls, 1);
        assert!(delays.is_empty());
        assert!(error.to_string().contains("ssh executable not found"));
    }

    #[test]
    fn reconnect_limit_preserves_post_spawn_error() {
        let mut p = policy();
        p.max_attempts = 2;
        let mut calls = 0;
        let mut delays = Vec::new();
        let error = supervise_ssh(
            "host",
            p,
            false,
            |_| {
                calls += 1;
                Ok((
                    SshRun::FailedAfterSpawn(anyhow::anyhow!("relay failed")),
                    Duration::ZERO,
                ))
            },
            |delay| delays.push(delay),
        )
        .unwrap_err();
        assert_eq!(calls, 3);
        assert_eq!(delays, [Duration::from_secs(1), Duration::from_secs(2)]);
        assert!(error
            .to_string()
            .contains("failed after 2 reconnect attempts"));
        assert!(format!("{error:#}").contains("relay failed"));
    }

    #[cfg(unix)]
    #[test]
    fn read_only_remote_failure_is_preserved() {
        assert_eq!(
            connection_outcome(exit_status(1), true),
            ConnectionOutcome::RemoteFailure
        );
    }

    #[cfg(unix)]
    #[test]
    fn ssh_failure_reconnects_in_both_modes() {
        assert_eq!(
            connection_outcome(exit_status(SSH_FAILURE), false),
            ConnectionOutcome::Reconnect
        );
        assert_eq!(
            connection_outcome(exit_status(SSH_FAILURE), true),
            ConnectionOutcome::Reconnect
        );
    }

    #[test]
    fn mosh_ssh_command_preserves_argument_boundaries() {
        let mux = vec!["-o".into(), "ProxyCommand=jump host".into()];
        let passthrough = vec!["-p".into(), "2222".into()];
        assert_eq!(
            mosh_ssh_command(Some("dev user"), &mux, &passthrough),
            "'ssh' '-o' 'ProxyCommand=jump host' '-p' '2222' '-l' 'dev user'"
        );
    }

    #[test]
    fn interactive_ssh_uses_heartbeat_defaults() {
        let cmd = ssh_command("host", None, &[], &[], &[], "remote");
        let args: Vec<_> = cmd
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(args
            .windows(2)
            .any(|pair| pair == ["-o", "ServerAliveInterval=15"]));
        assert!(args
            .windows(2)
            .any(|pair| pair == ["-o", "ServerAliveCountMax=3"]));
    }

    #[test]
    fn passthrough_can_override_heartbeat_defaults() {
        let passthrough = vec!["-o".into(), "ServerAliveInterval=5".into()];
        let cmd = ssh_command("host", None, &[], &[], &passthrough, "remote");
        let args: Vec<_> = cmd
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        let default = args
            .iter()
            .position(|arg| arg == "ServerAliveInterval=15")
            .unwrap();
        let override_arg = args
            .iter()
            .position(|arg| arg == "ServerAliveInterval=5")
            .unwrap();
        assert!(default < override_arg);
    }

    #[test]
    fn interactive_ssh_command_preserves_all_arguments() {
        let mux = vec!["-o".into(), "ControlMaster=auto".into()];
        let forwards = vec!["-L".into(), "8080:localhost:80".into()];
        let passthrough = vec!["-p".into(), "2222".into()];
        let cmd = ssh_command(
            "example.com",
            Some("deploy"),
            &mux,
            &forwards,
            &passthrough,
            "tmux attach",
        );
        let args: Vec<_> = cmd
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args.first().map(String::as_str), Some("-t"));
        assert!(args
            .windows(2)
            .any(|pair| pair == ["-L", "8080:localhost:80"]));
        assert!(args.windows(2).any(|pair| pair == ["-p", "2222"]));
        assert!(args.windows(2).any(|pair| pair == ["-l", "deploy"]));
        assert_eq!(args[args.len() - 2..], ["example.com", "tmux attach"]);
    }

    #[test]
    fn mosh_server_bootstrap_is_valid_shell() {
        #[cfg(unix)]
        assert!(Command::new("sh")
            .args(["-n", "-c", MOSH_SERVER_BOOTSTRAP])
            .status()
            .expect("run shell syntax check")
            .success());
    }
}
