//! Per-provider wiring. What differs by provider *kind* is a method here
//! (builtin tables); what differs by provider *configuration* (endpoint,
//! key, protocol pin, headers) is derived once into
//! `config::ResolvedProvider` from the `providers:` map + models.dev
//! catalog. Identity (`parse`/`name`) stays on the enum in `core::types`.

use std::collections::BTreeMap;

use crate::core::types::Provider;

/// Model used when `--model` and config file `model:` are both unset.
pub(crate) const DEFAULT_MODEL: &str = "gpt-5.6-luna";

/// Context-window fallback when `DEX_CONTEXT_WINDOW` is unset and the
/// models.dev catalog has no entry for the model.
pub(crate) const DEFAULT_CONTEXT_WINDOW: u64 = 128_000;

impl Provider {
    /// Base URL when `--base-url` and config-file
    /// `base_url` are all unset. Bare model picks route themselves to the
    /// right endpoint via the models.dev catalog (see `apply_model`), so
    /// this is just the landing endpoint, not a per-model decision. Generic
    /// providers land on their catalog `api` URL instead (`None` here;
    /// `config::landing_base_url_for` resolves it).
    pub(crate) fn default_base_url(&self) -> Option<&'static str> {
        match self {
            Self::OpenCode => Some("https://opencode.ai/zen/v1"),
            Self::OpenAiCodex => Some("https://chatgpt.com/backend-api/codex"),
            Self::Generic(_) => None,
        }
    }

    /// Named endpoints offered to `/model` routing (`zen/<id>`, `go/<id>`).
    /// Generic providers carry a single implicit endpoint (their catalog
    /// `api` URL, named after the provider); `LlmConfig` injects it.
    pub(crate) fn endpoints(&self) -> BTreeMap<String, String> {
        match self {
            // ponytail: static table, add dynamic registry if more than
            // 3 builtin endpoints
            Self::OpenCode => [
                ("zen", "https://opencode.ai/zen/v1"),
                ("go", "https://opencode.ai/zen/go/v1"),
            ]
            .into_iter()
            .map(|(name, url)| (name.to_string(), url.to_string()))
            .collect(),
            // Generic endpoints live in LlmConfig (catalog/config-derived).
            _ => BTreeMap::new(),
        }
    }

    /// models.dev catalog keys that can serve this provider (pricing lookup).
    /// These are the catalog's own provider ids — `"openai"` here is
    /// OpenAI proper in models.dev, not a dex provider alias (there is none).
    pub(crate) fn catalog_keys(&self) -> Vec<String> {
        match self {
            Self::OpenCode => ["opencode", "opencode-go", "openai"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            Self::OpenAiCodex => ["openai-codex", "codex"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            // The catalog entry of the same key prices a generic provider.
            Self::Generic(name) => vec![name.clone()],
        }
    }

    /// Does the endpoint also speak chat-completions, so a rejected
    /// `/responses` call may be retried there? (Empirical protocol fallback.)
    pub(crate) fn has_protocol_fallback(&self) -> bool {
        !matches!(self, Self::OpenAiCodex)
    }

    /// Can a 401 be recovered by re-reading credentials from their source?
    /// Codex tokens are file-based and can be refreshed by another process;
    /// env/config keys are static within a run.
    pub(crate) fn credentials_refreshable(&self) -> bool {
        matches!(self, Self::OpenAiCodex)
    }

    /// How this provider authenticates HTTP requests. One match arm is the
    /// whole seam: a new scheme (e.g. an `api-key` header for Azure-style
    /// endpoints) is a new `AuthScheme` variant + one `apply` arm — request
    /// call sites never change.
    pub(crate) fn auth_scheme(&self) -> AuthScheme {
        match self {
            Self::OpenAiCodex => AuthScheme::Codex,
            _ => AuthScheme::Bearer,
        }
    }
}

/// Request authentication scheme, resolved per provider by
/// [`Provider::auth_scheme`] and applied in exactly one place
/// (`client::authenticated_request`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum AuthScheme {
    /// `Authorization: Bearer <key>` — every provider except codex.
    Bearer,
    /// Bearer key plus the codex backend-api originator marker and
    /// account id.
    Codex,
}

impl AuthScheme {
    pub(crate) fn apply(
        self,
        request: reqwest::RequestBuilder,
        api_key: &str,
        account_id: Option<&str>,
    ) -> reqwest::RequestBuilder {
        let request = request.bearer_auth(api_key);
        match self {
            Self::Bearer => request,
            Self::Codex => {
                let mut request = request.header("originator", "codex_cli_rs");
                if let Some(account_id) = account_id {
                    request = request.header("ChatGPT-Account-ID", account_id);
                }
                request
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_spec_tables() {
        assert_eq!(
            Provider::OpenCode.endpoints().get("go").map(String::as_str),
            Some("https://opencode.ai/zen/go/v1")
        );
        assert!(Provider::OpenAiCodex.endpoints().is_empty());
        assert!(Provider::OpenCode.has_protocol_fallback());
        assert!(!Provider::OpenAiCodex.has_protocol_fallback());
        assert!(Provider::OpenAiCodex.credentials_refreshable());
        assert!(!Provider::OpenCode.credentials_refreshable());
        assert!(matches!(
            Provider::OpenCode.auth_scheme(),
            AuthScheme::Bearer
        ));
        assert!(matches!(
            Provider::OpenAiCodex.auth_scheme(),
            AuthScheme::Codex
        ));
        assert!(matches!(
            Provider::Generic("zai".into()).auth_scheme(),
            AuthScheme::Bearer
        ));
        // The scheme is the single application point: bearer sets the key,
        // codex adds its originator marker and account id.
        let req = Provider::OpenAiCodex
            .auth_scheme()
            .apply(
                reqwest::Client::new().get("http://localhost/v1"),
                "tok",
                Some("acct"),
            )
            .build()
            .unwrap();
        assert_eq!(
            req.headers()
                .get("authorization")
                .map(|v| v.to_str().unwrap()),
            Some("Bearer tok")
        );
        assert_eq!(
            req.headers().get("originator").map(|v| v.to_str().unwrap()),
            Some("codex_cli_rs")
        );
        assert_eq!(
            req.headers()
                .get("chatgpt-account-id")
                .map(|v| v.to_str().unwrap()),
            Some("acct")
        );
        assert_eq!(
            Provider::OpenCode.default_base_url(),
            Some("https://opencode.ai/zen/v1")
        );
        assert_eq!(
            Provider::OpenAiCodex.default_base_url(),
            Some("https://chatgpt.com/backend-api/codex")
        );
        assert_eq!(Provider::Generic("zai".into()).default_base_url(), None);
        // Pricing: a generic provider prices via its own catalog entry.
        assert_eq!(
            Provider::Generic("zai".into()).catalog_keys(),
            vec!["zai".to_string()]
        );
    }
}
