//! Shared streaming boundary. The concrete SSE readers remain compatible with
//! both provider protocols and are called through these typed entry points.

use crate::core::types::{ApiProtocol, ChatMessage, SinkLine};
use crate::llm::config::LlmConfig;
use crate::llm::stream::Turn;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// Models empirically switched to chat-completions after the responses API
/// rejected them (e.g. glm-5.3-flash on zen/go 500s on `/responses`, 200s on
/// `/chat/completions`). Keyed by (base_url, model); per-process, so the
/// one-time cost of learning is a single failed call per model per run.
/// ponytail: in-memory only — re-learned on restart; persist to the cache dir
/// if cold-start latency for completions-only models ever matters.
fn probed_apis() -> &'static Mutex<HashMap<(String, String), ApiProtocol>> {
    static MAP: OnceLock<Mutex<HashMap<(String, String), ApiProtocol>>> = OnceLock::new();
    MAP.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Failure raised after output already streamed; defined in `stream.rs` (the
/// stream driver knows when output actually flowed), re-exported here where
/// the protocol-fallback gate consumes it.
pub(crate) use crate::llm::stream::MidStreamError;

/// Only a failure with no streamed output (HTTP status, connect failure,
/// drop before the first delta) qualifies for protocol fallback; a
/// mid-stream failure may have already put partial text on the transcript,
/// and a retried call would duplicate it.
pub(crate) fn is_mid_stream(err: &(dyn std::error::Error + 'static)) -> bool {
    err.downcast_ref::<MidStreamError>().is_some()
}

/// Wire protocol for this call: an explicit pin (config-file `api:` or the
/// provider entry's, baked into `config.api_pinned`) or a `DEX_MODEL_APIS`
/// entry always wins; otherwise a learned fallback overrides the configured
/// default (`openai-responses`).
fn effective_api(config: &LlmConfig) -> ApiProtocol {
    if config.api_pinned {
        return config.api;
    }
    // Return the table value itself, not `config.api`: a pinned endpoint
    // (`--base-url` / file `base_url:`) skips `apply_model`, so `config.api`
    // may still hold the global default while the table names completions.
    if let Some(api) = crate::llm::config::model_api_from_env(&config.model, &config.model) {
        return api;
    }
    probed_apis()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&(config.base_url.clone(), config.model.clone()))
        .copied()
        .unwrap_or(config.api)
}

/// May we infer the protocol by retrying a failed `/responses` call as
/// chat-completions? Only when nothing explicitly pinned the protocol, the
/// provider exposes both wire shapes, and the failure isn't a cancellation.
fn try_responses_fallback(config: &LlmConfig, err: &str) -> bool {
    if config.api_pinned {
        return false; // user pinned one protocol for everything
    }
    if crate::llm::config::model_api_from_env(&config.model, &config.model).is_some() {
        return false; // explicit per-model table entry
    }
    if !config.provider.has_protocol_fallback() {
        return false; // codex backend-api has no /chat/completions
    }
    !err.contains("cancelled")
}

pub(crate) async fn complete(
    config: &LlmConfig,
    messages: &[ChatMessage],
    with_tools: bool,
    sink: Option<tokio::sync::mpsc::Sender<crate::core::types::SinkLine>>,
    cancel: &(dyn crate::agent::state::CancellationSource + Send + Sync),
) -> Result<Turn, Box<dyn std::error::Error + Send + Sync>> {
    match effective_api(config) {
        ApiProtocol::ChatCompletions => {
            crate::llm::client::call_chat_completions(config, messages, with_tools, sink, cancel)
                .await
        }
        // Native Messages endpoint: no empirical fallback — the endpoint
        // speaks one wire, and a pin (or the provider default) already
        // decided it.
        ApiProtocol::Anthropic => {
            crate::llm::client::call_anthropic_messages(config, messages, with_tools, sink, cancel)
                .await
        }
        ApiProtocol::Responses => {
            match crate::llm::client::call_responses(
                config,
                messages,
                with_tools,
                sink.clone(),
                cancel,
            )
            .await
            {
                Ok(ok) => Ok(ok),
                Err(e) if !is_mid_stream(&*e) && try_responses_fallback(config, &e.to_string()) => {
                    // Empirical protocol inference: responses API rejected the
                    // model — try chat-completions once and remember. The gate
                    // is deliberately broad: any pre-output failure on a 200'd
                    // /responses call (immediate EOF, connect blip) re-issues
                    // the whole turn over chat-completions; once anything has
                    // streamed, MidStreamError blocks the retry so a partial
                    // transcript is never duplicated.
                    match crate::llm::client::call_chat_completions(
                        config,
                        messages,
                        with_tools,
                        sink.clone(),
                        cancel,
                    )
                    .await
                    {
                        Ok(ok) => {
                            probed_apis()
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .insert(
                                    (config.base_url.clone(), config.model.clone()),
                                    ApiProtocol::ChatCompletions,
                                );
                            // Survive restarts / one-shot runs.
                            crate::llm::config::remember_learned_api(
                                &config.base_url,
                                &config.model,
                                ApiProtocol::ChatCompletions,
                            );
                            if let Some(sink) = &sink {
                                let _ = sink
                                    .send(SinkLine::System(format!(
                                        "auto: {} speaks openai-completions (responses API failed); remembered for future runs",
                                        config.model
                                    )))
                                    .await;
                            }
                            Ok(ok)
                        }
                        // The original responses error is the canonical one.
                        Err(_) => Err(e),
                    }
                }
                Err(e) => Err(e),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::Provider;
    use crate::llm::config::tests::test_cfg;

    /// Set/restore env around gate tests (local copy of config's EnvRestore).
    struct EnvGuard {
        prev: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }

    impl EnvGuard {
        fn clear(keys: &[&'static str]) -> Self {
            let prev = keys.iter().map(|k| (*k, std::env::var_os(k))).collect();
            for k in keys {
                std::env::remove_var(k);
            }
            Self { prev }
        }

        /// Point `key` at `value`, restoring the previous value on drop.
        fn set(mut self, key: &'static str, value: &std::path::Path) -> Self {
            self.prev.push((key, std::env::var_os(key)));
            std::env::set_var(key, value);
            self
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (k, v) in self.prev.drain(..) {
                match v {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
        }
    }

    #[test]
    fn fallback_gate_and_learned_protocol() {
        let _lock = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut cfg = test_cfg();
        {
            // Hermetic: no real env pins and no developer config.yaml.
            let absent = std::env::temp_dir().join("dex-gate-test-absent.yaml");
            let _env = EnvGuard::clear(&["DEX_MODEL_APIS"]).set("DEX_CONFIG", &absent);
            // Unpinned opencode model: fallback allowed.
            assert!(try_responses_fallback(&cfg, "500 Internal server error"));
            // Never on cancellation.
            assert!(!try_responses_fallback(&cfg, "cancelled"));
            // Explicit DEX_MODEL_APIS entry — user already decided.
            std::env::set_var("DEX_MODEL_APIS", "m-r=openai-responses");
            assert!(!try_responses_fallback(&cfg, "500 boom"));
            std::env::remove_var("DEX_MODEL_APIS");
            // A baked-in pin (config-file `api:` / provider entry `api:`,
            // computed once by `LlmConfig::from_env`) blocks inference too.
            cfg.api_pinned = true;
            assert!(!try_responses_fallback(&cfg, "500 boom"));
            cfg.api_pinned = false;
            // Codex backend has no /chat/completions.
            cfg.provider = Provider::OpenAiCodex;
            assert!(!try_responses_fallback(&cfg, "500 boom"));
            cfg.provider = Provider::OpenCode;
            // Learned protocol overrides the configured default.
            assert_eq!(effective_api(&cfg), ApiProtocol::Responses);
            probed_apis()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(
                    (cfg.base_url.clone(), cfg.model.clone()),
                    ApiProtocol::ChatCompletions,
                );
            assert_eq!(effective_api(&cfg), ApiProtocol::ChatCompletions);
            // But an explicit table entry still wins over what we learned.
            std::env::set_var("DEX_MODEL_APIS", "m-r=openai-responses");
            assert_eq!(effective_api(&cfg), ApiProtocol::Responses);
        }
        probed_apis()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    #[test]
    fn effective_api_returns_table_value_not_unresolved_default() {
        // A pinned endpoint skips `apply_model`, so `config.api` may still
        // hold the global default while the table names completions — the
        // request must follow the table, not the stale default.
        let _lock = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _env = EnvGuard::clear(&["DEX_MODEL_APIS"]);
        probed_apis()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        let cfg = test_cfg();
        assert_eq!(cfg.api, ApiProtocol::Responses);
        std::env::set_var("DEX_MODEL_APIS", "m-r=openai-completions");
        assert_eq!(effective_api(&cfg), ApiProtocol::ChatCompletions);
        probed_apis()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    #[test]
    fn mid_stream_marker_blocks_fallback_and_keeps_message() {
        let err: Box<dyn std::error::Error + Send + Sync> =
            Box::new(MidStreamError("cancelled".into()));
        assert!(is_mid_stream(&*err));
        // Display passes the message through untouched so callers matching on
        // `"cancelled"` (exact or substring) keep working.
        assert_eq!(err.to_string(), "cancelled");
        let plain: Box<dyn std::error::Error + Send + Sync> = "API error: boom".into();
        assert!(!is_mid_stream(&*plain));
    }
}
