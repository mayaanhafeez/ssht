//! ssht-specific TOML configuration at `~/.config/ssht/config.toml`.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Deserialize;

/// Top-level ssht configuration.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub settings: Settings,
    /// Per-host metadata keyed by ssh alias.
    pub hosts: HashMap<String, HostMeta>,
    /// Named tmux layouts.
    pub layouts: HashMap<String, Layout>,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Default tmux session name when a host doesn't override it.
    pub default_session: String,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            default_session: "main".to_string(),
        }
    }
}

/// ssht metadata for a single host.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct HostMeta {
    /// Override the tmux session name.
    pub session: Option<String>,
    /// Name of a layout (from `[layouts]`) to apply on first attach.
    pub layout: Option<String>,
    /// Free-form notes shown in the picker.
    pub notes: Option<String>,
    /// Local forwards, `ssh -L` syntax minus the flag
    /// (`[bind:]port:host:hostport`).
    pub local_forwards: Vec<String>,
    /// Remote forwards, `ssh -R` syntax minus the flag.
    pub remote_forwards: Vec<String>,
    /// Dynamic SOCKS forwards, `ssh -D` syntax minus the flag (`[bind:]port`).
    pub dynamic_forwards: Vec<String>,
}

/// A tmux layout: an ordered set of windows.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct Layout {
    pub windows: Vec<Window>,
}

/// A single tmux window within a layout.
#[derive(Debug, Clone, Deserialize)]
pub struct Window {
    pub name: String,
    /// Optional command to run in the window on creation.
    #[serde(default)]
    pub command: Option<String>,
}

/// Path to the ssht config file (`~/.config/ssht/config.toml`, honoring
/// `$XDG_CONFIG_HOME`). Uses XDG conventions on both Linux and macOS, as
/// specified, rather than the platform-native config dir.
pub fn config_path() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => dirs::home_dir()
            .context("could not determine home directory")?
            .join(".config"),
    };
    Ok(base.join("ssht").join("config.toml"))
}

impl Config {
    /// Load config from the default path. Missing file yields defaults.
    pub fn load() -> Result<Config> {
        let path = config_path()?;
        if !path.exists() {
            return Ok(Config::default());
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let config: Config = toml::from_str(&text)
            .with_context(|| format!("parsing config {}", path.display()))?;
        Ok(config)
    }

    /// Resolve the effective tmux session name for a host alias.
    pub fn session_for(&self, alias: &str) -> String {
        self.hosts
            .get(alias)
            .and_then(|m| m.session.clone())
            .unwrap_or_else(|| self.settings.default_session.clone())
    }

    /// Resolve the layout to use, given an explicit `--layout` override or the
    /// host's configured layout.
    pub fn resolve_layout(&self, alias: &str, override_name: Option<&str>) -> Option<&Layout> {
        let name = override_name
            .map(|s| s.to_string())
            .or_else(|| self.hosts.get(alias).and_then(|m| m.layout.clone()))?;
        self.layouts.get(&name)
    }

    /// Build the `ssh` arguments for a host's configured port forwards, plus
    /// any given on the command line. Because every reconnect re-invokes `ssh`
    /// with these same arguments, forwards come back with the session.
    pub fn forward_args(&self, alias: &str, extra: &Forwards) -> Result<Vec<String>> {
        let meta = self.hosts.get(alias);
        let mut args = Vec::new();

        let groups: [(&str, &[String], &[String]); 3] = [
            (
                "-L",
                meta.map(|m| m.local_forwards.as_slice()).unwrap_or(&[]),
                &extra.local,
            ),
            (
                "-R",
                meta.map(|m| m.remote_forwards.as_slice()).unwrap_or(&[]),
                &extra.remote,
            ),
            (
                "-D",
                meta.map(|m| m.dynamic_forwards.as_slice()).unwrap_or(&[]),
                &extra.dynamic,
            ),
        ];

        for (flag, configured, from_cli) in groups {
            for spec in configured.iter().chain(from_cli.iter()) {
                validate_forward(flag, spec)?;
                args.push(flag.to_string());
                args.push(spec.clone());
            }
        }

        Ok(args)
    }
}

/// Port forwards supplied on the command line, merged with configured ones.
#[derive(Debug, Default, Clone)]
pub struct Forwards {
    pub local: Vec<String>,
    pub remote: Vec<String>,
    pub dynamic: Vec<String>,
}

impl Forwards {
    pub fn is_empty(&self) -> bool {
        self.local.is_empty() && self.remote.is_empty() && self.dynamic.is_empty()
    }
}

/// Reject specs that `ssh` would misread. We deliberately don't parse the full
/// grammar — IPv6 brackets, unix-socket paths and `*` binds are all legal and
/// varied — but a spec containing whitespace would be split into separate argv
/// entries and silently mean something else, so that one is worth catching.
fn validate_forward(flag: &str, spec: &str) -> Result<()> {
    if spec.trim().is_empty() {
        anyhow::bail!("empty {flag} port forward in config");
    }
    if spec.split_whitespace().count() > 1 {
        anyhow::bail!(
            "{flag} port forward {spec:?} contains whitespace; write it as a \
             single spec such as \"8080:localhost:80\""
        );
    }
    Ok(())
}

/// Write a starter config file if one doesn't exist; return its path.
pub fn ensure_config_file() -> Result<PathBuf> {
    let path = config_path()?;
    if !path.exists() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(&path, STARTER_CONFIG)
            .with_context(|| format!("writing starter config {}", path.display()))?;
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with_forwards() -> Config {
        let mut config = Config::default();
        config.hosts.insert(
            "web".to_string(),
            HostMeta {
                local_forwards: vec!["8080:localhost:80".into()],
                remote_forwards: vec!["9000:localhost:9000".into()],
                dynamic_forwards: vec!["1080".into()],
                ..Default::default()
            },
        );
        config
    }

    #[test]
    fn builds_flags_for_each_forward_kind() {
        let config = config_with_forwards();
        let args = config.forward_args("web", &Forwards::default()).unwrap();
        assert_eq!(
            args,
            vec![
                "-L", "8080:localhost:80",
                "-R", "9000:localhost:9000",
                "-D", "1080",
            ]
        );
    }

    #[test]
    fn host_without_forwards_gets_none() {
        let config = config_with_forwards();
        assert!(config.forward_args("other", &Forwards::default()).unwrap().is_empty());
    }

    #[test]
    fn cli_forwards_append_to_configured_ones() {
        let config = config_with_forwards();
        let extra = Forwards {
            local: vec!["5432:db:5432".into()],
            ..Default::default()
        };
        let args = config.forward_args("web", &extra).unwrap();
        // Configured first, then the command-line addition, within -L.
        assert_eq!(&args[0..4], &["-L", "8080:localhost:80", "-L", "5432:db:5432"]);
    }

    #[test]
    fn cli_forwards_work_for_hosts_with_no_config_entry() {
        let config = Config::default();
        let extra = Forwards {
            dynamic: vec!["1080".into()],
            ..Default::default()
        };
        assert_eq!(
            config.forward_args("anything", &extra).unwrap(),
            vec!["-D", "1080"]
        );
    }

    #[test]
    fn whitespace_in_a_spec_is_rejected() {
        let mut config = Config::default();
        config.hosts.insert(
            "web".to_string(),
            HostMeta {
                // A plausible mistake: writing the flag into the value.
                local_forwards: vec!["-L 8080:localhost:80".into()],
                ..Default::default()
            },
        );
        let err = config
            .forward_args("web", &Forwards::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("whitespace"), "unexpected error: {err}");
    }

    #[test]
    fn empty_spec_is_rejected() {
        let mut config = Config::default();
        config.hosts.insert(
            "web".to_string(),
            HostMeta {
                remote_forwards: vec!["  ".into()],
                ..Default::default()
            },
        );
        assert!(config.forward_args("web", &Forwards::default()).is_err());
    }
}

const STARTER_CONFIG: &str = r#"# ssht configuration
# Docs: https://github.com/ (your repo)

[settings]
# Default tmux session name used when a host doesn't override it.
default_session = "main"

# Per-host metadata. Keys are ssh aliases (as in ~/.ssh/config).
# [hosts.prod-web]
# session = "web"
# layout = "dev"
# notes = "primary web server"
#
# Port forwards re-established on every connect and every reconnect.
# Same syntax as ssh's -L / -R / -D, minus the flag itself.
# local_forwards = ["8080:localhost:80"]
# remote_forwards = ["9000:localhost:9000"]
# dynamic_forwards = ["1080"]

# Named layouts applied on first attach.
# [[layouts.dev.windows]]
# name = "editor"
# command = "nvim"
#
# [[layouts.dev.windows]]
# name = "logs"
# command = "journalctl -f"
#
# [[layouts.dev.windows]]
# name = "shell"
"#;
