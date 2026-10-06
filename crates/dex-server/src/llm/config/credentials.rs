use serde::Deserialize;
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::PathBuf;

use crate::protocol::Provider;

use super::catalog_query::catalog_env_vars;
use super::provider::ProviderEntry;

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

/// Extra key env vars `pi` documents that the models.dev catalog doesn't
/// (or shares across regions), keyed by catalog provider name. Tried ahead
/// of the catalog names so a region-specific var beats a shared one.
fn pi_key_env_aliases(provider: &str) -> &'static [&'static str] {
    match provider {
        "azure" | "azure-cognitive-services" => &["AZURE_OPENAI_API_KEY"],
        "zai" | "zai-coding-plan" => &["ZAI_API_KEY"],
        "zhipuai-coding-plan" => &["ZAI_CODING_CN_API_KEY"],
        "meta" => &["META_API_KEY"],
        "bailing" => &["ANT_LING_API_KEY"],
        "cloudflare-ai-gateway" => &["CLOUDFLARE_API_KEY"],
        "alibaba-token-plan" => &["QWEN_TOKEN_PLAN_API_KEY"],
        "alibaba-token-plan-cn" => &["QWEN_TOKEN_PLAN_CN_API_KEY"],
        "xiaomi-token-plan-cn" => &["XIAOMI_TOKEN_PLAN_CN_API_KEY"],
        "xiaomi-token-plan-ams" => &["XIAOMI_TOKEN_PLAN_AMS_API_KEY"],
        "xiaomi-token-plan-sgp" => &["XIAOMI_TOKEN_PLAN_SGP_API_KEY"],
        _ => &[],
    }
}

/// Ordered env var names tried for a provider's key: pinned builtin, `pi`
/// aliases, then the catalog `env` map. Shared with `dex doctor`.
pub fn key_env_names(provider: &Provider) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let extra = pinned_key_env(provider)
        .into_iter()
        .chain(pi_key_env_aliases(provider.name()).iter().copied())
        .map(str::to_string)
        .chain(catalog_env_vars(provider.name()));
    for n in extra {
        if !names.contains(&n) {
            names.push(n);
        }
    }
    names
}

/// Per-provider credentials — the uniform deposit order for every provider
/// except codex (which reads its own credential file):
/// 1. `providers.<name>.api_key` in config.yaml,
/// 2. the provider's own conventional env vars from the catalog `env` map
///    (`OPENCODE_API_KEY`, `ZHIPU_API_KEY`, `OPENROUTER_API_KEY`, …).
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
    if let Some(key) = entries
        .get(name)
        .and_then(|e| e.api_key.clone())
        .filter(|k| !k.is_empty())
    {
        return Ok((key, None));
    }
    // The provider's own documented env vars; pinned builtin vars (see
    // `pinned_key_env`) work cache-less — the catalog is the source for
    // every other provider, plus `pi`-compatible aliases.
    let env_names = key_env_names(provider);
    for var in &env_names {
        if let Ok(key) = env::var(var) {
            if !key.trim().is_empty() {
                return Ok((key, None));
            }
        }
    }
    Err(format!(
        "no API key for provider '{name}': set providers.{name}.api_key in config.yaml{}:\n  providers:\n    {name}:\n      api_key: <key>",
        if env_names.is_empty() {
            " or export the provider's key env var (run `dex update --models` to learn its name)"
                .to_string()
        } else {
            format!(" or export {}", env_names.join(", "))
        }
    )
    .into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    #[test]
    fn pi_aliases_precede_catalog_names() {
        let names = key_env_names(&Provider::Generic("zai".into()));
        assert_eq!(names.first().map(String::as_str), Some("ZAI_API_KEY"));
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
}
