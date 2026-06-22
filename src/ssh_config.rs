//! Discovery of hosts from `~/.ssh/config` (with `Include`/`Match`/wildcards)
//! and a fallback scan of `~/.ssh/known_hosts`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::config::HostMeta;
use crate::model::{Host, HostSource};
use crate::state::HostState;

/// Discover all hosts from ssh config and known_hosts.
pub fn discover() -> Result<Vec<Host>> {
    let mut hosts = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    if let Some(home) = dirs::home_dir() {
        let cfg = home.join(".ssh").join("config");
        if cfg.exists() {
            let mut visited = HashSet::new();
            parse_config_file(&cfg, &mut hosts, &mut seen, &mut visited);
        }

        let known = home.join(".ssh").join("known_hosts");
        if known.exists() {
            parse_known_hosts(&known, &mut hosts, &mut seen);
        }
    }

    Ok(hosts)
}

/// Construct a bare host placeholder; metadata/state are filled in later.
fn new_host(alias: String, source: HostSource) -> Host {
    Host {
        alias,
        hostname: None,
        user: None,
        port: None,
        source,
        meta: HostMeta::default(),
        state: HostState::default(),
    }
}

/// True if a `Host` pattern is a literal alias (no wildcards / negation).
fn is_literal(pattern: &str) -> bool {
    !pattern.contains('*') && !pattern.contains('?') && !pattern.starts_with('!')
}

/// Parse one ssh config file, recursively following `Include` directives.
fn parse_config_file(
    path: &Path,
    hosts: &mut Vec<Host>,
    seen: &mut HashSet<String>,
    visited: &mut HashSet<PathBuf>,
) {
    // Guard against include cycles.
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if !visited.insert(canonical) {
        return;
    }

    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => return,
    };

    // Indices into `hosts` for the aliases declared by the current `Host` block,
    // so subsequent option lines (HostName/User/Port) can be attached to them.
    let mut current: Vec<usize> = Vec::new();
    // When inside a `Match` block we still want to capture HostName etc. for any
    // literal alias mentioned, but Match blocks rarely declare new aliases.
    let mut in_match = false;

    for raw in text.lines() {
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }

        let (key, value) = match split_kv(line) {
            Some(kv) => kv,
            None => continue,
        };
        let key_lc = key.to_lowercase();

        match key_lc.as_str() {
            "host" => {
                in_match = false;
                current.clear();
                for pat in value.split_whitespace() {
                    if !is_literal(pat) {
                        continue;
                    }
                    if seen.insert(pat.to_string()) {
                        hosts.push(new_host(pat.to_string(), HostSource::SshConfig));
                    }
                    // Record index of this alias for attaching options.
                    if let Some(idx) = hosts.iter().position(|h| h.alias == pat) {
                        current.push(idx);
                    }
                }
            }
            "match" => {
                in_match = true;
                current.clear();
            }
            "include" => {
                for inc in expand_includes(value, path) {
                    parse_config_file(&inc, hosts, seen, visited);
                }
            }
            "hostname" => {
                if !in_match {
                    for &idx in &current {
                        hosts[idx].hostname = Some(value.to_string());
                    }
                }
            }
            "user" => {
                if !in_match {
                    for &idx in &current {
                        hosts[idx].user = Some(value.to_string());
                    }
                }
            }
            "port" => {
                if !in_match {
                    if let Ok(p) = value.parse::<u16>() {
                        for &idx in &current {
                            hosts[idx].port = Some(p);
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

/// Expand an `Include` value (which may have multiple whitespace-separated
/// globs) into concrete file paths, relative to the including file's dir.
fn expand_includes(value: &str, including: &Path) -> Vec<PathBuf> {
    let base = including.parent().map(|p| p.to_path_buf());
    let mut out = Vec::new();

    for token in value.split_whitespace() {
        // Expand ~ and environment-style tildes.
        let expanded = shellexpand::tilde(token).to_string();
        let pattern_path = if Path::new(&expanded).is_absolute() {
            expanded
        } else {
            match &base {
                Some(b) => b.join(&expanded).to_string_lossy().into_owned(),
                None => expanded,
            }
        };

        match glob_paths(&pattern_path) {
            Some(mut paths) => out.append(&mut paths),
            None => out.push(PathBuf::from(pattern_path)),
        }
    }
    out
}

/// Minimal glob: supports `*` and `?` in the final path component only,
/// which covers the common `Include config.d/*` style. Returns `None` if the
/// pattern has no wildcards (caller treats it as a literal path).
fn glob_paths(pattern: &str) -> Option<Vec<PathBuf>> {
    if !pattern.contains('*') && !pattern.contains('?') {
        return None;
    }
    let path = Path::new(pattern);
    let dir = path.parent()?;
    let file_pat = path.file_name()?.to_string_lossy().into_owned();

    let mut matches = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if wildcard_match(&file_pat, &name) {
                matches.push(entry.path());
            }
        }
    }
    matches.sort();
    Some(matches)
}

/// Glob-style match supporting `*` (any run) and `?` (single char).
fn wildcard_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    matches_from(&p, 0, &t, 0)
}

fn matches_from(p: &[char], pi: usize, t: &[char], ti: usize) -> bool {
    if pi == p.len() {
        return ti == t.len();
    }
    match p[pi] {
        '*' => {
            // Try consuming zero or more chars.
            for skip in ti..=t.len() {
                if matches_from(p, pi + 1, t, skip) {
                    return true;
                }
            }
            false
        }
        '?' => ti < t.len() && matches_from(p, pi + 1, t, ti + 1),
        c => ti < t.len() && t[ti] == c && matches_from(p, pi + 1, t, ti + 1),
    }
}

/// Parse `~/.ssh/known_hosts`, adding any plaintext hostnames not already seen.
/// Hashed entries (`|1|...`) cannot be reversed and are skipped.
fn parse_known_hosts(path: &Path, hosts: &mut Vec<Host>, seen: &mut HashSet<String>) {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => return,
    };

    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('@') {
            continue;
        }
        let first = match line.split_whitespace().next() {
            Some(f) => f,
            None => continue,
        };
        if first.starts_with('|') {
            // Hashed host entry — not recoverable.
            continue;
        }
        for token in first.split(',') {
            if let Some(name) = clean_known_host(token) {
                if seen.insert(name.clone()) {
                    hosts.push(new_host(name, HostSource::KnownHosts));
                }
            }
        }
    }
}

/// Normalize a known_hosts host token: strip `[host]:port` brackets, ignore IPs
/// would still be added (they're connectable). Returns the bare host string.
fn clean_known_host(token: &str) -> Option<String> {
    let t = token.trim();
    if t.is_empty() {
        return None;
    }
    // Handle [host]:port form.
    if let Some(rest) = t.strip_prefix('[') {
        if let Some(end) = rest.find(']') {
            let host = &rest[..end];
            if host.is_empty() {
                return None;
            }
            return Some(host.to_string());
        }
    }
    Some(t.to_string())
}

/// Strip a trailing `#` comment from a config line (ssh config comments run to
/// end of line; there is no inline-quote escaping to worry about in practice).
fn strip_comment(line: &str) -> &str {
    match line.find('#') {
        Some(idx) => &line[..idx],
        None => line,
    }
}

/// Split a config line into (key, value). ssh config allows `key value` or
/// `key = value`.
fn split_kv(line: &str) -> Option<(&str, &str)> {
    let line = line.trim();
    if let Some((k, v)) = line.split_once('=') {
        let k = k.trim();
        let v = v.trim();
        if !k.is_empty() {
            return Some((k, v));
        }
    }
    let mut parts = line.splitn(2, char::is_whitespace);
    let key = parts.next()?.trim();
    let value = parts.next().unwrap_or("").trim();
    if key.is_empty() {
        return None;
    }
    Some((key, value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn literal_detection() {
        assert!(is_literal("prod-web"));
        assert!(!is_literal("*.example.com"));
        assert!(!is_literal("web?"));
        assert!(!is_literal("!secret"));
    }

    #[test]
    fn kv_splitting() {
        assert_eq!(split_kv("Host prod"), Some(("Host", "prod")));
        assert_eq!(split_kv("HostName = 1.2.3.4"), Some(("HostName", "1.2.3.4")));
        assert_eq!(split_kv("  Port   2222 "), Some(("Port", "2222")));
        assert_eq!(split_kv(""), None);
    }

    #[test]
    fn comment_stripping() {
        assert_eq!(strip_comment("Host prod # primary").trim(), "Host prod");
        assert_eq!(strip_comment("# whole line").trim(), "");
    }

    #[test]
    fn wildcard_matching() {
        assert!(wildcard_match("*.conf", "web.conf"));
        assert!(wildcard_match("config*", "config_local"));
        assert!(wildcard_match("h?st", "host"));
        assert!(!wildcard_match("*.conf", "web.txt"));
        assert!(wildcard_match("*", "anything"));
    }

    #[test]
    fn known_host_cleaning() {
        assert_eq!(clean_known_host("example.com").as_deref(), Some("example.com"));
        assert_eq!(clean_known_host("[example.com]:2222").as_deref(), Some("example.com"));
        assert_eq!(clean_known_host("  ").as_deref(), None);
    }

    #[test]
    fn parses_hosts_and_options_with_include() {
        let dir = std::env::temp_dir().join(format!("ssht-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("conf.d")).unwrap();

        let main_cfg = dir.join("config");
        std::fs::write(
            &main_cfg,
            "Host prod-web\n  HostName 10.0.0.1\n  User deploy\n  Port 2200\n\n\
             Host *\n  ServerAliveInterval 60\n\n\
             Include conf.d/*.conf\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("conf.d").join("extra.conf"),
            "Host db\n  HostName db.internal\n",
        )
        .unwrap();

        let mut hosts = Vec::new();
        let mut seen = HashSet::new();
        let mut visited = HashSet::new();
        parse_config_file(&main_cfg, &mut hosts, &mut seen, &mut visited);

        let prod = hosts.iter().find(|h| h.alias == "prod-web").expect("prod-web");
        assert_eq!(prod.hostname.as_deref(), Some("10.0.0.1"));
        assert_eq!(prod.user.as_deref(), Some("deploy"));
        assert_eq!(prod.port, Some(2200));

        // Wildcard `Host *` must be skipped.
        assert!(!hosts.iter().any(|h| h.alias == "*"));
        // Included host present.
        assert!(hosts.iter().any(|h| h.alias == "db"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
