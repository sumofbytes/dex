//! Secret references and the `auth.json` credential store.
//!
//! `providers.<name>.api_key` in config.yaml is a *reference*, not
//! necessarily a literal (same shapes as `pi`'s `models.json`):
//! `$NAME` / `${NAME}` (env var), `!command` (stdout of `sh -c`), or
//! `file:path` (file contents, `~` expanded). Anything else is a literal
//! key; `$$…` / `!!…` escape a literal that starts with `$` / `!`.
//!
//! `dex auth login <provider>` writes keys to `$XDG_DATA_HOME/dex/auth.json`
//! (0600) so config.yaml stays secret-free and shareable.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// How a `providers.<name>.api_key` value resolves — never the secret itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SecretRef {
    Env(String),
    Command(String),
    File(String),
    Literal(String),
}

impl SecretRef {
    pub fn parse(raw: &str) -> SecretRef {
        let raw = raw.trim();
        if let Some(rest) = raw.strip_prefix("$$") {
            return SecretRef::Literal(format!("${rest}"));
        }
        if let Some(rest) = raw.strip_prefix("!!") {
            return SecretRef::Literal(format!("!{rest}"));
        }
        if let Some(cmd) = raw.strip_prefix('!') {
            return SecretRef::Command(cmd.trim().to_string());
        }
        if let Some(path) = raw.strip_prefix("file:") {
            return SecretRef::File(path.trim().to_string());
        }
        if let Some(name) = env_ref_name(raw) {
            return SecretRef::Env(name.to_string());
        }
        SecretRef::Literal(raw.to_string())
    }

    /// Short origin label for `dex doctor` (no secret material).
    pub fn describe(&self) -> String {
        match self {
            SecretRef::Env(n) => format!("${n}"),
            SecretRef::Command(_) => "!command".to_string(),
            SecretRef::File(p) => format!("file:{p}"),
            SecretRef::Literal(_) => "literal".to_string(),
        }
    }

    pub fn is_literal(&self) -> bool {
        matches!(self, SecretRef::Literal(_))
    }

    /// Resolve to the secret. `Ok(None)` means the reference points at
    /// something empty/unset (an env var), so the caller may fall through.
    pub fn resolve(&self) -> Result<Option<String>, String> {
        let value = match self {
            SecretRef::Literal(v) => v.clone(),
            SecretRef::Env(name) => std::env::var(name).unwrap_or_default(),
            SecretRef::File(path) => {
                let path = expand_home(path);
                std::fs::read_to_string(&path)
                    .map_err(|e| format!("could not read key file {}: {e}", path.display()))?
            }
            SecretRef::Command(cmd) => run_key_command(cmd)?,
        };
        let value = value.trim().to_string();
        Ok((!value.is_empty()).then_some(value))
    }
}

/// `$NAME` or `${NAME}` where NAME is a plain identifier.
fn env_ref_name(raw: &str) -> Option<&str> {
    let name = raw.strip_prefix('$')?;
    let name = name
        .strip_prefix('{')
        .and_then(|n| n.strip_suffix('}'))
        .unwrap_or(name);
    let mut chars = name.chars();
    let first = chars.next()?;
    (first.is_ascii_alphabetic() || first == '_')
        .then_some(())
        .filter(|_| chars.all(|c| c.is_ascii_alphanumeric() || c == '_'))
        .map(|_| name)
}

fn expand_home(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join(rest))
            .unwrap_or_else(|| PathBuf::from(path)),
        None => PathBuf::from(path),
    }
}

fn run_key_command(cmd: &str) -> Result<String, String> {
    if cmd.is_empty() {
        return Err("empty `!command` api_key".to_string());
    }
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit())
        .output()
        .map_err(|e| format!("could not run key command: {e}"))?;
    if !out.status.success() {
        return Err(format!("key command exited with {}", out.status));
    }
    String::from_utf8(out.stdout).map_err(|_| "key command output is not UTF-8".to_string())
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
struct StoredKey {
    #[serde(rename = "type")]
    kind: String,
    key: String,
}

/// `$XDG_DATA_HOME/dex/auth.json`.
pub fn auth_file_path() -> Option<PathBuf> {
    crate::runtime::logging::data_home().map(|base| base.join("dex/auth.json"))
}

fn read_store() -> BTreeMap<String, StoredKey> {
    auth_file_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn write_store(store: &BTreeMap<String, StoredKey>) -> Result<(), String> {
    let path = auth_file_path().ok_or("cannot locate data directory (HOME unset)")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("auth: {e}"))?;
    }
    let text = serde_json::to_string_pretty(store).map_err(|e| format!("auth: {e}"))?;
    let tmp = path.with_extension("json.tmp");
    #[cfg(unix)]
    {
        // 0600 from creation (no umask window), then atomic rename.
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        let _ = std::fs::remove_file(&tmp);
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|e| format!("auth: {e}"))?;
        f.write_all(text.as_bytes())
            .map_err(|e| format!("auth: {e}"))?;
    }
    #[cfg(not(unix))]
    std::fs::write(&tmp, text).map_err(|e| format!("auth: {e}"))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("auth: {e}"))
}

/// True when the credential file is readable by group/others (Unix only).
pub fn auth_file_too_open() -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        auth_file_path()
            .and_then(|p| std::fs::metadata(p).ok())
            .is_some_and(|m| m.permissions().mode() & 0o077 != 0)
    }
    #[cfg(not(unix))]
    false
}

/// Stored key for a provider; ignored (with a warning) when the file is
/// group/world-readable.
pub fn stored_key(provider: &str) -> Option<String> {
    if auth_file_too_open() {
        super::warn_once(
            "auth-file-perms",
            "dex: auth.json is readable by others — ignoring it; run `chmod 600` on it",
        );
        return None;
    }
    read_store()
        .get(provider)
        .map(|k| k.key.trim().to_string())
        .filter(|k| !k.is_empty())
}

pub fn store_key(provider: &str, key: &str) -> Result<(), String> {
    let key = key.trim();
    if key.is_empty() {
        return Err("empty key".to_string());
    }
    let mut store = read_store();
    store.insert(
        provider.to_string(),
        StoredKey {
            kind: "api_key".to_string(),
            key: key.to_string(),
        },
    );
    write_store(&store)
}

/// True when an entry was removed.
pub fn remove_key(provider: &str) -> Result<bool, String> {
    let mut store = read_store();
    let removed = store.remove(provider).is_some();
    if removed {
        write_store(&store)?;
    }
    Ok(removed)
}

pub fn stored_providers() -> Vec<String> {
    read_store().into_keys().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_reference_forms() {
        assert_eq!(SecretRef::parse("$A_B"), SecretRef::Env("A_B".into()));
        assert_eq!(SecretRef::parse("${A_B}"), SecretRef::Env("A_B".into()));
        assert_eq!(
            SecretRef::parse("!pass show x"),
            SecretRef::Command("pass show x".into())
        );
        assert_eq!(SecretRef::parse("file:~/k"), SecretRef::File("~/k".into()));
        assert_eq!(
            SecretRef::parse("sk-abc"),
            SecretRef::Literal("sk-abc".into())
        );
        assert_eq!(SecretRef::parse("$$abc"), SecretRef::Literal("$abc".into()));
        assert_eq!(SecretRef::parse("!!abc"), SecretRef::Literal("!abc".into()));
        // Not an identifier → literal.
        assert!(SecretRef::parse("$1x").is_literal());
        assert!(SecretRef::parse("$a-b").is_literal());
    }

    #[test]
    fn resolves_command_and_file() {
        assert_eq!(
            SecretRef::parse("!printf ' k1\\n'")
                .resolve()
                .unwrap()
                .as_deref(),
            Some("k1")
        );
        assert!(SecretRef::parse("!exit 3").resolve().is_err());
        let p = std::env::temp_dir().join(format!("dex-secret-{}", std::process::id()));
        std::fs::write(&p, "k2\n").unwrap();
        let r = SecretRef::File(p.display().to_string());
        assert_eq!(r.resolve().unwrap().as_deref(), Some("k2"));
        let _ = std::fs::remove_file(&p);
        assert!(SecretRef::File("/nonexistent/dex-key".into())
            .resolve()
            .is_err());
    }

    #[test]
    fn unset_env_ref_is_none() {
        assert_eq!(
            SecretRef::Env("DEX_SURELY_UNSET_KEY_VAR".into())
                .resolve()
                .unwrap(),
            None
        );
    }
}
