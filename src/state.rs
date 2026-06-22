//! Local connection-state persistence in SQLite at
//! `~/.local/share/ssht/state.db`.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use rusqlite::Connection;

/// Persisted per-host state.
#[derive(Debug, Default, Clone)]
pub struct HostState {
    /// Unix timestamp (seconds) of the last successful connect, if any.
    pub last_connected: Option<i64>,
    pub connection_count: i64,
    pub notes: Option<String>,
}

/// Handle to the state database.
pub struct State {
    conn: Connection,
}

/// Default state DB path (`~/.local/share/ssht/state.db`, honoring
/// `$XDG_DATA_HOME`). Uses XDG conventions on both Linux and macOS, as
/// specified, rather than the platform-native data dir.
pub fn state_db_path() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_DATA_HOME") {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => dirs::home_dir()
            .context("could not determine home directory")?
            .join(".local")
            .join("share"),
    };
    Ok(base.join("ssht").join("state.db"))
}

impl State {
    /// Open (creating if needed) the state database and run migrations.
    pub fn open() -> Result<State> {
        let path = state_db_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let conn = Connection::open(&path)
            .with_context(|| format!("opening state db {}", path.display()))?;
        Self::migrate(&conn)?;
        Ok(State { conn })
    }

    fn migrate(conn: &Connection) -> Result<()> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS hosts (
                alias            TEXT PRIMARY KEY,
                last_connected   INTEGER,
                connection_count INTEGER NOT NULL DEFAULT 0,
                notes            TEXT
            );",
        )
        .context("running migrations")?;
        Ok(())
    }

    /// Load all host states keyed by alias.
    pub fn all(&self) -> Result<HashMap<String, HostState>> {
        let mut stmt = self
            .conn
            .prepare("SELECT alias, last_connected, connection_count, notes FROM hosts")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                HostState {
                    last_connected: row.get(1)?,
                    connection_count: row.get(2)?,
                    notes: row.get(3)?,
                },
            ))
        })?;

        let mut map = HashMap::new();
        for row in rows {
            let (alias, state) = row?;
            map.insert(alias, state);
        }
        Ok(map)
    }

    /// Record a successful connection: bump count and set last_connected to now.
    pub fn record_connection(&self, alias: &str) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        self.conn.execute(
            "INSERT INTO hosts (alias, last_connected, connection_count)
             VALUES (?1, ?2, 1)
             ON CONFLICT(alias) DO UPDATE SET
                last_connected = ?2,
                connection_count = connection_count + 1",
            (alias, now),
        )?;
        Ok(())
    }

    /// Alias of the most recently connected host, if any.
    pub fn last_host(&self) -> Result<Option<String>> {
        let alias = self
            .conn
            .query_row(
                "SELECT alias FROM hosts
                 WHERE last_connected IS NOT NULL
                 ORDER BY last_connected DESC LIMIT 1",
                [],
                |row| row.get::<_, String>(0),
            )
            .ok();
        Ok(alias)
    }
}
