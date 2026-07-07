use std::collections::HashMap;
use std::path::PathBuf;
use std::io::{self, Write};

use anyhow::{Context, Result, bail};
use serde::{Serialize, Deserialize};
use age::secrecy::SecretString;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct HostSettings {
    pub name: Option<String>,
    pub address: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct VaultContent {
    version: i32,
    #[serde(default)]
    hosts: HashMap<String, HostSettings>,
}

#[derive(Debug, Deserialize)]
struct VaultContentV1 {
    hosts: HashMap<String, String>,
}

/// Plaintext sidecar listing which aliases have vault entries (names only,
/// no secrets). Lets callers check "does this alias have stored settings?"
/// without unlocking the vault, so we don't prompt for the passphrase on
/// every connection.
#[derive(Debug, Default, Serialize, Deserialize)]
struct VaultIndex {
    #[serde(default)]
    aliases: Vec<String>,
}

pub struct Vault {
    path: PathBuf,
    passphrase: String,
    entries: HashMap<String, HostSettings>,
}

impl Vault {
    pub fn vault_path() -> Result<PathBuf> {
        if let Some(v) = std::env::var_os("SSHT_VAULT_FILE") {
            if !v.is_empty() {
                return Ok(PathBuf::from(v));
            }
        }
        let base = match std::env::var_os("XDG_CONFIG_HOME") {
            Some(v) if !v.is_empty() => PathBuf::from(v),
            _ => dirs::home_dir()
                .context("could not determine home directory")?
                .join(".config"),
        };
        Ok(base.join("ssht").join("vault.age"))
    }

    pub fn exists() -> Result<bool> {
        Ok(Self::vault_path()?.exists())
    }

    fn index_path_for(vault_path: &PathBuf) -> PathBuf {
        vault_path.with_extension("idx")
    }

    /// Returns true if the on-disk vault appears to have an entry for
    /// `alias`, without requiring the passphrase. If the vault doesn't
    /// exist, returns false. If the vault exists but predates the alias
    /// index (or the index is unreadable), conservatively returns true so a
    /// real stored password is never silently skipped — the index is
    /// (re)written the next time the vault is opened or saved.
    pub fn alias_hint_exists(alias: &str) -> Result<bool> {
        let vault_path = Self::vault_path()?;
        if !vault_path.exists() {
            return Ok(false);
        }
        match std::fs::read_to_string(Self::index_path_for(&vault_path)) {
            Ok(text) => {
                let index: VaultIndex = toml::from_str(&text).unwrap_or_default();
                Ok(index.aliases.iter().any(|a| a == alias))
            }
            Err(_) => Ok(true),
        }
    }

    fn write_index(&self) -> Result<()> {
        let mut aliases: Vec<String> = self.entries.keys().cloned().collect();
        aliases.sort();
        let index_path = Self::index_path_for(&self.path);
        let text = toml::to_string(&VaultIndex { aliases })
            .context("serializing vault index")?;
        std::fs::write(&index_path, text)
            .with_context(|| format!("writing vault index to {}", index_path.display()))?;
        Ok(())
    }

    pub fn init(passphrase: &str) -> Result<Vault> {
        let path = Self::vault_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        if path.exists() {
            bail!("vault already exists at {}", path.display());
        }
        let content = VaultContent {
            version: 2,
            hosts: HashMap::new(),
        };
        let plaintext = toml::to_string(&content)
            .context("serializing vault content")?;
        let encrypted = Self::encrypt(passphrase, plaintext.as_bytes())
            .context("encrypting vault")?;
        std::fs::write(&path, &encrypted)
            .with_context(|| format!("writing vault to {}", path.display()))?;
        let vault = Vault { path, passphrase: passphrase.to_string(), entries: HashMap::new() };
        vault.write_index()?;
        Ok(vault)
    }

    pub fn open(passphrase: &str) -> Result<Vault> {
        let path = Self::vault_path()?;
        let encrypted = std::fs::read(&path)
            .with_context(|| format!("reading vault from {}", path.display()))?;
        let plaintext = Self::decrypt(passphrase, &encrypted)
            .context("decrypting vault (wrong passphrase?)")?;
        let text = String::from_utf8(plaintext).context("vault content is not valid UTF-8")?;

        let entries = match toml::from_str::<VaultContent>(&text) {
            Ok(content) => content.hosts,
            Err(_) => {
                let old: VaultContentV1 = toml::from_str(&text)
                    .context("could not parse vault (neither v1 nor v2 format)")?;
                old.hosts.into_iter().map(|(k, v)| {
                    (k, HostSettings { password: Some(v), ..Default::default() })
                }).collect()
            }
        };

        let vault = Vault { path, passphrase: passphrase.to_string(), entries };
        // Self-heal the alias index (covers vaults created before it existed).
        vault.write_index()?;
        Ok(vault)
    }

    pub fn change_passphrase(&mut self, new_passphrase: &str) -> Result<()> {
        self.passphrase = new_passphrase.to_string();
        self.save()
    }

    pub fn get_settings(&self, alias: &str) -> Option<&HostSettings> {
        self.entries.get(alias)
    }

    pub fn set_settings(&mut self, alias: &str, settings: HostSettings) -> Result<()> {
        self.entries.insert(alias.to_string(), settings);
        self.save()
    }

    pub fn remove(&mut self, alias: &str) -> Result<()> {
        self.entries.remove(alias);
        self.save()
    }

    pub fn list(&self) -> Vec<&str> {
        let mut aliases: Vec<&str> = self.entries.keys().map(|s| s.as_str()).collect();
        aliases.sort();
        aliases
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn save(&self) -> Result<()> {
        let content = VaultContent {
            version: 2,
            hosts: self.entries.clone(),
        };
        let plaintext = toml::to_string(&content)
            .context("serializing vault content")?;
        let encrypted = Self::encrypt(&self.passphrase, plaintext.as_bytes())
            .context("encrypting vault")?;
        std::fs::write(&self.path, &encrypted)
            .with_context(|| format!("writing vault to {}", self.path.display()))?;
        self.write_index()?;
        Ok(())
    }

    fn encrypt(passphrase: &str, data: &[u8]) -> Result<Vec<u8>> {
        let secret = SecretString::from(passphrase.to_owned());
        let recipient = age::scrypt::Recipient::new(secret);
        age::encrypt(&recipient, data).context("age encryption failed")
    }

    fn decrypt(passphrase: &str, data: &[u8]) -> Result<Vec<u8>> {
        let secret = SecretString::from(passphrase.to_owned());
        let identity = age::scrypt::Identity::new(secret);
        age::decrypt(&identity, data).context("age decryption failed")
    }
}

/// A lazily-unlocked vault holding per-host settings (address, name, username, password).
pub struct LazyVault {
    vault: Option<Vault>,
}

impl LazyVault {
    pub fn new() -> Self {
        LazyVault { vault: None }
    }

    pub fn ensure_unlocked(&mut self) -> Result<()> {
        if self.vault.is_some() {
            return Ok(());
        }
        if !Vault::exists()? {
            return Ok(());
        }
        self.vault = Some(prompt_and_open_vault()?);
        Ok(())
    }

    /// Inject an already-unlocked vault (used by TUI unlock flow).
    /// Replaces whatever is currently stored.
    pub fn inject(&mut self, vault: Vault) {
        self.vault = Some(vault);
    }

    /// Returns true if the vault is already unlocked (inner Vault is Some).
    pub fn is_unlocked(&self) -> bool {
        self.vault.is_some()
    }

    /// Returns true if the vault file exists but is not yet unlocked.
    pub fn needs_unlock_or_init(&self) -> bool {
        self.vault.is_none()
    }

    /// Save host settings. Requires the vault to already be unlocked.
    /// Returns an error if the vault is locked or doesn't exist.
    pub fn set_settings_data(&mut self, alias: &str, settings: HostSettings) -> Result<()> {
        let vault = self.vault.as_mut()
            .ok_or_else(|| anyhow::anyhow!("vault is not unlocked"))?;
        vault.set_settings(alias, settings)?;
        Ok(())
    }

    /// Get host settings for `alias`. Never prompts — returns `None` if vault
    /// is locked or doesn't exist, or if the alias has no stored settings.
    pub fn get_settings(&mut self, alias: &str) -> Result<Option<HostSettings>> {
        Ok(self.vault.as_ref().and_then(|v| v.get_settings(alias)).cloned())
    }

    /// Returns true if `alias` might have stored settings, without prompting
    /// for the passphrase. If already unlocked, this is exact; otherwise it
    /// consults the on-disk alias index. Callers should use this to decide
    /// whether unlocking (and thus prompting) is worthwhile at all.
    pub fn might_have_settings(&self, alias: &str) -> Result<bool> {
        match &self.vault {
            Some(v) => Ok(v.get_settings(alias).is_some()),
            None => Vault::alias_hint_exists(alias),
        }
    }

    /// Returns `true` if the vault file exists but is not yet unlocked.
    pub fn is_locked(&self) -> Result<bool> {
        Ok(Vault::exists()? && self.vault.is_none())
    }

    /// Get just the password for `alias`.
    #[allow(dead_code)]
    pub fn get_password(&mut self, alias: &str) -> Result<Option<String>> {
        Ok(self.get_settings(alias)?.and_then(|s| s.password))
    }

    /// Open the settings editor for `alias`: exit TUI, prompt for each field,
    /// save to vault.
    #[allow(dead_code)]
    pub fn edit_settings(&mut self, alias: &str, current_addr: Option<&str>,
                          current_user: Option<&str>) -> Result<()> {
        if Vault::exists()? {
            self.ensure_unlocked()
                .context("could not unlock vault")?;
        } else {
            let passphrase = prompt_passphrase("Create vault passphrase: ")?;
            let confirm = prompt_passphrase("Confirm passphrase: ")?;
            if passphrase != confirm {
                bail!("passphrases do not match");
            }
            if passphrase.is_empty() {
                bail!("passphrase cannot be empty");
            }
            self.vault = Some(Vault::init(&passphrase)?);
            eprintln!("Vault created");
        }

        let existing = self.vault.as_ref().unwrap().get_settings(alias).cloned().unwrap_or_default();

        let name = prompt_field("Name", existing.name.as_deref().or(Some(alias)))?;
        let addr = prompt_field("Address", existing.address.as_deref().or(current_addr))?;
        let user = prompt_field("Username", existing.username.as_deref().or(current_user))?;
        let password = prompt_password_field("Password", existing.password.as_deref())?;

        let settings = HostSettings {
            name: Some(name).filter(|s| !s.is_empty()),
            address: Some(addr).filter(|s| !s.is_empty()),
            username: Some(user).filter(|s| !s.is_empty()),
            password: Some(password).filter(|s| !s.is_empty()),
        };

        self.vault.as_mut().unwrap().set_settings(alias, settings)?;
        eprintln!("Settings saved for {alias}");
        Ok(())
    }
}

pub fn prompt_field(label: &str, current: Option<&str>) -> Result<String> {
    let default = current.unwrap_or("");
    let prompt = if default.is_empty() {
        format!("{label}: ")
    } else {
        format!("{label} [{default}]: ")
    };
    print!("{prompt}");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let val = input.trim().to_string();
    if val.is_empty() && !default.is_empty() {
        Ok(default.to_string())
    } else {
        Ok(val)
    }
}

pub fn prompt_password_field(label: &str, current: Option<&str>) -> Result<String> {
    if let Some(cur) = current {
        let prompt = format!("{label} (leave blank to keep current): ");
        let input = rpassword::prompt_password(&prompt)?;
        if input.is_empty() {
            return Ok(cur.to_string());
        }
        Ok(input)
    } else {
        let prompt = format!("{label}: ");
        rpassword::prompt_password(&prompt).context("reading password")
    }
}

/// Prompt user for vault passphrase with retry logic.
fn prompt_and_open_vault() -> Result<Vault> {
    let max_attempts = 3;
    for attempt in 1..=max_attempts {
        let prompt = format!("Vault passphrase (attempt {attempt}/{max_attempts}): ");
        let passphrase = prompt_passphrase(&prompt)?;
        match Vault::open(&passphrase) {
            Ok(vault) => {
                return Ok(vault);
            }
            Err(e) => {
                if attempt < max_attempts {
                    eprintln!("{e:#}");
                    eprintln!("Wrong passphrase, try again.");
                } else {
                    bail!("wrong vault passphrase after {max_attempts} attempts");
                }
            }
        }
    }
    unreachable!()
}

pub fn setup_ssh_askpass(
    cmd: &mut std::process::Command,
    password: &str,
) -> Result<tempfile::TempDir> {
    let tmp_dir = tempfile::tempdir()
        .context("creating temp directory for SSH_ASKPASS")?;

    let pass_path = tmp_dir.path().join("password");
    let askpass_path = tmp_dir.path().join("askpass.sh");

    std::fs::write(&pass_path, password.as_bytes())
        .context("writing password file")?;

    let script = format!("#!/bin/sh\ncat \"{}\"\n", pass_path.display());
    std::fs::write(&askpass_path, script.as_bytes())
        .context("writing askpass script")?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&askpass_path, std::fs::Permissions::from_mode(0o755))
            .context("making askpass script executable")?;
    }

    cmd.env("SSH_ASKPASS", askpass_path);
    cmd.env("SSH_ASKPASS_REQUIRE", "force");

    Ok(tmp_dir)
}

pub fn prompt_passphrase(prompt: &str) -> Result<String> {
    let passphrase = rpassword::prompt_password(prompt)
        .context("reading passphrase")?;
    Ok(passphrase)
}



/// Test helper shared across modules (e.g. `picker`'s tests) that need a
/// throwaway vault on disk without touching the user's real one.
#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::Mutex;

    /// Serialize tests that modify the SSHT_VAULT_FILE env var, since it's
    /// process-global and cargo runs tests in parallel threads.
    static VAULT_ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Set up the vault file path to a temp dir and run the test closure.
    pub(crate) fn with_temp_vault<F>(f: F)
    where
        F: FnOnce(),
    {
        let _lock = VAULT_ENV_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("ssht_vault_test_{}", std::process::id()));
        let vault_file = dir.join("vault.age");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let prev = std::env::var_os("SSHT_VAULT_FILE");
        // SAFETY: serialized by mutex, single-threaded within each test
        unsafe {
            std::env::set_var("SSHT_VAULT_FILE", &vault_file);
        }
        f();
        std::fs::remove_dir_all(&dir).ok();
        unsafe {
            match prev {
                Some(v) => std::env::set_var("SSHT_VAULT_FILE", v),
                None => std::env::remove_var("SSHT_VAULT_FILE"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::test_support::with_temp_vault;

    #[test]
    fn test_encrypt_decrypt_roundtrip() {
        let data = b"hello world";
        let passphrase = "test-passphrase";
        let encrypted = Vault::encrypt(passphrase, data).unwrap();
        assert_ne!(encrypted, data);
        assert!(encrypted.len() > data.len());
        let decrypted = Vault::decrypt(passphrase, &encrypted).unwrap();
        assert_eq!(decrypted, data);
    }

    #[test]
    fn test_wrong_passphrase_fails() {
        let data = b"secret data";
        let encrypted = Vault::encrypt("correct-horse", data).unwrap();
        let result = Vault::decrypt("wrong-passphrase", &encrypted);
        assert!(result.is_err());
    }

    #[test]
    fn test_vault_init_and_reopen() {
        with_temp_vault(|| {
            let vault = Vault::init("mypass").unwrap();
            assert_eq!(vault.len(), 0);
            assert!(vault.is_empty());

            let reopened = Vault::open("mypass").unwrap();
            assert_eq!(reopened.len(), 0);

            let result = Vault::open("wrongpass");
            assert!(result.is_err());
        });
    }

    fn pw(settings: &HostSettings) -> Option<&str> {
        settings.password.as_deref()
    }

    #[test]
    fn test_vault_crud() {
        with_temp_vault(|| {
            let mut vault = Vault::init("mypass").unwrap();
            let s = HostSettings { password: Some("s3cret!".into()), ..Default::default() };
            vault.set_settings("prod-web", s).unwrap();
            assert_eq!(vault.len(), 1);
            assert!(!vault.is_empty());
            assert_eq!(pw(vault.get_settings("prod-web").unwrap()), Some("s3cret!"));

            let s = HostSettings { password: Some("p@ss".into()), ..Default::default() };
            vault.set_settings("db-primary", s).unwrap();
            assert_eq!(vault.len(), 2);

            let list = vault.list();
            assert_eq!(list, vec!["db-primary", "prod-web"]);

            assert!(vault.get_settings("nonexistent").is_none());

            vault.remove("prod-web").unwrap();
            assert_eq!(vault.len(), 1);
            assert!(vault.get_settings("prod-web").is_none());

            let reopened = Vault::open("mypass").unwrap();
            assert_eq!(reopened.len(), 1);
            assert_eq!(pw(reopened.get_settings("db-primary").unwrap()), Some("p@ss"));
        });
    }

    #[test]
    fn test_alias_hint_avoids_unnecessary_unlock() {
        with_temp_vault(|| {
            // No vault at all: no hint, no unlock needed.
            assert!(!Vault::alias_hint_exists("prod-web").unwrap());

            let mut vault = Vault::init("mypass").unwrap();
            let s = HostSettings { password: Some("s3cret!".into()), ..Default::default() };
            vault.set_settings("prod-web", s).unwrap();

            // Vault exists and has an index: only the stored alias hints true.
            assert!(Vault::alias_hint_exists("prod-web").unwrap());
            assert!(!Vault::alias_hint_exists("no-such-host").unwrap());

            let mut lazy = LazyVault::new();
            assert!(lazy.might_have_settings("prod-web").unwrap());
            assert!(!lazy.might_have_settings("no-such-host").unwrap());
        });
    }

    #[test]
    fn test_change_passphrase() {
        with_temp_vault(|| {
            let mut vault = Vault::init("old-pass").unwrap();
            let s = HostSettings { password: Some("secret123".into()), ..Default::default() };
            vault.set_settings("server1", s).unwrap();

            vault.change_passphrase("new-pass").unwrap();

            assert!(Vault::open("old-pass").is_err());

            let reopened = Vault::open("new-pass").unwrap();
            assert_eq!(pw(reopened.get_settings("server1").unwrap()), Some("secret123"));
        });
    }

    #[test]
    fn test_setup_ssh_askpass_content() {
        let tmp = tempfile::tempdir().unwrap();
        let pass_path = tmp.path().join("password");
        let askpass_path = tmp.path().join("askpass.sh");

        std::fs::write(&pass_path, b"test-password").unwrap();
        let script = format!("#!/bin/sh\ncat \"{}\"\n", pass_path.display());
        std::fs::write(&askpass_path, script.as_bytes()).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&askpass_path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        // Verify the script can be executed and returns the password
        let output = std::process::Command::new("sh")
            .arg(&askpass_path)
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "test-password");
    }
}
