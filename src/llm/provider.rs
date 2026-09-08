//! Per-provider wiring. What differs by provider *kind* is a method here
//! (builtin tables); what differs by provider *configuration* (endpoint,
//! key, protocol pin, headers) is derived once into
//! `config::ResolvedProvider` from the `providers:` map + models.dev
//! catalog. Identity (`parse`/`name`) stays on the enum in `core::types`.

use std::collections::BTreeMap;

use crate::core::types::{ApiProtocol, Provider};

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
            // Native Messages API landing; the request path appends
            // `/v1/messages` (see `anthropic::messages_url`).
            Self::Anthropic => Some("https://api.anthropic.com"),
            Self::Generic(_) => None,
        }
    }

    /// Model used for a bare provider pick (`--model anthropic` with no
    /// model id). Defaults to [`DEFAULT_MODEL`] for OpenAI-compatible
    /// providers; native providers need one of their own family.
    pub(crate) fn default_model(&self) -> &'static str {
        match self {
            Self::Anthropic => "claude-sonnet-4-5",
            _ => DEFAULT_MODEL,
        }
    }

    /// Wire protocol default when neither the provider entry's `api:` pin
    /// nor the config file pins one. OpenAI-compatible providers share the
    /// responses default (with the empirical completions fallback); native
    /// providers pin their own wire.
    pub(crate) fn default_api(&self) -> Option<ApiProtocol> {
        match self {
            Self::Anthropic => Some(ApiProtocol::Anthropic),
            _ => None,
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
            Self::Anthropic => ["anthropic"].iter().map(|s| s.to_string()).collect(),
            // The catalog entry of the same key prices a generic provider.
            Self::Generic(name) => vec![name.clone()],
        }
    }

    /// Does the endpoint also speak chat-completions, so a rejected
    /// `/responses` call may be retried there? (Empirical protocol fallback.)
    pub(crate) fn has_protocol_fallback(&self) -> bool {
        !matches!(self, Self::OpenAiCodex | Self::Anthropic)
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
            Self::Anthropic => AuthScheme::Anthropic,
            _ => AuthScheme::Bearer,
        }
    }
}

/// Request authentication scheme, resolved per provider by
/// [`Provider::auth_scheme`] and applied in exactly one place
/// (`client::authenticated_request`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum AuthScheme {
    /// `Authorization: Bearer <key>` — every provider except codex/anthropic.
    Bearer,
    /// Bearer key plus the codex backend-api originator marker and
    /// account id.
    Codex,
    /// Anthropic Messages auth: `x-api-key` plus the pinned
    /// `anthropic-version` header — no `Authorization` header at all.
    Anthropic,
}

impl AuthScheme {
    pub(crate) fn apply(
        self,
        request: reqwest::RequestBuilder,
        api_key: &str,
        account_id: Option<&str>,
    ) -> reqwest::RequestBuilder {
        match self {
            Self::Bearer => request.bearer_auth(api_key),
            Self::Anthropic => request.header("x-api-key", api_key).header(
                "anthropic-version",
                crate::llm::anthropic::ANTHROPIC_VERSION,
            ),
            Self::Codex => {
                let mut request = request
                    .bearer_auth(api_key)
                    .header("originator", "codex_cli_rs");
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
            Provider::Anthropic.auth_scheme(),
            AuthScheme::Anthropic
        ));
        assert!(matches!(
            Provider::Generic("zai".into()).auth_scheme(),
            AuthScheme::Bearer
        ));
        // The scheme is the single application point: bearer sets the key,
        // codex adds its originator marker and account id, anthropic uses
        // x-api-key + the protocol version and no Authorization at all.
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
        let req = Provider::Anthropic
            .auth_scheme()
            .apply(
                reqwest::Client::new().get("http://localhost/v1"),
                "tok",
                None,
            )
            .build()
            .unwrap();
        assert_eq!(
            req.headers().get("x-api-key").map(|v| v.to_str().unwrap()),
            Some("tok")
        );
        assert_eq!(
            req.headers()
                .get("anthropic-version")
                .map(|v| v.to_str().unwrap()),
            Some(crate::llm::anthropic::ANTHROPIC_VERSION)
        );
        assert!(req.headers().get("authorization").is_none());
        assert_eq!(
            Provider::OpenCode.default_base_url(),
            Some("https://opencode.ai/zen/v1")
        );
        assert_eq!(
            Provider::OpenAiCodex.default_base_url(),
            Some("https://chatgpt.com/backend-api/codex")
        );
        assert_eq!(
            Provider::Anthropic.default_base_url(),
            Some("https://api.anthropic.com")
        );
        assert_eq!(Provider::Generic("zai".into()).default_base_url(), None);
        // Bare provider picks get a model of their own family.
        assert_eq!(Provider::Anthropic.default_model(), "claude-sonnet-4-5");
        assert_eq!(Provider::OpenCode.default_model(), DEFAULT_MODEL);
        // Native providers pin their own wire; OpenAI-compatible ones keep
        // the responses default + completions fallback.
        assert_eq!(
            Provider::Anthropic.default_api(),
            Some(ApiProtocol::Anthropic)
        );
        assert_eq!(Provider::OpenCode.default_api(), None);
        assert!(!Provider::Anthropic.has_protocol_fallback());
        // Pricing: a generic provider prices via its own catalog entry.
        assert_eq!(
            Provider::Generic("zai".into()).catalog_keys(),
            vec!["zai".to_string()]
        );
        assert_eq!(
            Provider::Anthropic.catalog_keys(),
            vec!["anthropic".to_string()]
        );
    }
}
