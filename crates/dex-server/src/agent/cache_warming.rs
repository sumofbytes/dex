//! Prompt-cache warming: keep the provider's prompt cache alive across idle
//! gaps so the next turn re-reads the context at cache-hit price instead of
//! full input price.
//!
//! Anthropic-native turns mark their prefix with `cache_control`; the cache
//! expires after ~5 minutes idle. After a completed turn, [`schedule`]
//! re-sends the exact last request (same messages, tools, cache marks) once
//! at TTL minus margin with a 64-token output cap. The reply is dropped —
//! it never enters the session, journal, or usage totals. A newer schedule
//! for the same session cancels the pending one (and the real next turn
//! refreshes the cache anyway, so a skipped ping is always safe).

use std::sync::Arc;
use std::time::Duration;

use dex_ai::ChatMessage;

use crate::llm::client::{http_call, AnthropicMessages, WireProtocol};
use crate::llm::config::{cache_warming_origin, resolve_model_cost, LlmConfig};
use crate::protocol::{ApiProtocol, ToolDefinition};
use crate::runtime::cancel::GlobalCancellation;
use crate::runtime::console::CancellationToken;

use crate::daemon::state::DaemonState;

/// Fire this long after the turn: late enough that the turn's own request
/// no longer refreshed the provider's ~5-minute prompt cache, early enough
/// to beat expiry (TTL minus a 30-second margin).
const WARM_AFTER: Duration = Duration::from_secs(270);
/// Skip pings that cannot plausibly pay for themselves (pi's $0.05 shape).
const MIN_SAVED_USD: f64 = 0.05;
/// Output cap for the ping: the response is dropped, so keep it tiny. Also
/// forces `thinking` off — a thinking budget cannot fit under the cap.
pub(crate) const WARM_MAX_TOKENS: u64 = 64;

/// The last request a turn sent, captured in `before_model` (messages and
/// schemas there are exactly what the engine sends next). `cached_tokens`
/// is the provider-reported cache-hit subset of that request's prompt.
#[derive(Clone, Default)]
pub struct WarmCapture {
    pub messages: Vec<ChatMessage>,
    pub tools: Vec<ToolDefinition>,
    pub cached_tokens: Option<u64>,
}

impl WarmCapture {
    /// Estimated USD saved by one ping: the tokens that would otherwise be
    /// re-read at full input price instead of cache-read price. `None` when
    /// there is no pricing entry — the caller then falls back to a raw
    /// token-count floor.
    fn avoided_cost(&self, config: &LlmConfig) -> Option<f64> {
        let cached = self.cached_tokens?;
        let cost = resolve_model_cost(
            &config.model,
            &config.provider.catalog_keys(),
            &config.base_url,
        )?;
        let read = cost.cache_read.unwrap_or(cost.input);
        #[allow(clippy::cast_precision_loss)]
        let saved = cached as f64 * (cost.input - read).abs() / 1_000_000.0;
        Some(saved)
    }

    fn worth_warming(&self, config: &LlmConfig) -> bool {
        // # ponytail: fixed token floor when the catalog has no pricing;
        // per-tier thresholds if real-world sessions say so.
        match self.avoided_cost(config) {
            Some(saved) => saved >= MIN_SAVED_USD,
            None => self.cached_tokens.unwrap_or(0) >= 50_000,
        }
    }
}

/// Schedule one warming ping for `session_id` from a completed turn's
/// capture. No-op unless the model rides the Anthropic wire, warming is
/// enabled, and the ping pays for itself. A later schedule for the same
/// session cancels this one.
pub(crate) fn schedule(
    state: &Arc<DaemonState>,
    session_id: &str,
    config: &LlmConfig,
    capture: WarmCapture,
) {
    if !cache_warming_origin().0
        || !matches!(
            crate::llm::dispatch::effective_api(config),
            ApiProtocol::Anthropic
        )
        || !capture.worth_warming(config)
        || capture.messages.is_empty()
    {
        return;
    }
    let token = CancellationToken::new();
    let replaced = state
        .warm_tokens
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(session_id.to_string(), token.clone());
    if let Some(previous) = replaced {
        previous.cancel();
    }
    let session_id = session_id.to_string();
    let config = config.clone();
    let state = Arc::clone(state);
    tokio::spawn(async move {
        if tokio::select! {
            _ = tokio::time::sleep(WARM_AFTER) => false,
            _ = token.cancelled() => true,
        } {
            return; // replaced or session gone
        }
        fire(&config, &capture).await;
        state
            .warm_tokens
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&session_id);
    });
}

async fn fire(config: &LlmConfig, capture: &WarmCapture) {
    let mut call = http_call(
        config,
        crate::llm::anthropic::messages_url(&config.base_url),
        None,
        true,
    );
    call.warm_ping = true;
    // Dead-drop sink (same as compaction): with sink=None the stream printer
    // echoes deltas to the terminal — a warming reply must never surface.
    let (sink, rx) = tokio::sync::mpsc::channel(16);
    drop(rx);
    let _ = AnthropicMessages
        .stream(
            &call,
            &capture.messages,
            &capture.tools,
            Some(sink),
            &GlobalCancellation,
        )
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ApiProtocol;

    #[test]
    fn worth_warming_uses_catalog_pricing() {
        let _lock = crate::test_env::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("dex-warm-price-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("dex")).unwrap();
        std::fs::write(
            dir.join("dex/models.dev.json"),
            serde_json::json!({
                "opencode": {
                    "api": "https://opencode.ai/zen/v1",
                    "models": {
                        "m-r": {"cost": {"input": 1.0, "cache_read": 0.1}}
                    }
                }
            })
            .to_string(),
        )
        .unwrap();
        let prev = std::env::var_os("XDG_CACHE_HOME");
        let _guard = crate::test_env::EnvGuard(vec![("XDG_CACHE_HOME", prev)]);
        std::env::set_var("XDG_CACHE_HOME", &dir);
        let config = crate::llm::config::tests::test_cfg();
        // 100k cached tokens at $1/Mtok in, $0.1/Mtok read: $0.09 avoided.
        let capture = WarmCapture {
            cached_tokens: Some(100_000),
            ..Default::default()
        };
        assert!(
            capture.worth_warming(&config),
            "{:?}",
            capture.avoided_cost(&config)
        );
        // 10k cached tokens: $0.009 — below the floor.
        let small = WarmCapture {
            cached_tokens: Some(10_000),
            ..Default::default()
        };
        assert!(!small.worth_warming(&config));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unpriced_models_fall_back_to_a_token_floor() {
        let mut config = crate::llm::config::tests::test_cfg();
        config.model = "never-in-any-catalog".into();
        // No pricing entry: the 50k-token floor decides.
        let over = WarmCapture {
            cached_tokens: Some(60_000),
            ..Default::default()
        };
        assert!(over.worth_warming(&config));
        let under = WarmCapture {
            cached_tokens: Some(10_000),
            ..Default::default()
        };
        assert!(!under.worth_warming(&config));
    }

    #[tokio::test]
    async fn schedule_gates_on_wire_and_env() {
        // Non-Anthropic wire: never schedules (observable via the token map).
        let state = std::sync::Arc::new(DaemonState::new());
        let mut config = crate::llm::config::tests::test_cfg();
        config.api = ApiProtocol::ChatCompletions;
        let capture = WarmCapture {
            cached_tokens: Some(100_000),
            ..Default::default()
        };
        schedule(&state, "s1", &config, capture);
        assert!(state
            .warm_tokens
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty());
        // Anthropic wire schedules and registers its cancel token. Pinned,
        // so `effective_api` can't learn a different wire from the table.
        config.api = ApiProtocol::Anthropic;
        config.api_pinned = true;
        schedule(
            &state,
            "s2",
            &config,
            WarmCapture {
                messages: vec![ChatMessage::user("hi")],
                cached_tokens: Some(100_000),
                ..Default::default()
            },
        );
        assert_eq!(
            state
                .warm_tokens
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .len(),
            1
        );
        // A second schedule for the same session replaces the first.
        schedule(
            &state,
            "s2",
            &config,
            WarmCapture {
                messages: vec![ChatMessage::user("hi")],
                cached_tokens: Some(100_000),
                ..Default::default()
            },
        );
        let tokens = state.warm_tokens.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(tokens.len(), 1);
        let token = tokens.values().next().unwrap();
        // The replaced task was cancelled, the live one is not.
        assert!(!token.is_cancelled());
    }

    // `ApiProtocol` would otherwise be an unused import in non-test builds.
    #[allow(unused)]
    fn _api_protocol_marker(_: ApiProtocol) {}
}
