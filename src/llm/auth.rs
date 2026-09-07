use serde::Deserialize;
use std::env;
use std::fs;
use std::path::PathBuf;

#[derive(Deserialize)]
pub(crate) struct CodexAuthFile {
    pub(crate) tokens: Option<CodexTokens>,
}

#[derive(Deserialize)]
pub(crate) struct CodexTokens {
    pub(crate) access_token: String,
    pub(crate) account_id: Option<String>,
}

pub(crate) fn load_codex_credentials(
) -> Result<(String, Option<String>), Box<dyn std::error::Error>> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

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
