use serde::Deserialize;
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::PathBuf;

use crate::protocol::Provider;

use super::catalog_query::catalog_env_vars;
use super::provider::ProviderEntry;
use super::secrets::{stored_key, SecretRef};

#[derive(Deserialize)]
pub struct CodexAuthFile {
    pub tokens: Option<CodexTokens>,
}

#[derive(Deserialize)]
pub struct CodexTokens {
    pub access_token: String,
    pub account_id: Option<String>,
}

pub fn load_codex_credentials() -> Result<(String, Option<String>), Box<dyn std::error::Error>> {
    if let Some(access_token) = env::var_os("CODEX_ACCESS_TOKEN") {
        let access_token = access_token.to_string_lossy().trim().to_string();
        if !access_token.is_empty() {
            return Ok((
                access_token,
                env::var("CODEX_ACCOUNT_ID")
                    .ok()
                    .filter(|id| !id.is_empty()),
            ));
        }
    }
    let path = env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex")))
        .ok_or("HOME is not set; cannot locate Codex credentials")?
        .join("auth.json");
    let contents = fs::read_to_string(&path)
        .map_err(|e| format!("could not read Codex credentials {}: {}", path.display(), e))?;
    let auth: CodexAuthFile = serde_json::from_str(&contents)
        .map_err(|e| format!("invalid Codex credentials {}: {}", path.display(), e))?;
    let tokens = auth
        .tokens
        .ok_or("Codex auth.json has no OAuth tokens; run `codex --login`")?;
    if tokens.access_token.trim().is_empty() {
        return Err("Codex auth.json contains an empty access token".into());
    }
    Ok((tokens.access_token, tokens.account_id))
}

/// Builtin providers whose canonical key env var is pinned in dex rather
/// than catalog-discovered, so key resolution works cache-less on a fresh
/// install (no `dex update --models` needed first). Mirrored in `doctor`'s
/// key-origin row.
pub fn pinned_key_env(provider: &Provider) -> Option<&'static str> {
    match provider {
        Provider::Anthropic => Some("ANTHROPIC_API_KEY"),
        _ => None,
    }
}

/// Ordered env var names tried for a provider's key: pinned builtin, then
/// the catalog `env` map. Shared with `dex doctor`.
pub fn key_env_names(provider: &Provider) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for n in pinned_key_env(provider)
        .into_iter()
        .map(str::to_string)
        .chain(catalog_env_vars(provider.name()))
    {
        if !names.contains(&n) {
            names.push(n);
        }
    }
    names
}

/// Per-provider credentials — the uniform deposit order for every provider
/// except codex (which reads its own credential file):
/// 1. `providers.<name>.api_key` in config.yaml — a literal or a reference
///    (`$NAME`, `!command`, `file:path`; see `secrets`),
/// 2. `$XDG_DATA_HOME/dex/auth.json` (`dex auth login <provider>`),
/// 3. the provider's own conventional env vars (pinned builtin, then the
///    catalog `env` map: `OPENCODE_API_KEY`, `ZHIPU_API_KEY`, …).
///
/// Then a loud error naming the deposit places.
pub fn resolve_credentials(
    provider: &Provider,
    entries: &BTreeMap<String, ProviderEntry>,
) -> Result<(String, Option<String>), Box<dyn std::error::Error>> {
    if matches!(provider, Provider::OpenAiCodex) {
        return load_codex_credentials();
    }
    let name = provider.name();
    if let Some(raw) = entries
        .get(name)
        .and_then(|e| e.api_key.as_deref())
        .filter(|k| !k.trim().is_empty())
    {
        // A reference that resolves empty (unset env var) falls through;
        // a broken command/file is a loud error, not a silent fallback.
        let reference = SecretRef::parse(raw);
        if let Some(key) = reference
            .resolve()
            .map_err(|e| format!("providers.{name}.api_key: {e}"))?
        {
            return Ok((key, None));
        }
    }
    if let Some(key) = stored_key(name) {
        return Ok((key, None));
    }
    let env_names = key_env_names(provider);
    for var in &env_names {
        if let Ok(key) = env::var(var) {
            if !key.trim().is_empty() {
                return Ok((key, None));
            }
        }
    }
    Err(format!(
        "no API key for provider '{name}': run `dex auth login {name}`, or set providers.{name}.api_key in config.yaml{}:\n  providers:\n    {name}:\n      api_key: $MY_{upper}_KEY   # or a literal, !command, file:path",
        if env_names.is_empty() {
            " or export the provider's key env var (run `dex update --models` to learn its name)"
                .to_string()
        } else {
            format!(" or export {}", env_names.join(", "))
        },
        upper = name.to_ascii_uppercase().replace(|c: char| !c.is_ascii_alphanumeric(), "_"),
    )
    .into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    #[test]
    fn pinned_env_name_comes_first() {
        let names = key_env_names(&Provider::Anthropic);
        assert_eq!(names.first().map(String::as_str), Some("ANTHROPIC_API_KEY"));
    }

    #[test]
    fn load_resolves_env_then_file() {
        let prev_token = env::var_os("CODEX_ACCESS_TOKEN");
        let prev_acct = env::var_os("CODEX_ACCOUNT_ID");
        let prev_home = env::var_os("CODEX_HOME");

        // env token wins, trimmed, empty account filtered
        env::set_var("CODEX_ACCESS_TOKEN", " tok123 ");
        env::set_var("CODEX_ACCOUNT_ID", "acct1");
        let (tok, acct) = load_codex_credentials().unwrap();
        assert_eq!(tok, "tok123");
        assert_eq!(acct.as_deref(), Some("acct1"));
        env::set_var("CODEX_ACCOUNT_ID", "");
        let (_, acct2) = load_codex_credentials().unwrap();
        assert!(acct2.is_none());

        // no env -> auth.json under CODEX_HOME
        env::remove_var("CODEX_ACCESS_TOKEN");
        let dir = std::env::temp_dir().join(format!("dex-codex-auth-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        fs::write(
            dir.join("auth.json"),
            r#"{"tokens":{"access_token":"file-token","account_id":"file-acct"}}"#,
        )
        .unwrap();
        env::set_var("CODEX_HOME", &dir);
        let (tok, acct) = load_codex_credentials().unwrap();
        assert_eq!(tok, "file-token");
        assert_eq!(acct.as_deref(), Some("file-acct"));

        // blank token in file is rejected
        fs::write(
            dir.join("auth.json"),
            r#"{"tokens":{"access_token":"   "}}"#,
        )
        .unwrap();
        assert!(load_codex_credentials().is_err());
        let _ = fs::remove_dir_all(&dir);

        match prev_token {
            Some(v) => env::set_var("CODEX_ACCESS_TOKEN", v),
            None => env::remove_var("CODEX_ACCESS_TOKEN"),
        }
        match prev_acct {
            Some(v) => env::set_var("CODEX_ACCOUNT_ID", v),
            None => env::remove_var("CODEX_ACCOUNT_ID"),
        }
        match prev_home {
            Some(v) => env::set_var("CODEX_HOME", v),
            None => env::remove_var("CODEX_HOME"),
        }
    }

    #[test]
    fn key_precedence_config_ref_then_auth_json_then_env() {
        let _env = crate::test_env::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let prev = [
            ("XDG_DATA_HOME", env::var_os("XDG_DATA_HOME")),
            ("ANTHROPIC_API_KEY", env::var_os("ANTHROPIC_API_KEY")),
            ("DEX_TEST_KEY_REF", env::var_os("DEX_TEST_KEY_REF")),
        ];
        let dir = env::temp_dir().join(format!("dex-keyprec-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        env::set_var("XDG_DATA_HOME", &dir);
        env::set_var("ANTHROPIC_API_KEY", "from-env");
        env::remove_var("DEX_TEST_KEY_REF");
        let mut entries = BTreeMap::new();
        let key = |entries: &BTreeMap<String, ProviderEntry>| {
            resolve_credentials(&Provider::Anthropic, entries)
                .map(|(k, _)| k)
                .map_err(|e| e.to_string())
        };
        assert_eq!(key(&entries).unwrap(), "from-env");
        // auth.json beats env, is 0600, and `logout` removes it.
        super::super::secrets::store_key("anthropic", "from-auth").unwrap();
        assert_eq!(key(&entries).unwrap(), "from-auth");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = fs::metadata(dir.join("dex/auth.json"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
            // A group/world-readable store is ignored, not trusted.
            fs::set_permissions(dir.join("dex/auth.json"), fs::Permissions::from_mode(0o644))
                .unwrap();
            assert_eq!(key(&entries).unwrap(), "from-env");
            fs::set_permissions(dir.join("dex/auth.json"), fs::Permissions::from_mode(0o600))
                .unwrap();
        }
        // Config reference beats auth.json; an unset `$VAR` falls through.
        let entry = |k: &str| ProviderEntry {
            api_key: Some(k.to_string()),
            ..Default::default()
        };
        entries.insert("anthropic".into(), entry("$DEX_TEST_KEY_REF"));
        assert_eq!(key(&entries).unwrap(), "from-auth");
        env::set_var("DEX_TEST_KEY_REF", "from-ref");
        assert_eq!(key(&entries).unwrap(), "from-ref");
        entries.insert("anthropic".into(), entry("!printf from-cmd"));
        assert_eq!(key(&entries).unwrap(), "from-cmd");
        // A failing command is a loud error, not a silent fallback.
        entries.insert("anthropic".into(), entry("!exit 1"));
        assert!(key(&entries)
            .unwrap_err()
            .contains("providers.anthropic.api_key"));
        assert!(super::super::secrets::remove_key("anthropic").unwrap());
        let _ = fs::remove_dir_all(&dir);
        for (k, v) in prev {
            match v {
                Some(v) => env::set_var(k, v),
                None => env::remove_var(k),
            }
        }
    }
}
