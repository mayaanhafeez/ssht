//! Connection multiplexing, and file transfer that rides the connection you
//! already have.
//!
//! When ssht connects it opens an ssh control master. Anything else pointed at
//! the same `ControlPath` — `scp` here — reuses that connection instead of
//! building a new one: no second authentication, no second TCP handshake, no
//! re-prompting for a vault password.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result};

use crate::config::Config;
use crate::vault::{self, LazyVault};

/// Directory holding control sockets (`~/.local/share/ssht/control`, honoring
/// `$XDG_DATA_HOME`), matching where the state database lives.
fn control_dir() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_DATA_HOME") {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => dirs::home_dir()
            .context("could not determine home directory")?
            .join(".local")
            .join("share"),
    };
    Ok(base.join("ssht").join("control"))
}

/// Create the control directory if needed and return the `ControlPath`
/// template. `%C` is ssh's hash of (host, port, user, proxy) — a fixed 32
/// characters, which matters because the whole path has to fit in a unix
/// socket address (104 bytes on macOS).
pub fn control_path() -> Result<PathBuf> {
    let dir = control_dir()?;
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

    // Control sockets grant access to an authenticated connection, so the
    // directory must not be readable by other users on the box.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("securing {}", dir.display()))?;
    }

    Ok(dir.join("cm-%C"))
}

/// The `-o` arguments that turn on multiplexing, or nothing if it's disabled.
pub fn ssh_args(config: &Config) -> Result<Vec<String>> {
    if !config.settings.multiplex {
        return Ok(Vec::new());
    }
    let path = control_path()?;
    Ok(vec![
        "-o".into(),
        "ControlMaster=auto".into(),
        "-o".into(),
        format!("ControlPath={}", path.display()),
        "-o".into(),
        format!("ControlPersist={}s", config.settings.control_persist_secs),
    ])
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

    cmd.args(ssh_args(config)?);
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
        assert!(ssh_args(&config).unwrap().is_empty());
    }
}
