//! Prompt-cache warming: keep the provider's prompt cache alive across idle
//! gaps so the next turn re-reads the context at cache-hit price instead of
//! full input price.
//!
//! Anthropic-native turns mark their prefix with `cache_control`; the cache
//! expires after ~5 minutes idle. After a completed turn, [`schedule`]
//! re-sends the exact last request (same messages, tools, cache marks) once
//! at TTL minus margin with a tiny output cap. The reply is dropped —
//! it never enters the session, journal, or usage totals. A newer schedule
//! for the same session cancels the pending one (and the real next turn
//! refreshes the cache anyway, so a skipped ping is always safe).
//!
//! Thinking: history that replays signed thinking blocks keeps the
//! session's effort on the ping (the API rejects thinking blocks without
//! the parameter) — see [`warm_effort_and_cap`].

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

/// The ping's `(thinking effort, output cap)` for `messages_body`. Plain
/// history takes the tiny dead-reply cap with thinking off. History that
/// replays signed thinking blocks must keep thinking on — the API rejects
/// thinking blocks without the parameter — so those pings mirror the
/// session's effort with a budget-plus-headroom cap (mirroring the real
/// request's relationship); with effort off, mirror that too and take the
/// catalog limit.
pub(crate) fn warm_effort_and_cap<'a>(
    effort: Option<&'a str>,
    messages: &[ChatMessage],
) -> (Option<&'a str>, Option<u64>) {
    if !dex_ai::anthropic::replays_thinking(messages) {
        return (None, Some(WARM_MAX_TOKENS));
    }
    match effort.map(dex_ai::anthropic::thinking_budget) {
        Some(budget) => (effort, Some(budget + dex_ai::anthropic::THINKING_HEADROOM)),
        None => (None, None),
    }
}

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
        // Remove only when this is still the registered token: a
        // replacement racing the timer must keep its own entry live so the
        // next schedule can still cancel it.
        let mut map = state.warm_tokens.lock().unwrap_or_else(|e| e.into_inner());
        if map.get(&session_id).is_some_and(|t| t.same_token(&token)) {
            map.remove(&session_id);
        }
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
    // Console retry notices are sink-gated too, so the outcome is only
    // visible at `DEX_LOG=debug` — the feature's value rests on unseen
    // provider behavior, so keep at least that much observability.
    let (sink, rx) = tokio::sync::mpsc::channel(16);
    drop(rx);
    match AnthropicMessages
        .stream(
            &call,
            &capture.messages,
            &capture.tools,
            Some(sink),
            &GlobalCancellation,
        )
        .await
    {
        Ok(turn) => dex_runtime::log!(
            Debug,
            "cache warm ok: {} cached tokens",
            turn.usage.and_then(|u| u.cached_tokens).unwrap_or_default()
        ),
        Err(e) => dex_runtime::log!(Debug, "cache warm failed: {e}"),
    }
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
        let _lock = crate::test_env::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // Hermetic knob + catalog: a developer's DEX_CACHE_WARMING=0, real
        // config file, or cheaply-priced real model entry must not flip
        // these assertions (empty cache dir → unpriced → 50k-token floor).
        let dir = std::env::temp_dir().join(format!("dex-warm-sched-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("dex")).unwrap();
        let _guard = crate::test_env::EnvGuard(vec![
            ("DEX_CACHE_WARMING", std::env::var_os("DEX_CACHE_WARMING")),
            ("DEX_CONFIG", std::env::var_os("DEX_CONFIG")),
            ("XDG_CONFIG_HOME", std::env::var_os("XDG_CONFIG_HOME")),
            ("XDG_CACHE_HOME", std::env::var_os("XDG_CACHE_HOME")),
        ]);
        std::env::set_var("XDG_CONFIG_HOME", &dir);
        std::env::set_var("XDG_CACHE_HOME", &dir);
        std::env::remove_var("DEX_CONFIG");
        std::env::remove_var("DEX_CACHE_WARMING");
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

    #[test]
    fn warming_keeps_thinking_only_when_history_replays_blocks() {
        // Plain history: tiny dead-reply cap, thinking off regardless of
        // the session's effort.
        let plain = vec![ChatMessage::user("hi")];
        assert_eq!(
            warm_effort_and_cap(Some("low"), &plain),
            (None, Some(WARM_MAX_TOKENS))
        );
        // Signed thinking blocks in the history: mirror the session's
        // effort with a budget-plus-headroom cap.
        let mut thinking = ChatMessage::assistant("reply");
        thinking.reasoning_items = Some(vec![serde_json::json!(
            {"type": "thinking", "thinking": "hmm", "signature": "sig"}
        )]);
        let history = vec![thinking];
        assert_eq!(
            warm_effort_and_cap(Some("low"), &history),
            (
                Some("low"),
                Some(4096 + dex_ai::anthropic::THINKING_HEADROOM)
            )
        );
        // Effort off but blocks replayed: mirror that (no cap, no thinking).
        assert_eq!(warm_effort_and_cap(None, &history), (None, None));
    }
}
