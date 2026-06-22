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
