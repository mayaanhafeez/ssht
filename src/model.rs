//! Core data model: a unified `Host` view merged from all sources.

use std::collections::HashMap;

use crate::config::HostMeta;
use crate::state::HostState;

/// Where a host definition originated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostSource {
    /// Defined in an `~/.ssh/config` `Host` block.
    SshConfig,
    /// Discovered in `~/.ssh/known_hosts`.
    KnownHosts,
}

impl HostSource {
    pub fn label(self) -> &'static str {
        match self {
            HostSource::SshConfig => "ssh_config",
            HostSource::KnownHosts => "known_hosts",
        }
    }
}

/// A connectable host, merged from ssh config / known_hosts, ssht TOML metadata,
/// and the local state database.
#[derive(Debug, Clone)]
pub struct Host {
    /// The name passed to `ssh` (the `Host` alias, or a bare hostname).
    pub alias: String,
    /// Resolved `HostName`, if known and different from the alias.
    pub hostname: Option<String>,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub source: HostSource,
    /// ssht-specific metadata from `config.toml`.
    pub meta: HostMeta,
    /// Persisted connection state from SQLite.
    pub state: HostState,
}

impl Host {
    /// A short subtitle describing the resolved endpoint.
    pub fn endpoint(&self) -> String {
        let host = self.hostname.as_deref().unwrap_or(&self.alias);
        match (&self.user, self.port) {
            (Some(u), Some(p)) => format!("{u}@{host}:{p}"),
            (Some(u), None) => format!("{u}@{host}"),
            (None, Some(p)) => format!("{host}:{p}"),
            (None, None) => host.to_string(),
        }
    }

    /// Text used for fuzzy matching in the picker / autocomplete.
    pub fn haystack(&self) -> String {
        let mut s = self.alias.clone();
        if let Some(h) = &self.hostname {
            if h != &self.alias {
                s.push(' ');
                s.push_str(h);
            }
        }
        if let Some(n) = &self.meta.notes {
            s.push(' ');
            s.push_str(n);
        }
        s
    }
}

/// Merge hosts from discovery with TOML metadata and DB state into one list,
/// de-duplicated by alias (ssh config wins over known_hosts), sorted by alias.
pub fn merge_hosts(
    mut discovered: Vec<Host>,
    metas: &HashMap<String, HostMeta>,
    states: &HashMap<String, HostState>,
) -> Vec<Host> {
    // De-duplicate by alias, preferring ssh_config entries.
    let mut by_alias: HashMap<String, Host> = HashMap::new();
    for host in discovered.drain(..) {
        match by_alias.get(&host.alias) {
            Some(existing) if existing.source == HostSource::SshConfig => {}
            _ => {
                by_alias.insert(host.alias.clone(), host);
            }
        }
    }

    let mut out: Vec<Host> = by_alias
        .into_values()
        .map(|mut h| {
            if let Some(m) = metas.get(&h.alias) {
                h.meta = m.clone();
            }
            if let Some(s) = states.get(&h.alias) {
                h.state = s.clone();
            }
            h
        })
        .collect();

    out.sort_by(|a, b| a.alias.to_lowercase().cmp(&b.alias.to_lowercase()));
    out
}
