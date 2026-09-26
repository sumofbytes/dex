//! `config` unit tests (moved verbatim from the pre-split monolith).

use super::{
    apply_verify_optin, build_ctx_map, catalog_cache_missing, detect_verify_command, doctor,
    load_config_file, load_dex_models_cache, load_provider_entries, model_api_from_env,
    persist_selection, reasoning_options_for, remember_thinking_effort, stored_thinking_effort,
    unique_tmp_path, usage_cost, validate_thinking_effort, warn_provider_like_selection,
    write_ctx_index, ApiProtocol, LlmConfig, PermissionMode, Provider, ProviderEntry,
};
use crate::protocol::Usage;
use std::{collections::BTreeSet, env};
use unicode_width::UnicodeWidthStr;

#[test]
fn apply_model_routes_prefixed_selection_to_endpoint() {
    let mut cfg = test_cfg();
    // A second endpoint for the routing step under test — production
    // tables carry exactly one entry (the provider's own name).
    cfg.endpoints.insert(
        "go".to_string(),
        "https://opencode.ai/zen/go/v1".to_string(),
    );
    cfg.model = "gpt-5.6-luna".into();
    // Bare id keeps the current base_url.
    assert_eq!(cfg.apply_model("gpt-5.6-luna", false).unwrap(), None);
    assert_eq!(cfg.base_url, "https://opencode.ai/zen/v1");
    // Prefixed id routes to the endpoint and strips the prefix.
    assert_eq!(
        cfg.apply_model("go/kimi-k2", false).unwrap().as_deref(),
        Some("go")
    );
    assert_eq!(cfg.base_url, "https://opencode.ai/zen/go/v1");
    assert_eq!(cfg.model, "kimi-k2");
    // Provider-native slashes in model ids survive under an endpoint.
    assert_eq!(
        cfg.apply_model("go/moonshotai/kimi-k2", false)
            .unwrap()
            .as_deref(),
        Some("go")
    );
    assert_eq!(cfg.model, "moonshotai/kimi-k2");
    // Unknown prefix is a plain model id.
    assert_eq!(cfg.apply_model("unknown/m", false).unwrap(), None);
    assert_eq!(cfg.model, "unknown/m");
}

/// Synthetic models.dev catalog: the same id priced differently per
/// provider, with the endpoint-exact entry NOT first alphabetically.
fn write_cost_catalog(dir: &std::path::Path) {
    std::fs::create_dir_all(dir.join("dex")).unwrap();
    std::fs::write(
        dir.join("dex/models.dev.json"),
        serde_json::json!({
            "aaa-reseller": {
                "api": "https://aaa.example/v1",
                "models": {
                    "m-1": { "cost": { "input": 2.0 } },
                    "m-2": { "cost": { "input": 4.0 } }
                }
            },
            "opencode": {
                "api": "https://zen.example/v1",
                "models": {
                    "m-1": { "cost": { "input": 0.5 } },
                    "moonshotai/kimi-k2": { "cost": { "input": 0.5 } }
                }
            },
            "moonshotai": {
                "api": "https://moonshot.example/v1",
                "models": { "kimi-k2": { "cost": { "input": 1.0 } } }
            },
            "opencode-go": {
                "api": "https://go.example/v1",
                "models": {
                    "m-1": { "cost": { "input": 0.25, "cache_read": 0.0625, "output": 2.0 } }
                }
            }
        })
        .to_string(),
    )
    .unwrap();
}

#[test]
fn usage_cost_prefers_endpoint_then_provider_pricing() {
    // Serializes process-env redirection against other tests.
    let _lock = crate::test_env::TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join(format!("dex-cost-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write_cost_catalog(&dir);
    let prev_cache = env::var_os("XDG_CACHE_HOME");
    env::set_var("XDG_CACHE_HOME", &dir);
    let usage = |prompt: u64, completion: u64, cached: Option<u64>| Usage {
        prompt_tokens: prompt,
        completion_tokens: completion,
        cached_tokens: cached,
    };

    // Endpoint-exact match wins even though "aaa-reseller" sorts first.
    assert_eq!(
        usage_cost(
            "m-1",
            &Provider::Generic("opencode".to_string()),
            "https://go.example/v1",
            &usage(1_000_000, 0, None)
        ),
        Some(0.25)
    );
    // Fresh + cached + output are each billed at their own rate:
    // 0.6*0.25 + 0.4*0.0625 + 100k*2.0/1M.
    assert_eq!(
        usage_cost(
            "m-1",
            &Provider::Generic("opencode".to_string()),
            "https://go.example/v1",
            &usage(1_000_000, 100_000, Some(400_000))
        ),
        Some(0.375)
    );
    // No endpoint match: the configured provider's catalog keys win over
    // the global scan (0.5, not aaa-reseller's 2.0).
    assert_eq!(
        usage_cost(
            "m-1",
            &Provider::Generic("opencode".to_string()),
            "https://unrelated.example/v1",
            &usage(1_000_000, 0, None)
        ),
        Some(0.5)
    );
    // A catalog entry without an output rate bills output at the input
    // rate: 1M*0.5 + 1M*0.5.
    assert_eq!(
        usage_cost(
            "m-1",
            &Provider::Generic("opencode".to_string()),
            "https://unrelated.example/v1",
            &usage(1_000_000, 1_000_000, None)
        ),
        Some(1.0)
    );
    // Model only listed by another provider: global fallback still prices it.
    assert_eq!(
        usage_cost(
            "m-2",
            &Provider::OpenAiCodex,
            "https://x.example/v1",
            &usage(1_000_000, 0, None)
        ),
        Some(4.0)
    );

    match prev_cache {
        Some(v) => env::set_var("XDG_CACHE_HOME", v),
        None => env::remove_var("XDG_CACHE_HOME"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn resolve_model_cost_ignores_trailing_slash() {
    // Serializes process-env redirection against other tests. Hermetic
    // catalog via `XDG_CACHE_HOME` — the lookup is index-backed, so the
    // catalog arrives as a file, not a `Value`.
    let _lock = crate::test_env::TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join(format!("dex-cost-slash-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("dex")).unwrap();
    std::fs::write(
        dir.join("dex/models.dev.json"),
        serde_json::json!({
            "go": {
                "api": "https://go.example/v1",
                "models": {"m-1": {"cost": {"input": 1.0}}}
            }
        })
        .to_string(),
    )
    .unwrap();
    let _guard = EnvRestore::take(&["XDG_CACHE_HOME"]);
    std::env::set_var("XDG_CACHE_HOME", &dir);
    // A pinned `base_url` with a trailing slash still hits the
    // endpoint-exact tier instead of falling through to reseller pricing.
    let cost = super::resolve_model_cost("m-1", &["go".to_string()], "https://go.example/v1/");
    assert_eq!(cost.map(|c| c.input), Some(1.0));
    let _ = std::fs::remove_dir_all(&dir);
}

pub fn test_cfg() -> LlmConfig {
    LlmConfig {
        provider: Provider::Generic("opencode".to_string()),
        api_key: "k".into(),
        base_url: "https://opencode.ai/zen/v1".into(),
        model: "m-r".into(),
        available_models: Vec::new(),
        // Production shape: a generic exposes exactly one endpoint, named
        // after the provider (`endpoints_for`) — tests that need a second
        // one insert it explicitly.
        endpoints: [(
            "opencode".to_string(),
            "https://opencode.ai/zen/v1".to_string(),
        )]
        .into_iter()
        .collect(),
        api: ApiProtocol::Responses,
        api_pinned: false,
        account_id: None,
        thinking_effort: None,
        context_window: 128_000,
        reserve_tokens: 16_384,
        keep_recent_tokens: 20_000,
        permission: PermissionMode::Ask,
        verify_command: None,
        extra_headers: Default::default(),
        global_headers: Default::default(),
        connect_timeout_secs: 10,
        request_timeout_secs: 300,
        // The configured generic provider the fixture rides: its entry
        // supplies the key and the landing (generics have no builtin URL).
        provider_entries: [
            (
                "opencode".to_string(),
                ProviderEntry {
                    api_key: Some("k".to_string()),
                    base_url: Some("https://opencode.ai/zen/v1".to_string()),
                    ..ProviderEntry::default()
                },
            ),
            (
                "opencode-go".to_string(),
                ProviderEntry {
                    api_key: Some("k".to_string()),
                    base_url: Some("https://opencode.ai/zen/go/v1".to_string()),
                    ..ProviderEntry::default()
                },
            ),
        ]
        .into_iter()
        .collect(),
        provider_headers: Default::default(),
    }
}

/// Save/restore process env around tests that redirect dex env vars.
struct EnvRestore {
    vars: Vec<(&'static str, Option<std::ffi::OsString>)>,
}

impl EnvRestore {
    fn take(keys: &[&'static str]) -> Self {
        Self {
            vars: keys.iter().map(|k| (*k, std::env::var_os(k))).collect(),
        }
    }
}

impl Drop for EnvRestore {
    fn drop(&mut self) {
        for (key, prev) in self.vars.drain(..) {
            match prev {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
    }
}

/// DEX_MODEL_APIS table with `DEX_API` unpinned so resolution runs.
fn model_apis_env(table: &str) -> EnvRestore {
    let guard = EnvRestore::take(&["DEX_MODEL_APIS"]);
    std::env::set_var("DEX_MODEL_APIS", table);
    guard
}

#[test]
fn apply_model_follows_model_apis() {
    // Serializes process-env redirection (DEX_MODEL_APIS/DEX_API)
    // against daemon tests holding the same guard.
    let _env = crate::test_env::TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _guard = model_apis_env("m-c=openai-completions");
    let mut cfg = test_cfg();
    cfg.apply_model("m-c", false).unwrap();
    assert_eq!(cfg.api, ApiProtocol::ChatCompletions);
    // Unknown ids keep the current protocol.
    cfg.apply_model("m-r", false).unwrap();
    assert_eq!(cfg.api, ApiProtocol::ChatCompletions);
}

#[test]
fn apply_model_full_selection_key_wins() {
    let _env = crate::test_env::TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _guard = model_apis_env("m-x=openai-completions,go/m-x=openai-responses");
    let mut cfg = test_cfg();
    // The `endpoint/id` key needs a second endpoint; production tables
    // carry one entry (the provider's own name), so insert it explicitly.
    cfg.endpoints.insert(
        "go".to_string(),
        "https://opencode.ai/zen/go/v1".to_string(),
    );
    cfg.apply_model("go/m-x", false).unwrap();
    assert_eq!(cfg.base_url, "https://opencode.ai/zen/go/v1");
    assert_eq!(cfg.model, "m-x");
    assert_eq!(cfg.api, ApiProtocol::Responses);
    cfg.apply_model("m-x", false).unwrap();
    assert_eq!(cfg.api, ApiProtocol::ChatCompletions);
}

#[test]
fn bare_unconfigured_catalog_provider_resolves_and_warns() {
    // A selection naming an unconfigured models.dev provider ("zai",
    // bare or `zai/model`) still rides the current provider as an
    // opaque id at runtime: a one-time hint names the fix (at startup
    // the same selection fails with the deposit pointer instead).
    // Already-served and known native ids are not mistakes.
    let _env = crate::test_env::TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join(format!("dex-providerlike-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write_cost_catalog(&dir);
    let _cache = EnvRestore::take(&["XDG_CACHE_HOME"]);
    std::env::set_var("XDG_CACHE_HOME", &dir);
    let mut cfg = test_cfg();
    // Unconfigured provider key: warns; the bare name still rides the
    // current endpoint as a model id.
    assert!(warn_provider_like_selection(
        "aaa-reseller",
        "opencode",
        &["m-1".to_string()]
    ));
    cfg.apply_model("aaa-reseller", false).unwrap();
    assert_eq!(cfg.model, "aaa-reseller");
    assert_eq!(cfg.base_url, "https://opencode.ai/zen/v1");
    // A model id the gateway serves is fine.
    assert!(!warn_provider_like_selection(
        "m-1",
        "opencode",
        &["m-1".to_string()]
    ));
    // A qualified pick off an unconfigured provider warns too — it
    // rides the fallback as an opaque id just like the bare name.
    assert!(warn_provider_like_selection(
        "aaa-reseller/m-2",
        "opencode",
        &[]
    ));
    // A known native `org/model` id sharing its prefix with a provider
    // is legit — no warning. Unknown prefixes and the resolved
    // provider itself are exempt too.
    assert!(!warn_provider_like_selection(
        "moonshotai/kimi-k2",
        "opencode",
        &[]
    ));
    assert!(!warn_provider_like_selection(
        "unknown/m-1",
        "opencode",
        &[]
    ));
    assert!(!warn_provider_like_selection("opencode", "opencode", &[]));
}

#[test]
fn dex_model_bare_provider_name_resolves_as_model_id() {
    // `opencode/aaa-reseller` (an unconfigured catalog provider as the id)
    // is not a provider switch: it rides the configured provider as a model
    // id and the config builds — the runtime hint warns instead of
    // erroring. A bare `aaa-reseller` carries no provider at all: the error
    // names the provider it looks like and the deposit to configure.
    let _env = crate::test_env::TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join(format!("dex-bareprov-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    write_cost_catalog(&dir.join("cache"));
    let _guard = EnvRestore::take(&[
        "DEX_CONFIG",
        "DEX_MODEL",
        "OPENCODE_API_KEY",
        "XDG_CACHE_HOME",
        "DEX_CONTEXT_WINDOW",
    ]);
    std::fs::write(
        dir.join("config.yaml"),
        "model: opencode/m\ncontext_window: 1000\nproviders:\n  opencode:\n    base_url: https://opencode.ai/zen/v1\n    api_key: test-key\n",
    )
    .unwrap();
    std::env::set_var("XDG_CACHE_HOME", dir.join("cache"));
    std::env::set_var("DEX_CONFIG", dir.join("config.yaml"));
    std::env::remove_var("OPENCODE_API_KEY");
    super::invalidate_config_cache();
    std::env::set_var("DEX_CONTEXT_WINDOW", "1000");
    let err = match LlmConfig::from_env(None, Some("aaa-reseller".to_string()), None, &[]) {
        Err(e) => e.to_string(),
        Ok(_) => panic!("expected an unrouted-selection error for a bare id"),
    };
    assert!(err.contains("looks like provider 'aaa-reseller'"), "{err}");
    let cfg =
        LlmConfig::from_env(None, Some("opencode/aaa-reseller".to_string()), None, &[]).unwrap();
    assert_eq!(cfg.model, "aaa-reseller");
    assert_eq!(cfg.provider, Provider::Generic("opencode".to_string()));
    assert_eq!(cfg.base_url, "https://opencode.ai/zen/v1");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn model_api_from_env_ignores_malformed_entries() {
    let _env = crate::test_env::TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _guard = EnvRestore::take(&["DEX_MODEL_APIS"]);
    std::env::remove_var("DEX_MODEL_APIS");
    assert_eq!(
        model_api_from_env("m-c", "m-c"),
        None,
        "unset table resolves nothing"
    );
    std::env::set_var("DEX_MODEL_APIS", "junk-no-equals,m-c=bogus-api,m-c=chat");
    assert_eq!(
        model_api_from_env("m-c", "m-c"),
        Some(ApiProtocol::ChatCompletions),
        "first parseable match wins"
    );
}

#[test]
fn provider_parse_accepts_aliases() {
    let known: BTreeSet<String> = ["zai".to_string(), "opencode".to_string()]
        .into_iter()
        .collect();
    let assert_known = |name: &str| Provider::parse_known(name, &known).unwrap();
    assert_eq!(
        assert_known("opencode"),
        Provider::Generic("opencode".to_string())
    );
    assert_eq!(assert_known("codex"), Provider::OpenAiCodex);
    assert_eq!(assert_known("openai-codex"), Provider::OpenAiCodex);
    assert_eq!(assert_known("zai"), Provider::Generic("zai".to_string()));
    assert!(Provider::parse_known("unknown", &known).is_none());
    // No "openai" alias: a generic `providers.openai` entry is the way
    // to point that name at an OpenAI-compatible endpoint.
    assert!(Provider::parse_known("openai", &known).is_none());
    assert_eq!(
        Provider::Generic("opencode".to_string()).default_base_url(),
        None,
        "generics have no built-in landing"
    );
    assert!(Provider::Generic("opencode".to_string())
        .endpoints()
        .is_empty());
    assert_eq!(
        Provider::OpenAiCodex.default_base_url(),
        Some("https://chatgpt.com/backend-api/codex")
    );
}

#[test]
fn selection_trims_whitespace_around_provider_and_model() {
    let known: BTreeSet<String> = ["zai".to_string()].into_iter().collect();
    // The provider side already trims via `parse_known`; a stray space
    // on the model side must not survive into the catalog lookup.
    assert_eq!(
        super::split_selection("zai/ glm-x", &known),
        Ok((Some("zai".to_string()), "glm-x".to_string()))
    );
    assert_eq!(
        super::split_selection(" zai / glm-x ", &known),
        Ok((Some("zai".to_string()), "glm-x".to_string()))
    );
}

#[test]
fn apply_model_switches_provider_and_sets_base_url_without_env() {
    let _env = crate::test_env::TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _g = EnvRestore::take(&["OPENCODE_API_KEY", "CODEX_ACCESS_TOKEN", "CODEX_ACCOUNT_ID"]);
    std::env::set_var("OPENCODE_API_KEY", "test-key");
    std::env::set_var("CODEX_ACCESS_TOKEN", "codex-tok");
    let mut cfg = test_cfg();
    // starts on the opencode provider's landing
    assert_eq!(cfg.provider, Provider::Generic("opencode".to_string()));
    // Switch to codex via provider-qualified model — no env base_url required
    cfg.apply_model("openai-codex/gpt-5.6-luna", false).unwrap();
    assert_eq!(cfg.provider, Provider::OpenAiCodex);
    assert_eq!(
        cfg.base_url,
        Provider::OpenAiCodex.default_base_url().unwrap()
    );
    assert_eq!(cfg.model, "gpt-5.6-luna");
    // Switch back via the provider prefix
    cfg.apply_model("opencode/gpt-4o", false).unwrap();
    assert_eq!(cfg.provider, Provider::Generic("opencode".to_string()));
    assert_eq!(cfg.base_url, "https://opencode.ai/zen/v1");
    assert_eq!(cfg.model, "gpt-4o");
    // Sibling provider: the switch lands on that provider's own endpoint
    // (a generic provider exposes exactly one, under its own name).
    cfg.apply_model("opencode-go/kimi-k2", false).unwrap();
    assert_eq!(cfg.provider, Provider::Generic("opencode-go".to_string()));
    assert_eq!(cfg.base_url, "https://opencode.ai/zen/go/v1");
    assert_eq!(cfg.model, "kimi-k2");
    // A bare endpoint prefix still routes without a provider prefix (the
    // second endpoint is inserted explicitly: production tables carry one
    // entry, the provider's own name).
    cfg = test_cfg();
    cfg.endpoints.insert(
        "go".to_string(),
        "https://opencode.ai/zen/go/v1".to_string(),
    );
    cfg.apply_model("go/kimi-k2", false).unwrap();
    assert_eq!(cfg.base_url, "https://opencode.ai/zen/go/v1");
    assert_eq!(cfg.model, "kimi-k2");
    // Bare provider name switches provider and keeps the model.
    cfg = test_cfg();
    cfg.apply_model("gpt-4o", false).unwrap();
    cfg.apply_model("openai-codex", false).unwrap();
    assert_eq!(cfg.provider, Provider::OpenAiCodex);
    assert_eq!(
        cfg.base_url,
        Provider::OpenAiCodex.default_base_url().unwrap()
    );
    assert_eq!(cfg.model, "gpt-4o");
    cfg.apply_model("opencode", false).unwrap();
    assert_eq!(cfg.provider, Provider::Generic("opencode".to_string()));
    assert_eq!(cfg.model, "gpt-4o");
    assert_eq!(cfg.base_url, "https://opencode.ai/zen/v1");
}

#[test]
fn endpoints_always_available_for_opencode_without_env() {
    let _env = crate::test_env::TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join(format!("dex-oc-eps-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write_cost_catalog(&dir);
    let _g = EnvRestore::take(&[
        "OPENCODE_API_KEY",
        "DEX_MODELS",
        "DEX_CONFIG",
        "DEX_MODEL",
        "DEX_CONTEXT_WINDOW",
        "XDG_CACHE_HOME",
    ]);
    std::fs::write(
        dir.join("config.yaml"),
        "model: opencode/m\nproviders:\n  opencode:\n    api_key: test-key2\n",
    )
    .unwrap();
    std::env::set_var("OPENCODE_API_KEY", "test-key2");
    std::env::remove_var("DEX_MODELS");
    std::env::remove_var("DEX_MODEL");
    std::env::set_var("DEX_CONTEXT_WINDOW", "1000");
    std::env::set_var("XDG_CACHE_HOME", &dir);
    std::env::set_var("DEX_CONFIG", dir.join("config.yaml"));
    let cfg = LlmConfig::from_env(None, None, None, &[]).unwrap();
    // The catalog entry supplies the landing and the named endpoints;
    // nothing is hard-coded into dex.
    assert_eq!(cfg.base_url, "https://zen.example/v1");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn custom_headers_parse_json_and_pairs() {
    use super::parse_headers_str;
    let json = parse_headers_str(r#"{"X-Gateway-Key":"abc","X-Empty":"","num":42}"#);
    assert_eq!(json.get("X-Gateway-Key").map(String::as_str), Some("abc"));
    assert_eq!(json.get("num").map(String::as_str), Some("42"));
    assert!(!json.contains_key("X-Empty"), "empty values are skipped");
    assert!(parse_headers_str("  ").is_empty());

    let pairs = parse_headers_str("X-Foo: bar, X-Baz=qux");
    assert_eq!(pairs.get("X-Foo").map(String::as_str), Some("bar"));
    assert_eq!(pairs.get("X-Baz").map(String::as_str), Some("qux"));

    // Newline-separated pairs may contain commas in values.
    let multi = parse_headers_str("X-A: one, two\nX-B: three");
    assert_eq!(multi.get("X-A").map(String::as_str), Some("one, two"));
    assert_eq!(multi.get("X-B").map(String::as_str), Some("three"));

    // Malformed entries are skipped, later duplicates win.
    let messy = parse_headers_str("no-separator, : novalue, X-K: 1, X-K: 2");
    assert_eq!(messy.len(), 1);
    assert_eq!(messy.get("X-K").map(String::as_str), Some("2"));

    // Case-insensitive duplicates collapse (last casing/value wins).
    let ci = parse_headers_str("X-Foo: 1, x-foo: 2");
    assert_eq!(ci.len(), 1);
    assert_eq!(ci.get("x-foo").map(String::as_str), Some("2"));

    // `authorization` never lands in the map (api key owns it).
    assert!(parse_headers_str("Authorization: hacked").is_empty());
    assert!(parse_headers_str(r#"{"authorization":"hacked"}"#).is_empty());

    // A `{...}` value that isn't a JSON object falls back to pairs.
    let fb = parse_headers_str("{bad json");
    assert!(fb.is_empty(), "no separator means no pairs either");
    let fb = parse_headers_str("{X-Foo: bar}");
    assert_eq!(fb.get("X-Foo").map(String::as_str), Some("bar"));
}

#[test]
fn custom_headers_layer_file_env_cli() {
    use std::collections::BTreeMap;

    let _env = crate::test_env::TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _g = EnvRestore::take(&[
        "OPENCODE_API_KEY",
        "DEX_MODELS",
        "DEX_CONFIG",
        "DEX_HEADERS",
        "OPENAI_HEADERS",
        "ANTHROPIC_CUSTOM_HEADERS",
    ]);
    // Config file: `headers:` wins per-key over `http_headers:`.
    let dir = std::env::temp_dir().join(format!("dex-headers-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let cfg_path = dir.join("config.yaml");
    std::fs::write(
            &cfg_path,
            "model: opencode/m-h\ncontext_window: 1000\nproviders:\n  opencode:\n    base_url: https://opencode.example/v1\n    api_key: test-key\nhttp_headers:\n  X-File: file\n  X-Shared: http\nheaders:\n  X-Shared: file\n",
        )
        .unwrap();
    std::env::set_var("DEX_CONFIG", &cfg_path);
    std::env::set_var("OPENCODE_API_KEY", "test-key");
    std::env::remove_var("DEX_MODELS");
    std::env::set_var("DEX_HEADERS", "X-Env: env");
    std::env::set_var("OPENAI_HEADERS", "X-Env2: openai");
    std::env::set_var("ANTHROPIC_CUSTOM_HEADERS", "X-Env3: env3");
    let cfg = LlmConfig::from_env(
        None,
        None,
        None,
        &["X-Cli: cli".to_string(), "X-Shared: cli".to_string()],
    )
    .unwrap();
    assert_eq!(cfg.model, "m-h");
    // File layers land in `global_headers` (lowest precedence on the
    // wire); `http_headers:` loses to `headers:` per key inside that map.
    assert_eq!(
        cfg.global_headers.get("X-File").map(String::as_str),
        Some("file")
    );
    assert_eq!(
        cfg.global_headers.get("X-Shared").map(String::as_str),
        Some("file")
    );
    // Env / CLI land in `extra_headers` — above both file layers.
    assert_eq!(
        cfg.extra_headers.get("X-Env").map(String::as_str),
        Some("env")
    );
    assert_eq!(
        cfg.extra_headers.get("X-Env2").map(String::as_str),
        Some("openai")
    );
    assert_eq!(
        cfg.extra_headers.get("X-Env3").map(String::as_str),
        Some("env3")
    );
    assert_eq!(
        cfg.extra_headers.get("X-Cli").map(String::as_str),
        Some("cli")
    );
    assert!(!cfg.extra_headers.contains_key("X-File"));
    // The wire merge orders the layers: CLI wins over both config-file
    // spellings, file keys survive where nothing above them speaks.
    let merged: BTreeMap<String, String> = crate::llm::http::merged_headers(
        &cfg.global_headers,
        &cfg.provider_headers,
        &cfg.extra_headers,
    )
    .into_iter()
    .map(|(name, value)| {
        (
            name.as_str().to_string(),
            value.to_str().unwrap_or("").to_string(),
        )
    })
    .collect();
    assert_eq!(merged.get("x-shared").map(String::as_str), Some("cli"));
    assert_eq!(merged.get("x-file").map(String::as_str), Some("file"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn custom_headers_config_text_and_list_shapes() {
    use super::load_config_headers;
    // Text-header block (same syntax as env vars / `--header`).
    let file: Option<serde_yaml::Value> =
        Some(serde_yaml::from_str("headers: \"X-A: one\\nX-B: two, three\"\n").unwrap());
    let out = load_config_headers(&file);
    assert_eq!(out.get("X-A").map(String::as_str), Some("one"));
    assert_eq!(out.get("X-B").map(String::as_str), Some("two, three"));
    // List mixing text entries and one-key maps; later entries win.
    let file: Option<serde_yaml::Value> = Some(
        serde_yaml::from_str("http_headers:\n  - \"X-A: 1\"\n  - X-A: 2\n  - X-C: 3\n").unwrap(),
    );
    let out = load_config_headers(&file);
    assert_eq!(out.get("X-A").map(String::as_str), Some("2"));
    assert_eq!(out.get("X-C").map(String::as_str), Some("3"));
    // Non-string scalars and blank entries are skipped.
    let file: Option<serde_yaml::Value> =
        Some(serde_yaml::from_str("headers:\n  X-N: 42\n  X-E: \"\"\n").unwrap());
    let out = load_config_headers(&file);
    assert_eq!(out.get("X-N").map(String::as_str), Some("42"));
    assert!(!out.contains_key("X-E"));
}

#[test]
fn opencode_session_headers_gated_and_explicit_wins() {
    use super::apply_opencode_session_headers;
    // The opencode provider name + session id → both headers.
    let mut cfg = test_cfg();
    apply_opencode_session_headers(&mut cfg, "sess-1");
    assert_eq!(
        cfg.extra_headers
            .get("x-opencode-session")
            .map(String::as_str),
        Some("sess-1")
    );
    assert_eq!(
        cfg.extra_headers
            .get("x-opencode-client")
            .map(String::as_str),
        Some("dex")
    );
    // Generic provider on another host → nothing.
    let mut cfg = test_cfg();
    cfg.provider = Provider::Generic("other".to_string());
    cfg.base_url = "https://other.example/v1".into();
    apply_opencode_session_headers(&mut cfg, "sess-1");
    assert!(cfg.extra_headers.is_empty());
    // Generic provider pointed at opencode.ai → headers (host fallback).
    let mut cfg = test_cfg();
    cfg.provider = Provider::Generic("proxy".to_string());
    apply_opencode_session_headers(&mut cfg, "sess-1");
    assert_eq!(cfg.extra_headers.len(), 2);
    // Empty session id → nothing, even for opencode.
    let mut cfg = test_cfg();
    apply_opencode_session_headers(&mut cfg, "  ");
    assert!(cfg.extra_headers.is_empty());
    // Explicit user header wins (any casing); client header still fills.
    let mut cfg = test_cfg();
    cfg.extra_headers
        .insert("X-Opencode-Session".to_string(), "mine".to_string());
    apply_opencode_session_headers(&mut cfg, "sess-1");
    assert_eq!(
        cfg.extra_headers
            .get("X-Opencode-Session")
            .map(String::as_str),
        Some("mine")
    );
    assert_eq!(
        cfg.extra_headers
            .get("x-opencode-client")
            .map(String::as_str),
        Some("dex")
    );
    // A FILE-layer pin must suppress the auto-fill too: `extra_headers`
    // merges after both file layers, so injecting here would silently
    // override the user's config-file header.
    let mut cfg = test_cfg();
    cfg.provider_headers
        .insert("x-opencode-session".to_string(), "file".to_string());
    apply_opencode_session_headers(&mut cfg, "sess-1");
    assert!(!cfg.extra_headers.contains_key("x-opencode-session"));
    assert_eq!(
        cfg.extra_headers
            .get("x-opencode-client")
            .map(String::as_str),
        Some("dex")
    );
}

#[test]
fn permission_parse_and_ordering() {
    assert_eq!(
        PermissionMode::parse("read-only").unwrap(),
        PermissionMode::ReadOnly
    );
    assert_eq!(
        PermissionMode::parse("readonly").unwrap(),
        PermissionMode::ReadOnly
    );
    assert_eq!(
        PermissionMode::parse("ask_writes").unwrap(),
        PermissionMode::Ask
    );
    assert_eq!(
        PermissionMode::parse("trusted").unwrap(),
        PermissionMode::Trusted
    );
    assert!(PermissionMode::parse("nope").is_err());
    assert!(PermissionMode::ReadOnly.permissiveness() < PermissionMode::Trusted.permissiveness());
}

/// An explicit `verify_command` is never overwritten — even with the
/// opt-in env set. The auto-detect branch itself is cwd-dependent and
/// covered by `detect_verify_command_selects_by_manifest`.
#[test]
fn apply_verify_optin_explicit_command_wins() {
    let _lock = crate::test_env::TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let mut config = test_cfg();
    config.verify_command = Some("make check".into());
    apply_verify_optin(&mut config);
    assert_eq!(config.verify_command.as_deref(), Some("make check"));
    let _env = EnvRestore::take(&["DEX_VERIFY"]);
    std::env::set_var("DEX_VERIFY", "1");
    apply_verify_optin(&mut config);
    assert_eq!(config.verify_command.as_deref(), Some("make check"));
}

#[test]
fn detect_verify_command_selects_by_manifest() {
    // Isolated temp dir without manifests -> None
    let prev = std::env::current_dir().unwrap();
    let tmp = std::env::temp_dir().join(format!("dex-verify-none-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&tmp);
    std::env::set_current_dir(&tmp).unwrap();
    assert_eq!(detect_verify_command(), None);
    // Cargo.toml -> cargo test
    std::fs::write(tmp.join("Cargo.toml"), "[package]").unwrap();
    assert_eq!(detect_verify_command().as_deref(), Some("cargo test"));
    let _ = std::fs::remove_file(tmp.join("Cargo.toml"));
    std::fs::write(tmp.join("go.mod"), "module x").unwrap();
    assert_eq!(detect_verify_command().as_deref(), Some("go test ./..."));
    let _ = std::fs::remove_file(tmp.join("go.mod"));
    std::fs::write(tmp.join("package.json"), "{}").unwrap();
    assert_eq!(detect_verify_command().as_deref(), Some("npm test"));
    // Restore the cwd *before* deleting the temp dir: this runs
    // concurrently with other tests, and a deleted process cwd makes
    // `current_dir()` return None for them.
    std::env::set_current_dir(prev).unwrap();
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn catalog_cache_missing_reports_fresh_installs() {
    let _env = crate::test_env::TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _guard = EnvRestore::take(&["XDG_CACHE_HOME"]);
    let dir = std::env::temp_dir().join(format!("dex-catalog-missing-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("dex")).unwrap();
    std::env::set_var("XDG_CACHE_HOME", &dir);
    let path = dir.join("dex/models.dev.json");
    // No file (fresh install) or a zero-length file (torn write): missing.
    assert!(catalog_cache_missing());
    std::fs::write(&path, "").unwrap();
    assert!(catalog_cache_missing());
    // Any non-empty catalog counts as present.
    std::fs::write(&path, "{}").unwrap();
    assert!(!catalog_cache_missing());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn concurrent_ctx_index_writes_leave_no_torn_file() {
    let _env = crate::test_env::TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _guard = EnvRestore::take(&["XDG_CACHE_HOME"]);
    // tmp paths are unique per call even within one process — a shared
    // name lets one writer rename another's half-written tmp file.
    let base = std::path::PathBuf::from("/tmp/dex");
    assert_ne!(unique_tmp_path(&base), unique_tmp_path(&base));
    let dir = std::env::temp_dir().join(format!("dex-ctx-index-race-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("dex")).unwrap();
    std::env::set_var("XDG_CACHE_HOME", &dir);
    // Four concurrent writers on one destination. Maps are large enough
    // that a write spans syscalls, so a shared tmp name could publish
    // another writer's half-written bytes.
    let mk_map = |ctx: u64| -> std::collections::BTreeMap<String, u64> {
        (0..20_000u32)
            .map(|i| (format!("model-{i}"), ctx))
            .collect()
    };
    for _ in 0..4 {
        std::thread::scope(|s| {
            for ctx in [1000u64, 2000, 3000, 4000] {
                let map = mk_map(ctx);
                s.spawn(move || write_ctx_index(&map));
            }
        });
        let path = dir.join("dex/models.ctx.json");
        let text = std::fs::read_to_string(&path).unwrap();
        let parsed: std::collections::BTreeMap<String, u64> =
            serde_json::from_str(&text).expect("index must always parse");
        assert_eq!(parsed.len(), 20_000);
        assert!(
            [1000, 2000, 3000, 4000].contains(parsed.values().next().unwrap()),
            "torn or foreign index contents"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn builtin_default_missing_key_suggests_setup() {
    // Nothing selects a provider (no flag/env/file pointer): the
    // missing-key error must guide setup, not endorse opencode.
    let _env = crate::test_env::TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _guard = EnvRestore::take(&[
        "DEX_CONFIG",
        "DEX_MODEL",
        "OPENCODE_API_KEY",
        "ANTHROPIC_API_KEY",
        "DEX_MODEL_APIS",
        "DEX_MODELS",
        "DEX_CONTEXT_WINDOW",
        "XDG_CACHE_HOME",
    ]);
    let dir = std::env::temp_dir().join(format!("dex-nokey-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("dex")).unwrap();
    std::fs::write(dir.join("dex/models.dev.json"), "{}").unwrap();
    std::env::set_var("XDG_CACHE_HOME", &dir);
    // Point at a path that does not exist: no file `model:`.
    std::env::set_var("DEX_CONFIG", dir.join("config.yaml"));
    for key in ["DEX_MODEL", "OPENCODE_API_KEY", "ANTHROPIC_API_KEY"] {
        std::env::remove_var(key);
    }
    let err = match LlmConfig::from_env(None, None, None, &[]) {
        Err(e) => e.to_string(),
        Ok(_) => panic!("expected setup-guide error"),
    };
    assert!(err.contains("no model configured"), "{err}");
    assert!(err.contains("anthropic"), "{err}");
    assert!(err.contains("gateway"), "{err}");
    assert!(err.contains("openai-codex"), "{err}");
    assert!(!err.contains("no API key for provider"), "{err}");
    // The same guide surfaces in `dex doctor`'s resolve row.
    let report = doctor(None, None, None, &[], None).0;
    assert!(report.contains("no model configured"), "{report}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn explicit_base_url_without_selection_routes_to_custom_provider() {
    // `--base-url` + no provider pointer anywhere must NOT ride the
    // builtin default: key errors name providers.custom.api_key, and
    // no OPENCODE_API_KEY is demanded for a foreign endpoint.
    let _env = crate::test_env::TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join(format!("dex-custom-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("dex")).unwrap();
    std::fs::write(dir.join("dex/models.dev.json"), "{}").unwrap();
    std::env::set_var("XDG_CACHE_HOME", &dir);
    std::env::set_var("DEX_CONFIG", dir.join("config.yaml"));
    for key in ["DEX_MODEL", "OPENCODE_API_KEY", "ANTHROPIC_API_KEY"] {
        std::env::remove_var(key);
    }
    let err = match LlmConfig::from_env(
        Some("http://localhost:11434/v1".to_string()),
        None,
        None,
        &[],
    ) {
        Err(e) => e.to_string(),
        Ok(_) => panic!("expected missing-key error for providers.custom"),
    };
    assert!(err.contains("providers.custom.api_key"), "{err}");
    assert!(!err.contains("opencode.api_key"), "{err}");
    assert!(!err.contains("OPENCODE_API_KEY"), "{err}");
    // With providers.custom.api_key set, resolution succeeds on the
    // pinned URL with the default model.
    std::fs::write(
        dir.join("config.yaml"),
        "providers:\n  custom:\n    api_key: kk\n",
    )
    .unwrap();
    // No builtin default model exists: the build demands an explicit
    // model even with the key and URL in place.
    let err = match LlmConfig::from_env(
        Some("http://localhost:11434/v1".to_string()),
        None,
        None,
        &[],
    ) {
        Err(e) => e.to_string(),
        Ok(_) => panic!("expected no-model-configured error"),
    };
    assert!(err.contains("no model configured"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn doctor_names_custom_provider_for_base_url_only_setup() {
    let _env = crate::test_env::TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _guard = EnvRestore::take(&["DEX_CONFIG", "DEX_MODEL", "OPENCODE_API_KEY"]);
    let dir = std::env::temp_dir().join(format!("dex-doc-custom-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("dex")).unwrap();
    std::fs::write(dir.join("dex/models.dev.json"), "{}").unwrap();
    std::env::set_var("XDG_CACHE_HOME", &dir);
    std::env::set_var("DEX_CONFIG", dir.join("config.yaml"));
    for key in ["OPENCODE_API_KEY", "DEX_MODEL"] {
        std::env::remove_var(key);
    }
    let out = doctor(
        Some("http://localhost:11434/v1".to_string()),
        None,
        None,
        &[],
        None,
    )
    .0;
    let prow = out.lines().find(|l| l.starts_with("provider ")).unwrap();
    assert!(prow.contains("custom"), "{prow}");
    assert!(prow.contains("--base-url"), "{prow}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn setup_guide_lists_every_builtin_first() {
    // The guide leads with a concrete gateway shape, then builtins, then
    // the generic shapes: opencode, anthropic, a generic gateway, a custom
    // endpoint, then codex.
    let guide = super::setup_guide_error();
    let opencode = guide.find("quickest (OpenCode Zen gateway)").unwrap();
    let anthropic = guide.find("anthropic: model:").unwrap();
    let gateway = guide.find("other gateways: model:").unwrap();
    let custom = guide.find("custom endpoint (Bearer").unwrap();
    let codex = guide.find("codex: model:").unwrap();
    assert!(
        opencode < anthropic && anthropic < gateway && gateway < custom && custom < codex,
        "{guide}"
    );
}

#[test]
fn provider_samples_stay_consistent() {
    // `examples/config.yaml` is what the setup guide points at: it must
    // parse, its active block must be self-consistent (the `model:` prefix
    // names its `providers:` entry or a builtin), and every advertised
    // provider must still have a block.
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../examples/config.yaml"
    ))
    .expect("examples/config.yaml must exist");
    let parsed: serde_yaml::Value = serde_yaml::from_str(&text).expect("samples file must parse");
    let model = parsed
        .get("model")
        .and_then(|v| v.as_str())
        .expect("samples file needs one active model:");
    let (prefix, rest) = model
        .split_once('/')
        .expect("active sample model must be provider-qualified");
    assert!(!rest.trim().is_empty(), "active sample needs a model id");
    let builtin = ["anthropic", "openai-codex", "codex"];
    if !builtin.contains(&prefix) {
        assert!(
            parsed
                .get("providers")
                .and_then(|p| p.get(prefix))
                .is_some(),
            "active sample provider '{prefix}' needs a providers: entry"
        );
    }
    for provider in [
        "opencode",
        "anthropic",
        "openai-codex",
        "google",
        "openai",
        "deepseek",
        "moonshotai",
        "openrouter",
        "commandcode",
    ] {
        assert!(
            text.contains(provider),
            "samples file lost its {provider} block"
        );
    }
    // Every sample block names its own `model:` selection (active or
    // commented): a renamed provider with a stale model line would
    // otherwise pass the substring check above.
    for provider in [
        "opencode/",
        "anthropic/",
        "openai-codex/",
        "google/",
        "openai/",
        "deepseek/",
        "moonshotai/",
        "openrouter/",
        "commandcode/",
    ] {
        assert!(
            text.contains(&format!("model: {provider}")),
            "samples file lost its `{provider}` model line"
        );
    }
    // Providers with no catalog endpoint must document their `base_url:`
    // (and the custom-gateway template its `context_window:` fallback,
    // since nothing sizes a model outside the catalog).
    for url in [
        "https://generativelanguage.googleapis.com/v1beta/openai/",
        "https://api.openai.com/v1",
        "https://commandcode.example/v1",
        "https://gateway.example/v1",
    ] {
        assert!(text.contains(url), "samples file lost base_url {url}");
    }
    assert!(
        text.contains("api: anthropic-messages"),
        "samples file lost its Anthropic-wire pin"
    );
    assert!(
        text.contains("context_window:"),
        "samples file lost its custom-gateway context_window hint"
    );
    // The guide's concrete happy-path id must match the active sample, so
    // the two cannot drift apart.
    assert!(
        super::setup_guide_error().contains(&format!("model: {model}")),
        "guide and samples disagree on the happy path"
    );
}

#[test]
fn unrouted_selection_error_names_the_fix() {
    let known: BTreeSet<String> = ["opencode".to_string(), "opencode-go".to_string()]
        .into_iter()
        .collect();
    // Retired endpoint prefixes point at the replacement provider.
    let err = super::unrouted_selection_error("zen/gpt-5", &known);
    assert!(err.contains("retired 'zen' endpoint prefix"), "{err}");
    assert!(err.contains("'model: opencode/<model-id>'"), "{err}");
    let err = super::unrouted_selection_error("go/kimi-k2", &known);
    assert!(err.contains("'model: opencode-go/<model-id>'"), "{err}");
    // A bare id names the selection, the shape, the builtins, what is
    // already configured, and the config path — never "no model
    // configured".
    let err = super::unrouted_selection_error("gpt-5", &known);
    assert!(err.contains("selection 'gpt-5' names no provider"), "{err}");
    assert!(!err.contains("no model configured"), "{err}");
    assert!(err.contains("anthropic, openai-codex"), "{err}");
    assert!(err.contains("configured: opencode, opencode-go"), "{err}");
}

#[test]
fn opencode_key_resolves_entry_then_own_env_var() {
    // Deposit order: providers.opencode.api_key > OPENCODE_API_KEY
    // (opencode's gateway key).
    let _env = crate::test_env::TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _guard = EnvRestore::take(&[
        "DEX_CONFIG",
        "OPENCODE_API_KEY",
        "DEX_MODEL_APIS",
        "DEX_MODELS",
        "DEX_CONTEXT_WINDOW",
        "XDG_CACHE_HOME",
    ]);
    let dir = std::env::temp_dir().join(format!("dex-okey-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("dex")).unwrap();
    // The catalog is where a generic provider's key env var and landing
    // URL come from (`opencode` is an ordinary `providers:` entry now).
    std::fs::write(
        dir.join("dex/models.dev.json"),
        serde_json::json!({
            "opencode": {
                "api": "https://opencode.ai/zen/v1",
                "env": ["OPENCODE_API_KEY"],
                "models": { "m": { "limit": { "context": 1 } } }
            }
        })
        .to_string(),
    )
    .unwrap();
    std::env::set_var("XDG_CACHE_HOME", &dir);
    std::fs::write(
        dir.join("config.yaml"),
        "model: opencode/m\ncontext_window: 1000\napi: openai-completions\nproviders:\n  opencode: {}\n",
    )
    .unwrap();
    std::env::set_var("DEX_CONFIG", dir.join("config.yaml"));
    std::env::remove_var("OPENCODE_API_KEY");
    // The provider's own env var is the shell path.
    std::env::set_var("OPENCODE_API_KEY", "canonical");
    assert_eq!(
        LlmConfig::from_env(None, None, None, &[]).unwrap().api_key,
        "canonical"
    );
    // The scoped deposit place wins over the env var.
    std::fs::write(
            dir.join("config.yaml"),
            "model: opencode/m\ncontext_window: 1000\nproviders:\n  opencode:\n    api_key: deposited\n    base_url: https://opencode.example/v1\n",
        )
        .unwrap();
    assert_eq!(
        LlmConfig::from_env(None, None, None, &[]).unwrap().api_key,
        "deposited"
    );
    // Missing everywhere: the error points at the canonical names.
    std::fs::write(
        dir.join("config.yaml"),
        "model: opencode/m\ncontext_window: 1000\napi: openai-completions\nproviders:\n  opencode: {}\n",
    )
    .unwrap();
    std::env::remove_var("OPENCODE_API_KEY");
    let err = match LlmConfig::from_env(None, None, None, &[]) {
        Err(e) => e.to_string(),
        Ok(_) => panic!("expected missing-key error"),
    };
    assert!(err.contains("providers.opencode.api_key"), "{err}");
    assert!(err.contains("OPENCODE_API_KEY"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn anthropic_key_resolves_cacheless_via_pinned_env_var() {
    // The catalog cache is empty (`{}`) — ANTHROPIC_API_KEY must still
    // resolve because builtin native providers pin their canonical var
    // (see `pinned_key_env`). Deposit order matches opencode:
    // providers.anthropic.api_key > ANTHROPIC_API_KEY.
    let _env = crate::test_env::TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _guard = EnvRestore::take(&[
        "DEX_CONFIG",
        "ANTHROPIC_API_KEY",
        "DEX_MODEL_APIS",
        "DEX_MODELS",
        "DEX_CONTEXT_WINDOW",
        "XDG_CACHE_HOME",
    ]);
    let dir = std::env::temp_dir().join(format!("dex-akey-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("dex")).unwrap();
    std::fs::write(dir.join("dex/models.dev.json"), "{}").unwrap();
    std::env::set_var("XDG_CACHE_HOME", &dir);
    std::env::set_var("DEX_CONFIG", dir.join("config.yaml"));
    std::fs::write(
        dir.join("config.yaml"),
        "model: anthropic/claude-sonnet-4-5\ncontext_window: 1000\n",
    )
    .unwrap();
    // Cache-less: the pinned var is the shell path; the bare provider
    // pick gets a model of its own family and the native wire.
    std::env::set_var("ANTHROPIC_API_KEY", "sk-ant");
    let cfg = LlmConfig::from_env(None, None, None, &[]).unwrap();
    assert_eq!(cfg.api_key, "sk-ant");
    assert_eq!(cfg.model, "claude-sonnet-4-5");
    assert_eq!(cfg.api, ApiProtocol::Anthropic);
    // The scoped deposit place wins over the env var.
    std::fs::write(
            dir.join("config.yaml"),
            "model: anthropic/claude-sonnet-4-5\ncontext_window: 1000\nproviders:\n  anthropic:\n    api_key: deposited\n",
        )
        .unwrap();
    assert_eq!(
        LlmConfig::from_env(None, None, None, &[]).unwrap().api_key,
        "deposited"
    );
    // The primary knob (`model: anthropic`) picks the family default too.
    std::fs::write(
        dir.join("config.yaml"),
        "model: anthropic/claude-sonnet-4-5\ncontext_window: 1000\n",
    )
    .unwrap();
    std::env::set_var("ANTHROPIC_API_KEY", "sk-ant");
    let cfg = LlmConfig::from_env(None, None, None, &[]).unwrap();
    assert_eq!(cfg.model, "claude-sonnet-4-5");
    assert_eq!(cfg.api, ApiProtocol::Anthropic);
    // Missing everywhere: the error points at the canonical names.
    std::fs::write(
        dir.join("config.yaml"),
        "model: anthropic/claude-sonnet-4-5\ncontext_window: 1000\n",
    )
    .unwrap();
    std::env::remove_var("ANTHROPIC_API_KEY");
    let err = match LlmConfig::from_env(None, None, None, &[]) {
        Err(e) => e.to_string(),
        Ok(_) => panic!("expected missing-key error"),
    };
    assert!(err.contains("providers.anthropic.api_key"), "{err}");
    assert!(err.contains("ANTHROPIC_API_KEY"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn config_file_defaults_apply() {
    // Model, endpoint and protocol come from `--model`/`--base-url`,
    // the file, or builtins — never env vars (those are provider
    // properties, not agent globals).
    let _env = crate::test_env::TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _guard = EnvRestore::take(&[
        "DEX_CONFIG",
        "OPENCODE_API_KEY",
        "DEX_MODEL_APIS",
        "DEX_CONTEXT_WINDOW",
        "XDG_CACHE_HOME",
        "DEX_MODEL",
    ]);
    let dir = std::env::temp_dir().join(format!("dex-filecfg-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.yaml");
    std::fs::write(
            &path,
            "base_url: https://file.example/v1\nmodel: opencode/file-model\ncontext_window: 1000\napi: openai-completions\ncustom_key: keep-me\nproviders:\n  opencode:\n    api_key: env-key\n    base_url: https://opencode.example/v1\n",
        )
        .unwrap();
    std::env::set_var("DEX_CONFIG", &path);
    std::env::remove_var("OPENCODE_API_KEY");
    // File model + file base_url + file api apply; a `--model`
    // override beats the file.
    let cfg = LlmConfig::from_env(None, None, None, &[]).unwrap();
    assert_eq!(cfg.model, "file-model");
    assert_eq!(cfg.base_url, "https://file.example/v1");
    assert_eq!(cfg.api, ApiProtocol::ChatCompletions);
    let cfg =
        LlmConfig::from_env(None, Some("opencode/flag-model".to_string()), None, &[]).unwrap();
    assert_eq!(cfg.model, "flag-model");
    // Write-back: one canonical `model: <endpoint>/<id>` key; the
    // redundant `active_provider:`/`base_url:` keys are dropped; unknown
    // keys survive.
    let mut cfg = LlmConfig::from_env(None, None, None, &[]).unwrap();
    cfg.apply_model("opencode/new-model", true).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("model: opencode/new-model"), "{text}");
    assert!(
        !text
            .lines()
            .any(|l| { l.starts_with("active_provider:") || l.starts_with("base_url:") }),
        "redundant keys removed: {text}"
    );
    assert!(text.contains("api: openai-completions"), "{text}");
    assert!(text.contains("custom_key: keep-me"));
    // Learned protocol: remembered to the cache, picked up on the next
    // config build (nothing explicit pins this model's protocol).
    let _guard = EnvRestore::take(&["XDG_CACHE_HOME"]);
    let cache = dir.join("cache");
    std::env::set_var("XDG_CACHE_HOME", &cache);
    crate::llm::learned::remember(
        "https://file.example/v1",
        "file-model",
        ApiProtocol::ChatCompletions,
    );
    assert!(cache.join("dex/learned-apis.json").exists());
    let cfg = LlmConfig::from_env(None, None, None, &[]).unwrap();
    assert_eq!(cfg.api, ApiProtocol::ChatCompletions);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Synthetic catalog where each model lives on exactly one endpoint,
/// so bare picks must move `base_url` without any prefix knowledge.
fn write_routing_catalog(dir: &std::path::Path) {
    std::fs::create_dir_all(dir.join("dex")).unwrap();
    std::fs::write(
        dir.join("dex/models.dev.json"),
        serde_json::json!({
            "opencode": {
                "api": "https://opencode.ai/zen/v1",
                "models": { "m-zen-only": { "limit": { "context": 1 } } }
            },
            "opencode-go": {
                "api": "https://go.example/v1",
                "models": { "m-go-only": { "limit": { "context": 1 } } }
            },
        })
        .to_string(),
    )
    .unwrap();
}

#[test]
fn bare_model_auto_routes_to_serving_endpoint() {
    let _env = crate::test_env::TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _guard = EnvRestore::take(&["XDG_CACHE_HOME", "DEX_MODEL_APIS"]);
    let dir = std::env::temp_dir().join(format!("dex-route-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write_routing_catalog(&dir);
    std::env::set_var("XDG_CACHE_HOME", &dir);
    std::env::remove_var("DEX_MODEL_APIS");
    let mut cfg = test_cfg();
    cfg.endpoints
        .insert("go".to_string(), "https://go.example/v1".to_string());
    // Bare pick living on another endpoint moves base_url by itself.
    assert_eq!(
        cfg.apply_model("m-go-only", false).unwrap().as_deref(),
        Some("go")
    );
    assert_eq!(cfg.base_url, "https://go.example/v1");
    assert_eq!(cfg.model, "m-go-only");
    // Back to a zen-only model.
    assert_eq!(
        cfg.apply_model("m-zen-only", false).unwrap().as_deref(),
        Some("opencode")
    );
    assert_eq!(cfg.base_url, "https://opencode.ai/zen/v1");
    // A model the current endpoint serves never moves — dual-served ids
    // must not ping-pong between endpoints.
    assert_eq!(cfg.apply_model("m-zen-only", false).unwrap(), None);
    assert_eq!(cfg.base_url, "https://opencode.ai/zen/v1");
    // Unknown models and custom URLs stay put.
    assert_eq!(cfg.apply_model("m-unknown", false).unwrap(), None);
    assert_eq!(cfg.base_url, "https://opencode.ai/zen/v1");
    cfg.base_url = "https://custom.example/v1".to_string();
    assert_eq!(cfg.apply_model("m-go-only", false).unwrap(), None);
    assert_eq!(cfg.base_url, "https://custom.example/v1");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn explicit_base_url_wins_over_catalog_routing() {
    // An explicit pin holds even when it names a known endpoint: a
    // go-only model stays on the pinned URL instead of being silently
    // rerouted to the serving endpoint. Pins are `--base-url` and file
    // `base_url:` only — endpoint properties, not env globals.
    let _env = crate::test_env::TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _guard = EnvRestore::take(&[
        "DEX_CONFIG",
        "OPENCODE_API_KEY",
        "DEX_MODEL_APIS",
        "DEX_MODELS",
        "DEX_CONTEXT_WINDOW",
        "XDG_CACHE_HOME",
    ]);
    let dir = std::env::temp_dir().join(format!("dex-pin-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write_routing_catalog(&dir);
    std::fs::write(
        dir.join("config.yaml"),
        "model: opencode/m-zen\nproviders:\n  opencode:\n    api_key: test-key\n",
    )
    .unwrap();
    std::env::set_var("XDG_CACHE_HOME", &dir);
    std::env::set_var("DEX_CONFIG", dir.join("config.yaml"));
    std::env::set_var("OPENCODE_API_KEY", "test-key");
    for key in ["DEX_MODEL_APIS", "DEX_MODELS", "DEX_CONTEXT_WINDOW"] {
        std::env::remove_var(key);
    }
    // `--base-url` pin with a `--model` override.
    let cfg = LlmConfig::from_env(
        Some("https://opencode.ai/zen/v1".to_string()),
        Some("opencode/m-go-only".to_string()),
        None,
        &[],
    )
    .unwrap();
    assert_eq!(cfg.model, "m-go-only");
    assert_eq!(cfg.base_url, "https://opencode.ai/zen/v1");
    // File `base_url:` pin with a file model.
    std::fs::write(
        dir.join("config.yaml"),
        "base_url: https://opencode.ai/zen/v1\nmodel: opencode/m-go-only\nproviders:\n  opencode:\n    api_key: test-key\n",
    )
    .unwrap();
    let cfg = LlmConfig::from_env(None, None, None, &[]).unwrap();
    assert_eq!(cfg.model, "m-go-only");
    assert_eq!(cfg.base_url, "https://opencode.ai/zen/v1");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Synthetic catalog with a third-party OpenAI-compatible provider
/// ("zai") so the generic provider layer can be exercised end to end.
fn write_generic_catalog(dir: &std::path::Path) {
    std::fs::create_dir_all(dir.join("dex")).unwrap();
    std::fs::write(
        dir.join("dex/models.dev.json"),
        serde_json::json!({
            "zai": {
                "api": "https://api.zai.example/v4",
                "env": { "ZAI_TEST_KEY": "z.ai api key" },
                "models": {
                    "glm-x": {
                        "limit": { "context": 1 },
                        "reasoning_options": [
                            { "type": "effort", "values": ["low", "high"] }
                        ]
                    }
                }
            }
        })
        .to_string(),
    )
    .unwrap();
}

fn generic_config(dir: &std::path::Path, providers_yaml: &str) {
    std::fs::write(
        dir.join("config.yaml"),
        format!("model: zai/glm-x\n{providers_yaml}"),
    )
    .unwrap();
}

#[test]
fn generic_provider_resolves_endpoint_key_and_routing() {
    let _env = crate::test_env::TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _guard = EnvRestore::take(&[
        "DEX_CONFIG",
        "OPENCODE_API_KEY",
        "DEX_MODEL_APIS",
        "DEX_MODELS",
        "DEX_CONTEXT_WINDOW",
        "XDG_CACHE_HOME",
        "ZAI_TEST_KEY",
    ]);
    let dir = std::env::temp_dir().join(format!("dex-generic-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write_generic_catalog(&dir);
    generic_config(&dir, "providers:\n  zai:\n    api_key: zsk-deposit\n");
    std::env::set_var("XDG_CACHE_HOME", &dir);
    std::env::set_var("DEX_CONFIG", dir.join("config.yaml"));
    for key in [
        "OPENCODE_API_KEY",
        "DEX_MODEL_APIS",
        "DEX_MODELS",
        "DEX_CONTEXT_WINDOW",
        "ZAI_TEST_KEY",
    ] {
        std::env::remove_var(key);
    }
    // Endpoint + key from the deposit place; catalog supplies the URL.
    // (`--model` stands in for the removed model env override.)
    let cfg = LlmConfig::from_env(None, Some("zai/glm-x".to_string()), None, &[]).unwrap();
    assert_eq!(cfg.provider.name(), "zai");
    assert_eq!(cfg.base_url, "https://api.zai.example/v4");
    assert_eq!(cfg.api_key, "zsk-deposit");
    assert_eq!(
        cfg.endpoints.get("zai").map(String::as_str),
        Some("https://api.zai.example/v4")
    );
    // `/model zai/glm-x` is a no-op (already there); unknown prefixes
    // must still stay plain model ids.
    let mut cfg = cfg;
    assert!(cfg.apply_model("unknown/m", false).unwrap().is_none());
    assert_eq!(cfg.model, "unknown/m");
    // Key falls back to the provider's own conventional env var.
    std::fs::write(
        dir.join("config.yaml"),
        "model: zai/glm-x\nproviders:\n  zai: {}\n",
    )
    .unwrap();
    std::env::set_var("ZAI_TEST_KEY", "zsk-from-env");
    let cfg = LlmConfig::from_env(None, None, None, &[]).unwrap();
    assert_eq!(cfg.api_key, "zsk-from-env");
    // No key anywhere: the error names both deposit places.
    std::env::remove_var("ZAI_TEST_KEY");
    let err = match LlmConfig::from_env(None, None, None, &[]) {
        Err(e) => e.to_string(),
        Ok(_) => panic!("expected missing-key error"),
    };
    assert!(err.contains("providers.zai.api_key"), "{err}");
    assert!(err.contains("ZAI_TEST_KEY"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn generic_provider_switch_and_completion_ids() {
    let _env = crate::test_env::TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _guard = EnvRestore::take(&[
        "DEX_CONFIG",
        "OPENCODE_API_KEY",
        "DEX_MODEL_APIS",
        "DEX_MODELS",
        "DEX_CONTEXT_WINDOW",
        "XDG_CACHE_HOME",
    ]);
    let dir = std::env::temp_dir().join(format!("dex-gswitch-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write_generic_catalog(&dir);
    generic_config(&dir, "providers:\n  zai:\n    api_key: zsk-deposit\n");
    std::env::set_var("XDG_CACHE_HOME", &dir);
    std::env::set_var("DEX_CONFIG", dir.join("config.yaml"));
    std::env::set_var("OPENCODE_API_KEY", "test-key");
    // Completion list offers the configured provider's qualified ids.
    let ids = load_dex_models_cache().unwrap();
    assert!(ids.contains(&"glm-x".to_string()));
    assert!(ids.contains(&"zai/glm-x".to_string()));
    // Provider-qualified pick switches to the generic provider.
    let mut cfg = LlmConfig::from_env(None, None, None, &[]).unwrap();
    assert!(cfg.apply_model("zai/glm-x", false).unwrap().is_none()); // already on zai
    assert_eq!(cfg.model, "glm-x");
    // From opencode, the same pick switches provider AND endpoint.
    let mut cfg = test_cfg();
    cfg.provider_entries = load_provider_entries(&load_config_file());
    assert_eq!(cfg.apply_model("zai/glm-x", false).unwrap(), None);
    assert_eq!(cfg.provider.name(), "zai");
    assert_eq!(cfg.base_url, "https://api.zai.example/v4");
    // Thinking options come from the catalog for the selected model.
    assert_eq!(
        reasoning_options_for("glm-x"),
        Some(vec!["low".to_string(), "high".to_string()])
    );
    assert_eq!(reasoning_options_for("m-unknown"), None);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn models_cache_offers_provider_qualified_ids() {
    let _env = crate::test_env::TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _guard = EnvRestore::take(&["XDG_CACHE_HOME", "DEX_CONFIG"]);
    let dir = std::env::temp_dir().join(format!("dex-mlist-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write_routing_catalog(&dir);
    // The qualified prefix is each configured provider's own name.
    std::fs::write(
        dir.join("config.yaml"),
        "providers:\n  opencode: {}\n  opencode-go: {}\n",
    )
    .unwrap();
    std::env::set_var("XDG_CACHE_HOME", &dir);
    std::env::set_var("DEX_CONFIG", dir.join("config.yaml"));
    let ids = load_dex_models_cache().unwrap();
    // Bare ids for every catalog model, plus provider-qualified variants
    // so a pick can name its provider explicitly.
    assert!(ids.contains(&"m-zen-only".to_string()));
    assert!(ids.contains(&"opencode/m-zen-only".to_string()));
    assert!(ids.contains(&"m-go-only".to_string()));
    assert!(ids.contains(&"opencode-go/m-go-only".to_string()));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn prefixed_selection_restores_protocol_on_restart() {
    // A provider-prefixed `model:` in the config file must honor a bare-id
    // `DEX_MODEL_APIS` entry: the full selection key is tried first,
    // then the stripped id.
    let _env = crate::test_env::TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _guard = EnvRestore::take(&[
        "DEX_CONFIG",
        "OPENCODE_API_KEY",
        "DEX_MODEL_APIS",
        "DEX_MODELS",
        "DEX_CONTEXT_WINDOW",
        "XDG_CACHE_HOME",
    ]);
    let dir = std::env::temp_dir().join(format!("dex-restart-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("dex")).unwrap();
    std::fs::write(
        dir.join("config.yaml"),
        "model: opencode-go/m-z9\ncontext_window: 1000\nproviders:\n  opencode-go:\n    base_url: https://opencode.ai/zen/go/v1\n    api_key: test-key\n",
    )
    .unwrap();
    // Empty catalog dir: no routing interference, unknown model stays.
    std::fs::create_dir_all(dir.join("cache/dex")).unwrap();
    std::fs::write(dir.join("cache/dex/models.dev.json"), "{}").unwrap();
    std::env::set_var("DEX_CONFIG", dir.join("config.yaml"));
    std::env::set_var("XDG_CACHE_HOME", dir.join("cache"));
    std::env::set_var("OPENCODE_API_KEY", "test-key");
    std::env::set_var("DEX_MODEL_APIS", "m-z9=openai-completions");
    let cfg = LlmConfig::from_env(None, None, None, &[]).unwrap();
    assert_eq!(cfg.model, "m-z9");
    assert_eq!(cfg.base_url, "https://opencode.ai/zen/go/v1");
    assert_eq!(cfg.api, ApiProtocol::ChatCompletions);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn persisted_selection_reloads_stripped() {
    // Persisted `model:` must reload onto the same endpoint without the
    // raw prefix: `from_env` treats a file `base_url:` as an explicit
    // pin and skips routing, so a stored `go/<id>` would otherwise be
    // sent to the API verbatim after restart.
    let _env = crate::test_env::TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _guard = EnvRestore::take(&[
        "DEX_CONFIG",
        "OPENCODE_API_KEY",
        "DEX_MODEL_APIS",
        "DEX_MODELS",
        "DEX_CONTEXT_WINDOW",
        "XDG_CACHE_HOME",
    ]);
    let dir = std::env::temp_dir().join(format!("dex-persist-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    // Catalog whose serving URLs are exactly the builtin endpoints.
    std::fs::create_dir_all(dir.join("dex")).unwrap();
    std::fs::write(
        dir.join("dex/models.dev.json"),
        serde_json::json!({
            "opencode": {
                "api": "https://opencode.ai/zen/v1",
                "models": { "m-zen": { "limit": { "context": 1 } } }
            },
            "opencode-go": {
                "api": "https://opencode.ai/zen/go/v1",
                "models": { "m-go": { "limit": { "context": 1 } } }
            },
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        dir.join("config.yaml"),
        "model: opencode/m-zen\nproviders:\n  opencode:\n    api_key: test-key\n  opencode-go:\n    api_key: test-key\n",
    )
    .unwrap();
    std::env::set_var("XDG_CACHE_HOME", &dir);
    std::env::set_var("DEX_CONFIG", dir.join("config.yaml"));
    std::env::set_var("OPENCODE_API_KEY", "test-key");
    for key in ["DEX_MODEL_APIS", "DEX_MODELS", "DEX_CONTEXT_WINDOW"] {
        std::env::remove_var(key);
    }
    let mut cfg = LlmConfig::from_env(None, None, None, &[]).unwrap();
    assert_eq!(cfg.base_url, "https://opencode.ai/zen/v1");
    cfg.apply_model("opencode-go/m-go", true).unwrap();
    assert_eq!(cfg.model, "m-go");
    // Restart with the persisted file (now carrying a `base_url:`, i.e.
    // the explicit-pin path): the id stays stripped, the endpoint holds.
    let cfg = LlmConfig::from_env(None, None, None, &[]).unwrap();
    assert_eq!(cfg.model, "m-go");
    assert_eq!(cfg.base_url, "https://opencode.ai/zen/go/v1");
    // And back to a zen model.
    let mut cfg = cfg;
    cfg.apply_model("opencode/m-zen", true).unwrap();
    let cfg = LlmConfig::from_env(None, None, None, &[]).unwrap();
    assert_eq!(cfg.model, "m-zen");
    assert_eq!(cfg.base_url, "https://opencode.ai/zen/v1");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn pinned_base_url_still_resolves_per_model_protocol() {
    // A file `base_url:` pins routing only: the per-model table and the
    // learned fallback must still decide the wire protocol. Otherwise a
    // persisted `/model go/<id>` (which always writes `base_url:`)
    // retries `/responses` on every restart and a `DEX_MODEL_APIS` pin
    // is ignored for the request yet blocks the fallback — the exact
    // error loop in the glm-5.3-flash report.
    let _env = crate::test_env::TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _guard = EnvRestore::take(&[
        "DEX_CONFIG",
        "OPENCODE_API_KEY",
        "DEX_MODEL_APIS",
        "DEX_MODELS",
        "DEX_CONTEXT_WINDOW",
        "XDG_CACHE_HOME",
    ]);
    let dir = std::env::temp_dir().join(format!("dex-pin-proto-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("cache/dex")).unwrap();
    std::fs::write(dir.join("cache/dex/models.dev.json"), "{}").unwrap();
    // Persisted state after `/model go/m-go`: stripped id + pinned URL.
    std::fs::write(
        dir.join("config.yaml"),
        "base_url: https://opencode.ai/zen/go/v1\nmodel: opencode/m-go\ncontext_window: 1000\nproviders:\n  opencode:\n    api_key: test-key\n",
    )
    .unwrap();
    std::env::set_var("DEX_CONFIG", dir.join("config.yaml"));
    std::env::set_var("XDG_CACHE_HOME", dir.join("cache"));
    std::env::set_var("OPENCODE_API_KEY", "test-key");
    for key in ["DEX_MODELS", "DEX_CONTEXT_WINDOW"] {
        std::env::remove_var(key);
    }
    // Per-model table entry wins over the responses default.
    std::env::set_var("DEX_MODEL_APIS", "m-go=openai-completions");
    let cfg = LlmConfig::from_env(None, None, None, &[]).unwrap();
    assert_eq!(cfg.model, "m-go");
    assert_eq!(cfg.base_url, "https://opencode.ai/zen/go/v1");
    assert_eq!(cfg.api, ApiProtocol::ChatCompletions);
    // Without the table, the learned fallback decides instead.
    std::env::remove_var("DEX_MODEL_APIS");
    crate::llm::learned::remember(
        "https://opencode.ai/zen/go/v1",
        "m-go",
        ApiProtocol::ChatCompletions,
    );
    let cfg = LlmConfig::from_env(None, None, None, &[]).unwrap();
    assert_eq!(cfg.api, ApiProtocol::ChatCompletions);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn apply_model_refuses_provider_switch_without_credentials_or_endpoint() {
    // A provider-qualified pick is atomic: without credentials or a
    // known endpoint the switch is refused and the old key/URL stay
    // put — never one provider's key against another's endpoint.
    let _env = crate::test_env::TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _guard = EnvRestore::take(&[
        "OPENCODE_API_KEY",
        "CODEX_ACCESS_TOKEN",
        "CODEX_ACCOUNT_ID",
        "CODEX_HOME",
        "XDG_CACHE_HOME",
    ]);
    for key in ["OPENCODE_API_KEY", "CODEX_ACCESS_TOKEN", "CODEX_ACCOUNT_ID"] {
        std::env::remove_var(key);
    }
    // Hermetic XDG/CODEX_HOME: no credential file, no catalog.
    let dir = std::env::temp_dir().join(format!("dex-refuse-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::env::set_var("XDG_CACHE_HOME", &dir);
    std::env::set_var("CODEX_HOME", dir.join("codex-home"));
    let mut cfg = test_cfg();
    assert!(cfg.apply_model("openai-codex/gpt-x", false).is_err());
    assert_eq!(cfg.provider, Provider::Generic("opencode".to_string()));
    assert_eq!(cfg.api_key, "k");
    assert_eq!(cfg.base_url, "https://opencode.ai/zen/v1");
    assert_eq!(cfg.model, "m-r");
    // Configured generic provider with neither key nor endpoint.
    cfg.provider_entries
        .insert("zai".to_string(), ProviderEntry::default());
    assert!(cfg.apply_model("zai/glm-x", false).is_err());
    assert_eq!(cfg.provider, Provider::Generic("opencode".to_string()));
    assert_eq!(cfg.api_key, "k");
    assert_eq!(cfg.base_url, "https://opencode.ai/zen/v1");
    assert_eq!(cfg.model, "m-r");
    // Key deposited but no endpoint anywhere: same refusal.
    cfg.provider_entries.insert(
        "zai-keyed".to_string(),
        ProviderEntry {
            api_key: Some("zsk-x".to_string()),
            ..Default::default()
        },
    );
    assert!(cfg.apply_model("zai-keyed/glm-x", false).is_err());
    assert_eq!(cfg.provider, Provider::Generic("opencode".to_string()));
    assert_eq!(cfg.api_key, "k");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn catalog_env_vars_tries_every_documented_name() {
    // A provider documenting several key env vars accepts any of them —
    // not just the first key in the catalog `env` map.
    let _env = crate::test_env::TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _guard = EnvRestore::take(&[
        "DEX_CONFIG",
        "OPENCODE_API_KEY",
        "DEX_MODEL_APIS",
        "DEX_MODELS",
        "DEX_CONTEXT_WINDOW",
        "XDG_CACHE_HOME",
        "ZAI_A_KEY",
        "ZAI_B_KEY",
    ]);
    let dir = std::env::temp_dir().join(format!("dex-multienv-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("dex")).unwrap();
    std::fs::write(
        dir.join("dex/models.dev.json"),
        serde_json::json!({
            "zai": {
                "api": "https://api.zai.example/v4",
                "env": { "ZAI_B_KEY": "second", "ZAI_A_KEY": "first" },
                "models": { "glm-x": { "limit": { "context": 1 } } }
            }
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        dir.join("config.yaml"),
        "model: zai/glm-x\nproviders:\n  zai: {}\n",
    )
    .unwrap();
    std::env::set_var("XDG_CACHE_HOME", &dir);
    std::env::set_var("DEX_CONFIG", dir.join("config.yaml"));
    for key in [
        "OPENCODE_API_KEY",
        "DEX_MODEL_APIS",
        "DEX_MODELS",
        "DEX_CONTEXT_WINDOW",
        "ZAI_A_KEY",
        "ZAI_B_KEY",
    ] {
        std::env::remove_var(key);
    }
    // Only the non-first documented name is set.
    std::env::set_var("ZAI_A_KEY", "a-key");
    assert_eq!(
        LlmConfig::from_env(None, None, None, &[]).unwrap().api_key,
        "a-key"
    );
    // Neither set: the error names every documented name.
    std::env::remove_var("ZAI_A_KEY");
    let err = match LlmConfig::from_env(None, None, None, &[]) {
        Err(e) => e.to_string(),
        Ok(_) => panic!("expected missing-key error"),
    };
    assert!(err.contains("ZAI_A_KEY"), "{err}");
    assert!(err.contains("ZAI_B_KEY"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn legacy_provider_keys_ignored_and_dropped() {
    // Pre-rename files used `provider:`/`active_provider:` as the selection
    // pointer; neither is read anymore (selection lives in `model:`), the
    // file still loads, and write-back drops them — with top-level
    // `base_url:` — for the one-knob schema.
    let _env = crate::test_env::TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _guard = EnvRestore::take(&[
        "DEX_CONFIG",
        "OPENCODE_API_KEY",
        "DEX_MODEL_APIS",
        "DEX_MODELS",
        "DEX_CONTEXT_WINDOW",
        "XDG_CACHE_HOME",
    ]);
    let dir = std::env::temp_dir().join(format!("dex-legacyprov-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("dex")).unwrap();
    std::fs::write(dir.join("dex/models.dev.json"), "{}").unwrap();
    let path = dir.join("config.yaml");
    std::fs::write(
            &path,
            "model: opencode/m\nprovider: anthropic\nactive_provider: anthropic\ncontext_window: 1000\nproviders:\n  opencode:\n    api_key: deposited\n    base_url: https://opencode.example/v1\n",
        )
        .unwrap();
    std::env::set_var("XDG_CACHE_HOME", &dir);
    std::env::set_var("DEX_CONFIG", &path);
    std::env::remove_var("OPENCODE_API_KEY");
    for key in ["DEX_MODEL_APIS", "DEX_MODELS", "DEX_CONTEXT_WINDOW"] {
        std::env::remove_var(key);
    }
    let cfg = LlmConfig::from_env(None, None, None, &[]).unwrap();
    // `model:` selects; the legacy pointer keys are ignored, not honored.
    assert_eq!(cfg.provider, Provider::Generic("opencode".to_string()));
    assert_eq!(cfg.model, "m");
    assert_eq!(cfg.api_key, "deposited");
    // Write-back stores one canonical key and drops the legacy ones.
    let endpoints: std::collections::BTreeMap<String, String> = [
        ("zen".to_string(), "https://opencode.ai/zen/v1".to_string()),
        (
            "go".to_string(),
            "https://opencode.ai/zen/go/v1".to_string(),
        ),
    ]
    .into_iter()
    .collect();
    persist_selection(
        "m",
        &Provider::Generic("opencode".to_string()),
        "https://opencode.ai/zen/v1",
        &endpoints,
    );
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("model: zen/m"), "{text}");
    assert!(
        !text.lines().any(|l| {
            l.starts_with("provider:")
                || l.starts_with("active_provider:")
                || l.starts_with("base_url:")
        }),
        "legacy keys removed: {text}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn api_pin_bakes_into_config() {
    // The global protocol pin is computed once in `from_env` (file or
    // provider entry) so hot paths never re-read the file.
    let _env = crate::test_env::TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _guard = EnvRestore::take(&[
        "DEX_CONFIG",
        "OPENCODE_API_KEY",
        "DEX_MODEL_APIS",
        "DEX_MODELS",
        "DEX_CONTEXT_WINDOW",
        "XDG_CACHE_HOME",
    ]);
    let dir = std::env::temp_dir().join(format!("dex-pinbake-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::env::set_var("XDG_CACHE_HOME", &dir);
    std::env::set_var("DEX_CONFIG", dir.join("config.yaml"));
    std::env::set_var("OPENCODE_API_KEY", "test-key");
    for key in ["DEX_MODEL_APIS", "DEX_MODELS", "DEX_CONTEXT_WINDOW"] {
        std::env::remove_var(key);
    }
    std::fs::write(
        dir.join("config.yaml"),
        "model: opencode/m\ncontext_window: 1000\nproviders:\n  opencode:\n    base_url: https://opencode.example/v1\n    api_key: test-key\n",
    )
    .unwrap();
    assert!(
        !LlmConfig::from_env(None, None, None, &[])
            .unwrap()
            .api_pinned
    );
    std::fs::write(
            dir.join("config.yaml"),
            "model: opencode/m\ncontext_window: 1000\nproviders:\n  opencode:\n    api: openai-completions\n    api_key: test-key\n    base_url: https://opencode.example/v1\n",
        )
        .unwrap();
    assert!(
        LlmConfig::from_env(None, None, None, &[])
            .unwrap()
            .api_pinned
    );
    // A top-level `api:` pin still works too (deprecated but honored).
    std::fs::write(
            dir.join("config.yaml"),
            "model: opencode/m\ncontext_window: 1000\napi: openai-completions\nproviders:\n  opencode:\n    api_key: test-key\n    base_url: https://opencode.example/v1\n",
        )
        .unwrap();
    assert!(
        LlmConfig::from_env(None, None, None, &[])
            .unwrap()
            .api_pinned
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn resolved_provider_bundles_entry_overrides() {
    // One derivation feeds every consumer: entry `base_url:` beats the
    // catalog URL, entry `api:` pins (and bakes `api_pinned`), entry
    // `headers:` land in `provider_headers` — and all three refresh on
    // a provider switch.
    let _env = crate::test_env::TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _guard = EnvRestore::take(&[
        "DEX_CONFIG",
        "OPENCODE_API_KEY",
        "DEX_MODEL_APIS",
        "DEX_MODELS",
        "DEX_CONTEXT_WINDOW",
        "XDG_CACHE_HOME",
    ]);
    let dir = std::env::temp_dir().join(format!("dex-resolved-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write_generic_catalog(&dir);
    std::fs::write(
            dir.join("config.yaml"),
            "model: zai/glm-x\nproviders:\n  zai:\n    api_key: zsk-deposit\n    base_url: https://custom.zai.example/v1\n    api: openai-completions\n    headers:\n      X-Prov: prov\n  opencode:\n    api_key: ok-key\n    base_url: https://opencode.ai/zen/v1\n",
        )
        .unwrap();
    std::env::set_var("XDG_CACHE_HOME", &dir);
    std::env::set_var("DEX_CONFIG", dir.join("config.yaml"));
    std::env::set_var("OPENCODE_API_KEY", "test-key");
    for key in ["DEX_MODEL_APIS", "DEX_MODELS", "DEX_CONTEXT_WINDOW"] {
        std::env::remove_var(key);
    }
    let mut cfg = LlmConfig::from_env(None, None, None, &[]).unwrap();
    assert_eq!(cfg.base_url, "https://custom.zai.example/v1");
    assert_eq!(cfg.api, ApiProtocol::ChatCompletions);
    assert!(cfg.api_pinned);
    assert_eq!(
        cfg.provider_headers.get("X-Prov").map(String::as_str),
        Some("prov")
    );
    // Switching providers refreshes the whole bundle, not just the URL:
    // endpoint, protocol base + pin, and scoped headers.
    cfg.apply_model("opencode/glm-x", false).unwrap();
    assert_eq!(cfg.provider, Provider::Generic("opencode".to_string()));
    assert_eq!(cfg.base_url, "https://opencode.ai/zen/v1");
    assert_eq!(cfg.api, ApiProtocol::Responses);
    assert!(!cfg.api_pinned);
    assert!(cfg.provider_headers.is_empty());
    // And back via `switch_provider`: pin, endpoint and headers return.
    cfg.switch_provider(&Provider::Generic("zai".into()), false)
        .unwrap();
    assert_eq!(cfg.base_url, "https://custom.zai.example/v1");
    assert_eq!(cfg.api, ApiProtocol::ChatCompletions);
    assert!(cfg.api_pinned);
    assert_eq!(
        cfg.provider_headers.get("X-Prov").map(String::as_str),
        Some("prov")
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ctx_map_covers_both_catalog_shapes_lowercased() {
    let catalog = serde_json::json!({
        "opencode": { "models": {
            "GPT-5": { "limit": { "context": 128000 } },
            "zero": { "limit": { "context": 0 } },
            "nodoc": {},
        } },
        "models": {
            "Custom-X": { "limit": { "context": 200000 } },
        },
    });
    let map = build_ctx_map(&catalog);
    assert_eq!(map.get("gpt-5"), Some(&128000));
    assert_eq!(map.get("custom-x"), Some(&200000));
    assert!(!map.contains_key("zero"));
    assert!(!map.contains_key("nodoc"));
}

#[test]
fn thinking_effort_resolves_stored_then_env() {
    // Precedence: stored `/thinking` choice > `DEX_THINKING_EFFORT` >
    // unset; clearing falls back down the chain.
    let _env = crate::test_env::TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _guard = EnvRestore::take(&[
        "DEX_THINKING_EFFORT",
        "DEX_CONTEXT_WINDOW",
        "XDG_CACHE_HOME",
        "XDG_CONFIG_HOME",
        "DEX_CONFIG",
    ]);
    let dir = std::env::temp_dir().join(format!("dex-think-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::env::set_var("XDG_CACHE_HOME", &dir);
    // refresh_thinking_effort also falls back to the user config file;
    // a machine with `thinking_effort:` set there would fail the first
    // assertion, so point the config lookup at the empty temp dir too.
    std::env::set_var("XDG_CONFIG_HOME", &dir);
    std::env::remove_var("DEX_CONFIG");
    std::env::remove_var("DEX_THINKING_EFFORT");
    let mut cfg = test_cfg();
    cfg.refresh_thinking_effort();
    assert_eq!(cfg.thinking_effort, None);
    std::env::set_var("DEX_THINKING_EFFORT", "medium");
    cfg.refresh_thinking_effort();
    assert_eq!(cfg.thinking_effort.as_deref(), Some("medium"));
    remember_thinking_effort(&cfg.base_url, &cfg.model, Some("low"));
    cfg.refresh_thinking_effort();
    assert_eq!(cfg.thinking_effort.as_deref(), Some("low"));
    assert_eq!(
        stored_thinking_effort(&cfg.base_url, &cfg.model).as_deref(),
        Some("low")
    );
    remember_thinking_effort(&cfg.base_url, &cfg.model, None);
    cfg.refresh_thinking_effort();
    assert_eq!(cfg.thinking_effort.as_deref(), Some("medium"));
    std::env::remove_var("DEX_THINKING_EFFORT");
    cfg.refresh_thinking_effort();
    assert_eq!(cfg.thinking_effort, None);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn thinking_pick_validates_against_advertised_options() {
    let _env = crate::test_env::TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _guard = EnvRestore::take(&["XDG_CACHE_HOME"]);
    let dir = std::env::temp_dir().join(format!("dex-thinkval-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write_generic_catalog(&dir);
    std::env::set_var("XDG_CACHE_HOME", &dir);
    // Case-insensitive pick, catalog casing back.
    assert_eq!(
        validate_thinking_effort("glm-x", "HIGH"),
        Ok("high".to_string())
    );
    // Rejected pick returns the valid options.
    assert_eq!(
        validate_thinking_effort("glm-x", "ultra"),
        Err(vec!["low".to_string(), "high".to_string()])
    );
    // Unknown model: accepted raw, a stale catalog never blocks.
    assert_eq!(
        validate_thinking_effort("m-unknown", "whatever"),
        Ok("whatever".to_string())
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn thinking_mismatch_warning_is_data_not_stderr() {
    // The mismatch hint must be returnable data (transcript line), never
    // an `eprintln!` from config code: the daemon shares the TUI's
    // terminal, where stderr corrupts the alternate screen / OSC query.
    let _env = crate::test_env::TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _guard = EnvRestore::take(&["XDG_CACHE_HOME"]);
    let dir = std::env::temp_dir().join(format!("dex-thinkwarn-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write_generic_catalog(&dir);
    std::env::set_var("XDG_CACHE_HOME", &dir);
    let mut cfg = test_cfg();
    cfg.model = "glm-x".into();
    cfg.thinking_effort = None;
    assert!(cfg.thinking_mismatch_warning().is_none());
    cfg.thinking_effort = Some("high".into());
    assert!(cfg.thinking_mismatch_warning().is_none());
    cfg.thinking_effort = Some("ultra".into());
    let warning = cfg.thinking_mismatch_warning().expect("mismatch warns");
    assert!(
        warning.contains("ultra") && warning.contains("low, high"),
        "{warning}"
    );
    cfg.model = "m-unknown".into();
    assert!(cfg.thinking_mismatch_warning().is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn apply_model_swaps_effort_with_the_model() {
    // A model switch moves the thinking knob to the new model's stored
    // choice instead of leaking the old one.
    let _env = crate::test_env::TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _guard = EnvRestore::take(&[
        "DEX_THINKING_EFFORT",
        "DEX_CONTEXT_WINDOW",
        "XDG_CACHE_HOME",
    ]);
    let dir = std::env::temp_dir().join(format!("dex-thinkswap-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::env::set_var("XDG_CACHE_HOME", &dir);
    std::env::remove_var("DEX_THINKING_EFFORT");
    let mut cfg = test_cfg();
    remember_thinking_effort(&cfg.base_url, "m-r", Some("low"));
    remember_thinking_effort(&cfg.base_url, "m-new", Some("high"));
    cfg.refresh_thinking_effort();
    assert_eq!(cfg.thinking_effort.as_deref(), Some("low"));
    cfg.apply_model("m-new", false).unwrap();
    assert_eq!(cfg.thinking_effort.as_deref(), Some("high"));
    std::env::set_var("DEX_THINKING_EFFORT", "medium");
    cfg.apply_model("m-bare", false).unwrap();
    assert_eq!(cfg.thinking_effort.as_deref(), Some("medium"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn thinking_effort_reads_file_key_under_env() {
    // File `thinking_effort:` is the default under a stored choice and
    // the env var: stored > env > file > unset.
    let _env = crate::test_env::TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _guard = EnvRestore::take(&[
        "DEX_CONFIG",
        "OPENCODE_API_KEY",
        "DEX_MODEL_APIS",
        "DEX_MODELS",
        "DEX_CONTEXT_WINDOW",
        "DEX_THINKING_EFFORT",
        "XDG_CACHE_HOME",
    ]);
    let dir = std::env::temp_dir().join(format!("dex-thinkfile-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("dex")).unwrap();
    std::fs::write(dir.join("dex/models.dev.json"), "{}").unwrap();
    std::fs::write(
        dir.join("config.yaml"),
        "model: opencode/m\ncontext_window: 1000\nthinking_effort: low\nproviders:\n  opencode:\n    api_key: test-key\n    base_url: https://opencode.ai/zen/v1\n",
    )
    .unwrap();
    std::env::set_var("XDG_CACHE_HOME", &dir);
    std::env::set_var("DEX_CONFIG", dir.join("config.yaml"));
    std::env::set_var("OPENCODE_API_KEY", "test-key");
    for key in [
        "DEX_MODEL_APIS",
        "DEX_MODELS",
        "DEX_CONTEXT_WINDOW",
        "DEX_THINKING_EFFORT",
    ] {
        std::env::remove_var(key);
    }
    let cfg = LlmConfig::from_env(None, None, None, &[]).unwrap();
    assert_eq!(cfg.thinking_effort.as_deref(), Some("low"));
    std::env::set_var("DEX_THINKING_EFFORT", "medium");
    let cfg = LlmConfig::from_env(None, None, None, &[]).unwrap();
    assert_eq!(cfg.thinking_effort.as_deref(), Some("medium"));
    remember_thinking_effort(&cfg.base_url, &cfg.model, Some("high"));
    let cfg = LlmConfig::from_env(None, None, None, &[]).unwrap();
    assert_eq!(cfg.thinking_effort.as_deref(), Some("high"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// `model: <provider>/<model>` (and `DEX_MODEL`) is the single selection
/// knob: the prefix picks the provider, the rest the model.
#[test]
fn selection_prefix_and_dex_model_pick_provider_and_model() {
    let _env = crate::test_env::TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join(format!("dex-selection-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("dex")).unwrap();
    let _cache = EnvRestore::take(&["XDG_CACHE_HOME"]);
    std::env::set_var("XDG_CACHE_HOME", &dir); // hermetic: no real catalog
    let _guard = EnvRestore::take(&[
        "DEX_CONFIG",
        "DEX_MODEL",
        "OPENCODE_API_KEY",
        "DEX_MODEL_APIS",
        "DEX_MODELS",
        "DEX_CONTEXT_WINDOW",
    ]);
    let path = dir.join("config.yaml");
    std::fs::write(
        &path,
        "providers:\n  zai:\n    api_key: zk\n    base_url: https://zai.example/v1\n",
    )
    .unwrap();
    std::env::set_var("DEX_CONFIG", &path);

    // File `model: zai/glm-5.3`
    std::fs::write(
            &path,
            "providers:\n  zai:\n    api_key: zk\n    base_url: https://zai.example/v1\nmodel: zai/glm-5.3\ncontext_window: 1000\n",
        )
        .unwrap();
    let cfg = LlmConfig::from_env(None, None, None, &[]).unwrap();
    assert!(matches!(cfg.provider, Provider::Generic(ref n) if n == "zai"));
    assert_eq!(cfg.model, "glm-5.3");
    assert_eq!(cfg.base_url, "https://zai.example/v1");
    assert_eq!(cfg.api_key, "zk");

    // `DEX_MODEL` beats the file without touching it (its window rides
    // the env pin).
    std::env::set_var("DEX_CONTEXT_WINDOW", "1000");
    std::env::set_var("DEX_MODEL", "zai/kimi-k2");
    let cfg = LlmConfig::from_env(None, None, None, &[]).unwrap();
    assert!(matches!(cfg.provider, Provider::Generic(ref n) if n == "zai"));
    assert_eq!(cfg.model, "kimi-k2");

    // A bare provider name no longer implies a default model: it is
    // rejected, naming the `provider/<model>` form to use.
    std::env::set_var("DEX_MODEL", "zai");
    let err = match LlmConfig::from_env(None, None, None, &[]) {
        Err(e) => e.to_string(),
        Ok(_) => panic!("bare provider name must not resolve"),
    };
    assert!(err.contains("'zai' names a provider but no model"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn doctor_reports_selection_and_origins() {
    let _env = crate::test_env::TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _guard = EnvRestore::take(&["DEX_CONFIG", "DEX_MODEL", "OPENCODE_API_KEY"]);
    std::env::set_var("OPENCODE_API_KEY", "test-key");
    // Point at a missing file so the host config can't color the output.
    std::env::set_var(
        "DEX_CONFIG",
        std::env::temp_dir().join(format!("dex-missing-doctor-{}", std::process::id())),
    );
    let out = doctor(None, None, None, &[], None).0;
    assert!(out.contains("provider"), "{out}");
    assert!(out.contains("model"), "{out}");
    // Pin the guide's key-env wording, not a generic substring: the
    // unconfigured report has no `api key` row (no provider resolved),
    // so a bare "key env" assert would pass on guide text alone and
    // mask a key-row regression elsewhere.
    assert!(out.contains("endpoint + key env from the catalog"), "{out}");
    assert!(out.contains("built-in default"), "{out}");
    assert!(out.contains("resolve"), "{out}");
}

/// A bare provider pick (`DEX_MODEL=anthropic`) surfaces as the provider
/// row, never as a model id: the model row is unset (the selection named
/// as its origin) and the resolve row carries the fix.
#[test]
fn doctor_shows_bare_provider_pick_as_provider() {
    let _env = crate::test_env::TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _guard = EnvRestore::take(&["DEX_CONFIG", "DEX_MODEL", "OPENCODE_API_KEY"]);
    std::env::set_var("OPENCODE_API_KEY", "test-key");
    std::env::set_var("DEX_MODEL", "anthropic");
    // Point at a missing file so the host config can't color the output.
    std::env::set_var(
        "DEX_CONFIG",
        std::env::temp_dir().join(format!("dex-bare-doctor-{}", std::process::id())),
    );
    let out = doctor(None, None, None, &[], None).0;
    let prov = out
        .lines()
        .find(|l| l.starts_with("provider "))
        .expect("provider row");
    assert!(prov.contains("anthropic"), "{prov}");
    assert!(prov.contains("DEX_MODEL"), "{prov}");
    let model = out
        .lines()
        .find(|l| l.starts_with("model "))
        .expect("model row");
    assert!(model.contains("(unset)"), "{model}");
    assert!(!model.contains("anthropic"), "{model}");
}

/// Rows whose value overflows the value column wrap instead of
/// colliding with the origin text; the origin hangs at the origin
/// column (display columns 18+46 = 64).
#[test]
fn doctor_wraps_overlong_value_and_hangs_origin() {
    let _env = crate::test_env::TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _guard = EnvRestore::take(&["DEX_CONFIG", "DEX_MODEL", "OPENCODE_API_KEY"]);
    // Missing file, so the config row prints the path + a source note.
    std::env::set_var(
        "DEX_CONFIG",
        format!(
            "{}/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/config.yaml",
            std::env::temp_dir().display()
        ),
    );
    let out = doctor(None, None, None, &[], None).0;
    let mut lines = out.lines();
    while let Some(line) = lines.next() {
        if !line.starts_with("config ") {
            continue;
        }
        // The whole path is the only text on its own line.
        assert!(
            line.trim_end().ends_with("config.yaml"),
            "value keeps its line: {line:?}"
        );
        // The origin starts at the origin column on the next line.
        let origin = lines.next().expect("origin line");
        assert_eq!(
            origin.find("missing or invalid"),
            Some(24 + 46),
            "origin hangs at the origin column: {origin:?}"
        );
        return;
    }
    panic!("no config row in:\n{out}");
}

/// Padding counts display columns: a CJK path is 31 chars but only 45
/// columns wide, so the origin still lands on column 70.
#[test]
fn doctor_pads_by_display_width() {
    let _env = crate::test_env::TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _guard = EnvRestore::take(&["DEX_CONFIG", "DEX_MODEL", "OPENCODE_API_KEY"]);
    // "/tmp/" (5 cols) + 14 CJK chars (28 cols) + "/config.yaml" (12 cols) = 45.
    std::env::set_var(
        "DEX_CONFIG",
        format!("/tmp/{}/config.yaml", "配置文件配置文件配置文件配置"),
    );
    let out = doctor(None, None, None, &[], None).0;
    let line = out
        .lines()
        .find(|l| l.starts_with("config "))
        .expect("config row");
    let at = line
        .find("missing or invalid")
        .expect("origin inline on the value line");
    assert_eq!(
        UnicodeWidthStr::width(&line[..at]),
        24 + 46,
        "origin starts at display column 70: {line:?}"
    );
}

/// Byte-for-byte `doctor` output under a fully hermetic scenario: no
/// config file, no catalog caches, no dex env vars, one provider key.
/// This is the PR-18 motion gate — any refactor of the shared
/// resolution must reproduce this output exactly. Fixed paths (no pid)
/// keep the snapshot stable across runs and machines.
#[test]
fn doctor_output_is_byte_stable() {
    let _env = crate::test_env::TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _guard = EnvRestore::take(&[
        "DEX_CONFIG",
        "XDG_CACHE_HOME",
        "XDG_DATA_HOME",
        "XDG_CONFIG_HOME",
        "DEX_MODEL",
        "DEX_MODELS",
        "DEX_CONTEXT_WINDOW",
        "DEX_RESERVE_TOKENS",
        "DEX_KEEP_RECENT_TOKENS",
        "DEX_THINKING_EFFORT",
        "DEX_SYSTEM_PROMPT",
        "DEX_SYSTEM_PROMPT_FILE",
        "DEX_PERMISSION",
        "DEX_HEADERS",
        "DEX_AGENT_WAKE",
        "ANTHROPIC_CUSTOM_HEADERS",
        "OPENAI_HEADERS",
        "OPENCODE_API_KEY",
        "TYPESAFE_API_KEY",
        "TYPESAFE_JEV_URL",
        "DEX_MAX_TOOL_ITERATIONS",
        "XDG_CONFIG_HOME",
        "DEX_EXTENSIONS_PATHS",
    ]);
    std::env::remove_var("DEX_MAX_TOOL_ITERATIONS");
    std::env::remove_var("DEX_EXTENSIONS_PATHS");
    std::env::remove_var("DEX_SYSTEM_PROMPT");
    std::env::remove_var("DEX_SYSTEM_PROMPT_FILE");
    std::env::set_var("OPENCODE_API_KEY", "test-key");
    std::env::set_var("DEX_CONFIG", "/tmp/dex-doctor-snapshot/missing.yaml");
    std::env::set_var("XDG_CACHE_HOME", "/tmp/dex-doctor-snapshot/cache");
    std::env::set_var("XDG_DATA_HOME", "/tmp/dex-doctor-snapshot/data");
    // Hermetic extension discovery too: the extensions row reads the
    // XDG config dir, which must not see the developer's real installs.
    std::env::set_var("XDG_CONFIG_HOME", "/tmp/dex-doctor-snapshot/config");
    let out = doctor(None, None, None, &[], None).0;
    let expected = format!(
            "{}{}",
            concat!(
                concat!("dex ", env!("CARGO_PKG_VERSION"), "\n"),
                "\n",
                "config                  /tmp/dex-doctor-snapshot/missing.yaml         missing or invalid — ignored (env/defaults still apply)\n",
                "catalog                 /tmp/dex-doctor-snapshot/cache/dex/models.dev.json\n",
                "                                                                      missing — run `dex update --models`\n",
                "\n",
                "provider                (unset)                                       UNCONFIGURED — set 'model: <provider>/<model>'\n",
            ),
            concat!(
                "compaction              deterministic                                 built-in default\n",
                "harness tools           200                                           built-in default\n",
                "harness batch           10                                            built-in default\n",
                "harness compact         3                                             built-in default\n",
                "harness repeat          3                                             built-in default\n",
                "harness cut             keep 12 msgs / summarize >= 8                 built-in default\n",
                "harness prune           keep <500/trunc >2000/drop >10000 chars       built-in default\n",
                "jev scorer              heuristic scorer                              no TYPESAFE_API_KEY\n",
                "thinking                (unset)                                       model default\n",
                "permission              trusted                                       built-in default\n",
                "agent wake              on                                            built-in default\n",
                "headers                 0                                             none\n",
                "system prompt           default                                       built-in default\n",
                "extensions              none                                          cwd/.dex, XDG config dirs\n",
                "\n",
                "resolve                 ERROR                                         no model configured — set 'model: <provider>/<model>' in the config, then run `dex doctor`:\n",
                "                                                                        quickest (OpenCode Zen gateway): model: opencode/gpt-5-nano + providers.opencode.api_key (or OPENCODE_API_KEY)\n",
                "                                                                        anthropic: model: anthropic/<model-id> + providers.anthropic.api_key (or ANTHROPIC_API_KEY)\n",
                "                                                                        other gateways: model: <provider>/<model-id> + providers.<provider>: {api_key} (endpoint + key env from the catalog — run `dex update --models` first)\n",
                "                                                                        custom endpoint (Bearer + Anthropic wire): model: gateway/<model-id> + providers.gateway: {base_url: https://gateway.example/v1, api_key, api: anthropic-messages}\n",
                "                                                                        codex: model: openai-codex/<model-id> + run `codex --login` (or CODEX_ACCESS_TOKEN)\n",
                "                                                                        more copy-paste samples: examples/config.yaml\n",
                "                                                                      config: /tmp/dex-doctor-snapshot/missing.yaml\n",
            )
        );
    assert_eq!(out, expected, "doctor output drifted");
}

#[test]
fn system_prompt_precedence_is_cli_env_file_default() {
    let _env = crate::test_env::TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _guard = EnvRestore::take(&[
        "DEX_CONFIG",
        "DEX_SYSTEM_PROMPT",
        "DEX_SYSTEM_PROMPT_FILE",
        "XDG_CACHE_HOME",
    ]);
    let dir = std::env::temp_dir().join(format!("dex-sprompt-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let prompt_file = dir.join("prompt.md");
    std::fs::write(&prompt_file, "file prompt").unwrap();
    let env_file = dir.join("env.md");
    std::fs::write(&env_file, "env file prompt").unwrap();
    std::fs::write(
        dir.join("config.yaml"),
        "model: opencode/m\ncontext_window: 1000\nsystem_prompt: file inline\n",
    )
    .unwrap();
    std::env::set_var("DEX_CONFIG", dir.join("config.yaml"));
    std::env::set_var("XDG_CACHE_HOME", dir.join("cache"));
    for key in ["DEX_SYSTEM_PROMPT", "DEX_SYSTEM_PROMPT_FILE"] {
        std::env::remove_var(key);
    }
    // File inline wins over nothing.
    let (text, origin) = super::system_prompt_origin(None);
    assert_eq!(text.as_deref(), Some("file inline"));
    assert_eq!(origin, "config system_prompt:");
    // Env inline beats file.
    std::env::set_var("DEX_SYSTEM_PROMPT", "env inline");
    let (text, origin) = super::system_prompt_origin(None);
    assert_eq!(text.as_deref(), Some("env inline"));
    assert_eq!(origin, "DEX_SYSTEM_PROMPT");
    // Explicit CLI text beats env.
    let (text, origin) = super::system_prompt_origin(Some("cli text"));
    assert_eq!(text.as_deref(), Some("cli text"));
    assert_eq!(origin, "--system-prompt");
    std::env::remove_var("DEX_SYSTEM_PROMPT");
    // Env file beats file inline; file `system_prompt_file:` is the last
    // layer before the default.
    std::env::set_var("DEX_SYSTEM_PROMPT_FILE", &env_file);
    let (text, origin) = super::system_prompt_origin(None);
    assert_eq!(text.as_deref(), Some("env file prompt"));
    assert_eq!(origin, "DEX_SYSTEM_PROMPT_FILE");
    std::env::remove_var("DEX_SYSTEM_PROMPT_FILE");
    // Within the file layer, inline beats file.
    std::fs::write(
            dir.join("config.yaml"),
            format!(
                "model: opencode/m\ncontext_window: 1000\nsystem_prompt: inline wins\nsystem_prompt_file: {}\n",
                prompt_file.display()
            ),
        )
        .unwrap();
    let (text, origin) = super::system_prompt_origin(None);
    assert_eq!(text.as_deref(), Some("inline wins"));
    assert_eq!(origin, "config system_prompt:");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn system_prompt_file_layer_reads_absolute_path() {
    let _env = crate::test_env::TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _guard = EnvRestore::take(&[
        "DEX_CONFIG",
        "DEX_SYSTEM_PROMPT",
        "DEX_SYSTEM_PROMPT_FILE",
        "XDG_CACHE_HOME",
    ]);
    let dir = std::env::temp_dir().join(format!("dex-spfile-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let prompt_file = dir.join("prompt.md");
    std::fs::write(&prompt_file, "from file layer").unwrap();
    std::fs::write(
        dir.join("config.yaml"),
        format!(
            "model: opencode/m\ncontext_window: 1000\nsystem_prompt_file: {}\n",
            prompt_file.display()
        ),
    )
    .unwrap();
    std::env::set_var("DEX_CONFIG", dir.join("config.yaml"));
    std::env::set_var("XDG_CACHE_HOME", dir.join("cache"));
    for key in ["DEX_SYSTEM_PROMPT", "DEX_SYSTEM_PROMPT_FILE"] {
        std::env::remove_var(key);
    }
    let (text, origin) = super::system_prompt_origin(None);
    assert_eq!(text.as_deref(), Some("from file layer"));
    assert_eq!(origin, "config system_prompt_file:");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn resolve_cli_system_prompt_prefers_inline_and_errors_on_missing_file() {
    let dir = std::env::temp_dir().join(format!("dex-clicfg-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let prompt_file = dir.join("p.md");
    std::fs::write(&prompt_file, "cli file text").unwrap();
    // Inline wins when both are set.
    assert_eq!(
        super::resolve_cli_system_prompt(
            Some("inline".to_string()),
            Some(prompt_file.display().to_string()),
        )
        .unwrap(),
        Some(("inline".to_string(), "--system-prompt"))
    );
    // File content is returned verbatim with the file-flag origin.
    assert_eq!(
        super::resolve_cli_system_prompt(None, Some(prompt_file.display().to_string())).unwrap(),
        Some(("cli file text".to_string(), "--system-prompt-file"))
    );
    // Whitespace-only file content counts as unset and falls through.
    std::fs::write(dir.join("blank.md"), "   \n").unwrap();
    assert_eq!(
        super::resolve_cli_system_prompt(None, Some(dir.join("blank.md").display().to_string()))
            .unwrap(),
        None
    );
    // Missing file is a hard error, never a silent default.
    assert!(super::resolve_cli_system_prompt(
        None,
        Some(dir.join("missing.md").display().to_string())
    )
    .is_err());
    assert_eq!(super::resolve_cli_system_prompt(None, None).unwrap(), None);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn doctor_system_prompt_row_names_origin() {
    let _env = crate::test_env::TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _guard = EnvRestore::take(&[
        "DEX_CONFIG",
        "DEX_MODEL",
        "OPENCODE_API_KEY",
        "DEX_SYSTEM_PROMPT",
        "DEX_SYSTEM_PROMPT_FILE",
        "XDG_CACHE_HOME",
    ]);
    std::env::set_var("OPENCODE_API_KEY", "test-key");
    std::env::set_var(
        "DEX_CONFIG",
        std::env::temp_dir().join(format!("dex-spdoc-{}", std::process::id())),
    );
    std::env::set_var(
        "XDG_CACHE_HOME",
        std::env::temp_dir().join(format!("dex-spdoc-cache-{}", std::process::id())),
    );
    for key in ["DEX_MODEL", "DEX_SYSTEM_PROMPT", "DEX_SYSTEM_PROMPT_FILE"] {
        std::env::remove_var(key);
    }
    let out = doctor(None, None, None, &[], None).0;
    let prow = out
        .lines()
        .find(|l| l.starts_with("system prompt "))
        .expect("system prompt row");
    assert!(prow.contains("default"), "{prow}");
    assert!(prow.contains("built-in default"), "{prow}");
    std::env::set_var("DEX_SYSTEM_PROMPT", "custom base");
    let out = doctor(None, None, None, &[], None).0;
    let prow = out
        .lines()
        .find(|l| l.starts_with("system prompt "))
        .expect("system prompt row");
    assert!(prow.contains("custom (11 chars)"), "{prow}");
    assert!(prow.contains("DEX_SYSTEM_PROMPT"), "{prow}");
    // Explicit CLI text wins and reports as a flag.
    let out = doctor(
        None,
        None,
        None,
        &[],
        Some(("cli".to_string(), "--system-prompt")),
    )
    .0;
    let prow = out
        .lines()
        .find(|l| l.starts_with("system prompt "))
        .expect("system prompt row");
    assert!(prow.contains("--system-prompt"), "{prow}");
    // File-flag text reports its own origin.
    let out = doctor(
        None,
        None,
        None,
        &[],
        Some(("cli".to_string(), "--system-prompt-file")),
    )
    .0;
    let prow = out
        .lines()
        .find(|l| l.starts_with("system prompt "))
        .expect("system prompt row");
    assert!(prow.contains("--system-prompt-file"), "{prow}");
}

#[test]
fn whitespace_file_inline_system_prompt_falls_through_to_default() {
    let _env = crate::test_env::TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _guard = EnvRestore::take(&[
        "DEX_CONFIG",
        "DEX_SYSTEM_PROMPT",
        "DEX_SYSTEM_PROMPT_FILE",
        "XDG_CACHE_HOME",
    ]);
    let dir = std::env::temp_dir().join(format!("dex-spblank-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("config.yaml"),
        "model: opencode/m\ncontext_window: 1000\nsystem_prompt: '   '\n",
    )
    .unwrap();
    std::env::set_var("DEX_CONFIG", dir.join("config.yaml"));
    std::env::set_var("XDG_CACHE_HOME", dir.join("cache"));
    for key in ["DEX_SYSTEM_PROMPT", "DEX_SYSTEM_PROMPT_FILE"] {
        std::env::remove_var(key);
    }
    let (text, origin) = super::system_prompt_origin(None);
    assert_eq!(text, None);
    assert_eq!(origin, "built-in default");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Hermetic config for the extension snapshot tests: a generic provider
/// with its own endpoint + key, so resolution stays cache-less (no
/// catalog, no env keys).
fn write_extmodel_config(dir: &std::path::Path) {
    std::fs::write(
            dir.join("config.yaml"),
            "model: myprov/m-7\nproviders:\n  myprov:\n    base_url: https://myprov.example/v1\n    api_key: k-123\n",
        )
        .unwrap();
}

#[test]
fn extension_model_snapshot_resolves_selection_and_endpoint() {
    let _env = crate::test_env::TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _guard = EnvRestore::take(&[
        "DEX_CONFIG",
        "DEX_MODEL",
        "DEX_MODEL_APIS",
        "XDG_CACHE_HOME",
    ]);
    let dir = std::env::temp_dir().join(format!("dex-extmodel-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    write_extmodel_config(&dir);
    std::env::set_var("DEX_CONFIG", dir.join("config.yaml"));
    std::env::set_var("XDG_CACHE_HOME", dir.join("cache"));
    for key in ["DEX_MODEL", "DEX_MODEL_APIS"] {
        std::env::remove_var(key);
    }
    let snap = super::extension_model_snapshot().unwrap();
    assert_eq!(snap.provider, "myprov");
    assert_eq!(snap.model, "m-7");
    assert_eq!(snap.base_url, "https://myprov.example/v1");
    assert_eq!(snap.api, "openai-responses");
    assert_eq!(snap.id(), "myprov/m-7");
    let auth = super::extension_model_auth().unwrap();
    assert_eq!(auth.api_key, "k-123");
    assert_eq!(auth.base_url, "https://myprov.example/v1");
    assert!(auth
        .headers
        .keys()
        .all(|k| !k.eq_ignore_ascii_case("authorization")));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn extension_model_snapshot_errors_without_selection() {
    let _env = crate::test_env::TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _guard = EnvRestore::take(&["DEX_CONFIG", "DEX_MODEL", "XDG_CACHE_HOME"]);
    let dir = std::env::temp_dir().join(format!("dex-extmodel-no-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("config.yaml"), "context_window: 1000\n").unwrap();
    std::env::set_var("DEX_CONFIG", dir.join("config.yaml"));
    std::env::set_var("XDG_CACHE_HOME", dir.join("cache"));
    std::env::remove_var("DEX_MODEL");
    let err = super::extension_model_snapshot().unwrap_err();
    assert!(err.contains("no model configured"), "got: {err}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn extension_model_auth_merges_headers_and_drops_authorization() {
    let _env = crate::test_env::TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _guard = EnvRestore::take(&["DEX_CONFIG", "DEX_MODEL", "DEX_HEADERS", "XDG_CACHE_HOME"]);
    let dir = std::env::temp_dir().join(format!("dex-extmodel-h-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
            dir.join("config.yaml"),
            "model: myprov/m-7\nheaders:\n  X-A: global\n  authorization: Bearer file-key\nproviders:\n  myprov:\n    base_url: https://myprov.example/v1\n    api_key: k-123\n    headers:\n      X-A: scoped\n      X-B: scoped\n",
        )
        .unwrap();
    std::env::set_var("DEX_CONFIG", dir.join("config.yaml"));
    std::env::set_var("XDG_CACHE_HOME", dir.join("cache"));
    for key in ["DEX_MODEL", "DEX_HEADERS"] {
        std::env::remove_var(key);
    }
    let auth = super::extension_model_auth().unwrap();
    // Provider-scoped entries beat the global table per key (AGENTS.md
    // precedence); `authorization` never travels with the headers (the
    // key goes separately).
    assert_eq!(auth.headers.get("X-A").map(String::as_str), Some("scoped"));
    assert_eq!(auth.headers.get("X-B").map(String::as_str), Some("scoped"));
    assert!(auth
        .headers
        .keys()
        .all(|k| !k.eq_ignore_ascii_case("authorization")));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn builtin_provider_names_stay_in_sync_with_parse_known() {
    // `extension_configured_providers` seeds discovery from this list; every
    // entry must resolve without any file entry, or builtins silently drop
    // out of `net.providers` / `dex.model.providers()`.
    let known = std::collections::BTreeSet::new();
    for name in crate::protocol::Provider::BUILTINS {
        assert!(
            crate::protocol::Provider::parse_known(name, &known).is_some(),
            "BUILTINS entry '{name}' must parse without any file entry"
        );
    }
    // Alias spellings land on one canonical provider.
    assert_eq!(
        crate::protocol::Provider::parse_known("codex", &known).map(|p| p.name().to_string()),
        crate::protocol::Provider::parse_known("openai-codex", &known)
            .map(|p| p.name().to_string()),
    );
}

#[test]
fn extension_provider_auth_resolves_explicit_provider() {
    let _env = crate::test_env::TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _guard = EnvRestore::take(&[
        "DEX_CONFIG",
        "DEX_MODEL",
        "DEX_MODEL_APIS",
        "XDG_CACHE_HOME",
    ]);
    let dir = std::env::temp_dir().join(format!("dex-provauth-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
            dir.join("config.yaml"),
            "model: myprov/m-7\nproviders:\n  myprov:\n    base_url: https://myprov.example/v1\n    api_key: k-123\n  anthropic:\n    base_url: https://anthropic.example\n    api_key: k-ant\n    api: anthropic-messages\n",
        )
        .unwrap();
    std::env::set_var("DEX_CONFIG", dir.join("config.yaml"));
    std::env::set_var("XDG_CACHE_HOME", dir.join("cache"));
    for key in ["DEX_MODEL", "DEX_MODEL_APIS"] {
        std::env::remove_var(key);
    }
    // A configured provider with its own endpoint, key and wire pin.
    let resolved = super::extension_provider_auth("anthropic").unwrap();
    assert_eq!(resolved.auth.api_key, "k-ant");
    assert_eq!(resolved.auth.base_url, "https://anthropic.example");
    assert_eq!(resolved.api, "anthropic-messages");
    // Unknown provider: the error names the deposit place.
    let unknown = match super::extension_provider_auth("ghostprov") {
        Err(e) => e,
        Ok(_) => panic!("ghostprov has no deposits and must not resolve"),
    };
    assert!(
        unknown.contains("add it under 'providers:'"),
        "got: {unknown}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn extension_configured_providers_lists_resolvable_endpoints() {
    let _env = crate::test_env::TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _guard = EnvRestore::take(&[
        "DEX_CONFIG",
        "DEX_MODEL",
        "DEX_MODEL_APIS",
        "XDG_CACHE_HOME",
    ]);
    let dir = std::env::temp_dir().join(format!("dex-provlist-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
            dir.join("config.yaml"),
            "model: myprov/m-7\nproviders:\n  myprov:\n    base_url: https://myprov.example/v1\n    api_key: k-123\n  nokey:\n    base_url: https://nokey.example\n",
        )
        .unwrap();
    std::env::set_var("DEX_CONFIG", dir.join("config.yaml"));
    std::env::set_var("XDG_CACHE_HOME", dir.join("cache"));
    for key in ["DEX_MODEL", "DEX_MODEL_APIS"] {
        std::env::remove_var(key);
    }
    let providers: Vec<String> = super::extension_configured_providers()
        .into_iter()
        .map(|e| e.provider)
        .collect();
    // `myprov` has key + endpoint; `nokey` has no key deposit and is
    // skipped — the list only names providers that could actually
    // authenticate. (Builtins appear when this machine holds their
    // credentials, so the assertion is membership, not equality.)
    assert!(
        providers.contains(&"myprov".to_string()),
        "got: {providers:?}"
    );
    assert!(
        !providers.contains(&"nokey".to_string()),
        "got: {providers:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn harness_table_reads_file_and_rejects_zero() {
    let _env = crate::test_env::TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _guard = EnvRestore::take(&["DEX_CONFIG", "XDG_CACHE_HOME"]);
    let dir = std::env::temp_dir().join(format!("dex-harness-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("config.yaml"),
        "harness:\n  batch_max_concurrent: 3\n  max_compaction_attempts: 0\n",
    )
    .unwrap();
    std::env::set_var("DEX_CONFIG", dir.join("config.yaml"));
    std::env::set_var("XDG_CACHE_HOME", dir.join("cache"));
    super::invalidate_config_cache();
    assert_eq!(super::load_harness_num("batch_max_concurrent"), Some(3));
    assert_eq!(super::load_harness_num("max_compaction_attempts"), None);
    assert_eq!(super::load_harness_num("missing_key"), None);
    super::invalidate_config_cache();
    let _ = std::fs::remove_dir_all(&dir);
}
