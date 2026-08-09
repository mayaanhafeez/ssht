//! Connection multiplexing, and file transfer that rides the connection you
//! already have.
//!
//! When ssht connects it opens an ssh control master. Anything else pointed at
//! the same `ControlPath` — `scp` here — reuses that connection instead of
//! building a new one: no second authentication, no second TCP handshake, no
//! re-prompting for a vault password.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

use crate::config::Config;
use crate::vault::{self, LazyVault};

/// Longest path that can be bound as a unix domain socket. macOS allows 104
/// bytes including the terminating NUL, Linux 108; take the smaller.
const MAX_SOCKET_PATH: usize = 103;

/// `%C` expands to a SHA-1 hex digest of (host, port, user, proxy): 40
/// characters.
const CONTROL_HASH_LEN: usize = 40;

/// While a master is starting, ssh binds the socket under a temporary name —
/// the ControlPath plus a dot and 16 random characters — and renames it into
/// place once the connection is up. The *temporary* name is what has to fit,
/// so budget for it.
const SSH_TEMP_SUFFIX_LEN: usize = 17;

/// Filename prefix inside the control directory.
const CONTROL_PREFIX: &str = "cm-";

/// Whether sockets under `dir` will fit in a unix socket address once ssh has
/// expanded `%C` and added its temporary suffix.
fn dir_fits(dir: &Path) -> bool {
    // dir + "/" + "cm-" + hash + ".<16 random>"
    dir.as_os_str().len() + 1 + CONTROL_PREFIX.len() + CONTROL_HASH_LEN + SSH_TEMP_SUFFIX_LEN
        <= MAX_SOCKET_PATH
}

/// Candidate directories for control sockets, best first.
///
/// The XDG data directory is preferred because it's where the rest of ssht's
/// state lives — but on macOS `~/.local/share/ssht/control` is already 44
/// characters, and with the expansions above that lands two bytes over the
/// limit. So there are shorter fallbacks, all still inside the user's own
/// tree (never `/tmp`, where another user could pre-create the directory and
/// deny us the socket).
fn candidate_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();

    let data_base = match std::env::var_os("XDG_DATA_HOME") {
        Some(v) if !v.is_empty() => Some(PathBuf::from(v)),
        _ => dirs::home_dir().map(|h| h.join(".local").join("share")),
    };
    if let Some(base) = data_base {
        dirs.push(base.join("ssht").join("control"));
    }

    // Linux: /run/user/<uid>/ssht — short, tmpfs, cleaned on logout.
    if let Some(v) = std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
        dirs.push(PathBuf::from(v).join("ssht"));
    }

    // Last resort: a short dedicated directory in the home directory.
    if let Some(home) = dirs::home_dir() {
        dirs.push(home.join(".ssht"));
    }

    dirs
}

/// Create the control directory and return the `ControlPath` template, or
/// `None` if no candidate directory yields a short enough path.
///
/// Returning `None` rather than erroring is deliberate: multiplexing is an
/// optimisation, and failing to set it up must never be the reason a
/// connection doesn't happen.
pub fn control_path() -> Option<PathBuf> {
    for dir in candidate_dirs() {
        if !dir_fits(&dir) {
            continue;
        }
        if std::fs::create_dir_all(&dir).is_err() {
            continue;
        }

        // Control sockets grant access to an authenticated connection, so the
        // directory must not be reachable by other users on the box.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).is_err() {
                continue;
            }
        }

        return Some(dir.join(format!("{CONTROL_PREFIX}%C")));
    }
    None
}

/// The `-o` arguments that turn on multiplexing, or nothing if it's disabled
/// or unavailable.
pub fn ssh_args(config: &Config) -> Vec<String> {
    if !config.settings.multiplex {
        return Vec::new();
    }
    let Some(path) = control_path() else {
        // Don't fail the connection over an optimisation; say so once and
        // carry on without it.
        eprintln!(
            "ssht: no control socket path short enough for this system; \
             continuing without connection multiplexing"
        );
        return Vec::new();
    };
    vec![
        "-o".into(),
        "ControlMaster=auto".into(),
        "-o".into(),
        format!("ControlPath={}", path.display()),
        "-o".into(),
        format!("ControlPersist={}s", config.settings.control_persist_secs),
    ]
}

/// One side of a transfer: a local path, or a path on a remote host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    Local(String),
    Remote { host: String, path: String },
}

/// Split `host:path` from a plain local path, using scp's own rule: a colon
/// only introduces a host if no `/` appears before it. That keeps relative
/// paths containing a colon (`./weird:name`) local, as the user meant.
pub fn parse_endpoint(spec: &str) -> Endpoint {
    match spec.find(':') {
        Some(idx) if !spec[..idx].contains('/') && idx > 0 => Endpoint::Remote {
            host: spec[..idx].to_string(),
            path: spec[idx + 1..].to_string(),
        },
        _ => Endpoint::Local(spec.to_string()),
    }
}

/// Copy between local and remote, riding the multiplexed connection.
///
/// Exactly one endpoint may be remote. Remote-to-remote would mean brokering
/// between two hosts, which is a different feature; local-to-local is `cp`.
pub fn copy(
    source: &str,
    dest: &str,
    recursive: bool,
    config: &Config,
    vault: &mut LazyVault,
) -> Result<()> {
    let src = parse_endpoint(source);
    let dst = parse_endpoint(dest);

    let alias = match (&src, &dst) {
        (Endpoint::Remote { host, .. }, Endpoint::Local(_)) => host.clone(),
        (Endpoint::Local(_), Endpoint::Remote { host, .. }) => host.clone(),
        (Endpoint::Remote { .. }, Endpoint::Remote { .. }) => {
            anyhow::bail!("both paths are remote; ssht cp copies between local and one host")
        }
        (Endpoint::Local(_), Endpoint::Local(_)) => {
            anyhow::bail!("neither path names a host; use cp for local copies")
        }
    };

    if vault.might_have_settings(&alias)? {
        vault.ensure_unlocked().context("unlocking vault")?;
    }
    let settings = vault.get_settings(&alias)?;

    let mut cmd = Command::new("scp");

    let _askpass_cleanup = settings
        .as_ref()
        .and_then(|s| s.password.as_deref())
        .map(|password| vault::setup_ssh_askpass(&mut cmd, password))
        .transpose()?;

    cmd.args(ssh_args(config));
    if recursive {
        cmd.arg("-r");
    }

    // Vault overrides rewrite the remote side: the alias the user typed may not
    // be resolvable by ssh at all if the real address lives in the vault.
    let user = settings.as_ref().and_then(|s| s.username.as_deref());
    let address = settings.as_ref().and_then(|s| s.address.as_deref());
    cmd.arg(render_endpoint(&src, &alias, user, address));
    cmd.arg(render_endpoint(&dst, &alias, user, address));

    let status = cmd
        .status()
        .with_context(|| "launching scp (is it installed?)".to_string())?;

    if !status.success() {
        anyhow::bail!("scp failed copying {source} to {dest}");
    }
    Ok(())
}

/// Render an endpoint back into an scp argument, substituting the vault's
/// address and username for the alias where present.
fn render_endpoint(
    endpoint: &Endpoint,
    alias: &str,
    user: Option<&str>,
    address: Option<&str>,
) -> String {
    match endpoint {
        Endpoint::Local(path) => path.clone(),
        Endpoint::Remote { host, path } => {
            // Only rewrite the host this transfer actually resolved.
            let target = if host == alias {
                address.unwrap_or(host)
            } else {
                host
            };
            match user {
                Some(u) => format!("{u}@{target}:{path}"),
                None => format!("{target}:{path}"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_host_from_path() {
        assert_eq!(
            parse_endpoint("web:/var/log/app.log"),
            Endpoint::Remote {
                host: "web".into(),
                path: "/var/log/app.log".into()
            }
        );
    }

    #[test]
    fn plain_path_is_local() {
        assert_eq!(parse_endpoint("./file"), Endpoint::Local("./file".into()));
        assert_eq!(parse_endpoint("/tmp/x"), Endpoint::Local("/tmp/x".into()));
    }

    #[test]
    fn slash_before_colon_stays_local() {
        // A local file that happens to contain a colon must not look remote.
        assert_eq!(
            parse_endpoint("./notes/2026:draft.md"),
            Endpoint::Local("./notes/2026:draft.md".into())
        );
    }

    #[test]
    fn leading_colon_is_local() {
        assert_eq!(parse_endpoint(":weird"), Endpoint::Local(":weird".into()));
    }

    #[test]
    fn empty_remote_path_means_home() {
        assert_eq!(
            parse_endpoint("web:"),
            Endpoint::Remote { host: "web".into(), path: String::new() }
        );
    }

    #[test]
    fn renders_local_unchanged() {
        let e = Endpoint::Local("./f".into());
        assert_eq!(render_endpoint(&e, "web", None, None), "./f");
    }

    #[test]
    fn renders_remote_with_vault_overrides() {
        let e = Endpoint::Remote { host: "web".into(), path: "/tmp/f".into() };
        assert_eq!(render_endpoint(&e, "web", None, None), "web:/tmp/f");
        assert_eq!(
            render_endpoint(&e, "web", Some("deploy"), Some("10.0.0.4")),
            "deploy@10.0.0.4:/tmp/f"
        );
    }

    #[test]
    fn multiplex_can_be_turned_off() {
        let mut config = Config::default();
        config.settings.multiplex = false;
        assert!(ssh_args(&config).is_empty());
    }

    #[test]
    fn rejects_the_directory_that_actually_broke() {
        // The real failure, verbatim from the bug report. The directory is 44
        // characters; once ssh expands %C to a 40-char hash and adds its
        // 17-char temporary suffix the socket path is 105 bytes, past the
        // macOS limit of 103.
        let dir = Path::new("/Users/ayaanhafeez/.local/share/ssht/control");
        assert_eq!(dir.as_os_str().len(), 44);
        assert_eq!(
            dir.as_os_str().len() + 1 + CONTROL_PREFIX.len() + CONTROL_HASH_LEN
                + SSH_TEMP_SUFFIX_LEN,
            105
        );
        assert!(!dir_fits(dir));
    }

    #[test]
    fn accepts_a_short_enough_directory() {
        assert!(dir_fits(Path::new("/Users/ayaanhafeez/.ssht")));
        assert!(dir_fits(Path::new("/run/user/1000/ssht")));
        assert!(dir_fits(Path::new("/home/bob/.local/share/ssht/control")));
    }

    #[test]
    fn boundary_is_exactly_the_socket_limit() {
        // Longest directory that still fits, and one character more.
        let max_dir_len = MAX_SOCKET_PATH - 1 - CONTROL_PREFIX.len() - CONTROL_HASH_LEN - SSH_TEMP_SUFFIX_LEN;
        let ok = PathBuf::from("/".repeat(1) + &"a".repeat(max_dir_len - 1));
        assert_eq!(ok.as_os_str().len(), max_dir_len);
        assert!(dir_fits(&ok));

        let too_long = PathBuf::from("/".repeat(1) + &"a".repeat(max_dir_len));
        assert!(!dir_fits(&too_long));
    }

    #[test]
    fn always_offers_a_short_candidate() {
        // Whatever the platform, at least one candidate must be usable --
        // otherwise multiplexing silently never works.
        assert!(candidate_dirs().iter().any(|d| dir_fits(d)));
    }
}
