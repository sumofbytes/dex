use serde_json::json;
use std::env;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use tokio::sync::mpsc;

use crate::agent::state::CancellationSource;
use crate::core::console::with_console;
use crate::core::types::{ChatMessage, ChatRequest, SinkLine, StreamOptions};
use crate::llm::config::{insert_extra_header, LlmConfig};
use crate::llm::protocol::{responses_input, responses_tools, tools_schema};
use crate::llm::stream::{read_responses_stream, read_stream, Turn};

pub(crate) trait ModelClient: Clone + Send + Sync {
    async fn complete(
        &self,
        messages: &[ChatMessage],
        with_tools: bool,
        sink: Option<mpsc::Sender<SinkLine>>,
        cancel: &(dyn CancellationSource + Send + Sync),
    ) -> Result<Turn, Box<dyn std::error::Error + Send + Sync>>;
}

impl ModelClient for LlmConfig {
    async fn complete(
        &self,
        messages: &[ChatMessage],
        with_tools: bool,
        sink: Option<mpsc::Sender<SinkLine>>,
        cancel: &(dyn CancellationSource + Send + Sync),
    ) -> Result<Turn, Box<dyn std::error::Error + Send + Sync>> {
        crate::llm::streaming::complete(self, messages, with_tools, sink, cancel)
            .await
            .map_err(|e| {
                let msg = e.to_string();
                Box::<dyn std::error::Error + Send + Sync>::from(msg)
            })
    }
}

pub(crate) fn authenticated_request(
    request: reqwest::RequestBuilder,
    config: &LlmConfig,
) -> reqwest::RequestBuilder {
    let request =
        config
            .provider
            .auth_scheme()
            .apply(request, &config.api_key, config.account_id.as_deref());
    // Provider-scoped file headers apply first; the global/env/CLI extras
    // win on collision (explicit always beats file). `authorization` is
    // never overridable here — the api key owns it. Malformed names or
    // values are skipped so one bad header can't fail the turn.
    let mut merged = config.provider_headers.clone();
    for (name, value) in &config.extra_headers {
        insert_extra_header(&mut merged, name, value);
    }
    let mut request = request;
    for (name, value) in &merged {
        if name.eq_ignore_ascii_case("authorization") {
            continue;
        }
        if let (Ok(name), Ok(value)) = (
            reqwest::header::HeaderName::from_bytes(name.as_bytes()),
            reqwest::header::HeaderValue::from_str(value),
        ) {
            request = request.header(name, value);
        }
    }
    request
}

pub(crate) fn retryable_status(status: reqwest::StatusCode) -> bool {
    status.as_u16() == 408 || status.as_u16() == 429 || status.is_server_error()
}

pub(crate) fn provider_log(event: &str, detail: &str) {
    let Some(base) = env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
    else {
        return;
    };
    let path = base.join("dex/provider.jsonl");
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) {
        let record =
            json!({"timestamp": chrono::Utc::now().to_rfc3339(), "event": event, "detail": detail});
        // Single write syscall so concurrent turns cannot interleave records.
        let mut line = record.to_string();
        line.push('\n');
        let _ = file.write_all(line.as_bytes());
    }
}
/// Send a provider request with shared retry/backoff, 401 credential refresh
/// (OpenAI Codex), and provider logging. The protocol-specific request body
/// and post-success reader are supplied by the caller.
async fn post_with_retry(
    config: &LlmConfig,
    url: &str,
    body: &impl serde::Serialize,
    sink: Option<&mpsc::Sender<SinkLine>>,
) -> Result<reqwest::Response, Box<dyn std::error::Error + Send + Sync>> {
    const MAX_RETRIES: u32 = 3;
    let mut active_config = config.clone();
    for attempt in 0..=MAX_RETRIES {
        let request = config.client.post(url);
        let resp = match authenticated_request(request, &active_config)
            .json(body)
            .send()
            .await
        {
            Ok(resp) => resp,
            Err(e) if attempt < MAX_RETRIES => {
                let delay = Duration::from_millis(500 * 2u64.pow(attempt));
                with_console(sink.is_some(), || {
                    eprintln!("[llm] request failed: {}; retrying in {:?}", e, delay)
                });
                tokio::time::sleep(delay).await;
                continue;
            }
            Err(e) => {
                provider_log("request_failed", &e.to_string());
                return Err(Box::new(e));
            }
        };

        if !resp.status().is_success() {
            let status = resp.status();
            let body_text = resp.text().await.map_err(Box::new)?;
            if status == reqwest::StatusCode::UNAUTHORIZED
                && active_config.provider.credentials_refreshable()
                && attempt < MAX_RETRIES
            {
                if let Ok((token, account)) = crate::llm::config::resolve_credentials(
                    &active_config.provider,
                    &active_config.provider_entries,
                ) {
                    active_config.api_key = token;
                    active_config.account_id = account;
                    continue;
                }
            }
            let retryable = retryable_status(status);
            if retryable && attempt < MAX_RETRIES {
                let delay = Duration::from_millis(500 * 2u64.pow(attempt));
                with_console(sink.is_some(), || {
                    eprintln!("[llm] API error {}: retrying in {:?}", status, delay)
                });
                tokio::time::sleep(delay).await;
                continue;
            }
            provider_log("api_error", &format!("{}: {}", status, body_text));
            // Opaque 5xx / missing route from `/responses` usually means the
            // model only speaks chat-completions (proven for e.g.
            // glm-5.3-flash on zen/go) — point at the per-model override
            // instead of a bare body. Auth and rate-limit failures say
            // nothing about the protocol, and a pinned `api:` means the user
            // already decided.
            if url.ends_with("/responses")
                && !config.api_pinned
                && (status == reqwest::StatusCode::NOT_FOUND || status.is_server_error())
            {
                return Err(format!(
                    "API error: {} (hint: {} may speak openai-completions; set DEX_MODEL_APIS={}=openai-completions)",
                    body_text, config.model, config.model
                )
                .into());
            }
            return Err(format!("API error: {}", body_text).into());
        }

        return Ok(resp);
    }

    unreachable!()
}

pub(crate) async fn call_chat_completions(
    config: &LlmConfig,
    messages: &[ChatMessage],
    with_tools: bool,
    sink: Option<mpsc::Sender<SinkLine>>,
    cancel: &(dyn CancellationSource + Send + Sync),
) -> Result<Turn, Box<dyn std::error::Error + Send + Sync>> {
    let req = ChatRequest {
        model: &config.model,
        messages,
        tools: if with_tools {
            tools_schema()
        } else {
            Vec::new()
        },
        stream: true,
        stream_options: StreamOptions {
            include_usage: true,
        },
        reasoning_effort: &config.thinking_effort,
    };
    let resp = post_with_retry(
        config,
        &format!("{}/chat/completions", config.base_url),
        &req,
        sink.as_ref(),
    )
    .await?;
    read_stream(resp, sink, cancel).await
}

pub(crate) async fn call_responses(
    config: &LlmConfig,
    messages: &[ChatMessage],
    with_tools: bool,
    sink: Option<mpsc::Sender<SinkLine>>,
    cancel: &(dyn CancellationSource + Send + Sync),
) -> Result<Turn, Box<dyn std::error::Error + Send + Sync>> {
    let (instructions, input) = responses_input(messages);
    let mut body = json!({
        "model": config.model,
        "input": input,
        "stream": true,
        "store": false,
        // State is never stored server-side, so ask for the encrypted
        // reasoning blobs — without them reasoning can't be replayed and
        // the model re-reasons from scratch on every tool call.
        "include": ["reasoning.encrypted_content"],
    });
    if let Some(instructions) = instructions {
        body["instructions"] = json!(instructions);
    }
    if with_tools {
        body["tools"] = json!(responses_tools());
    }
    if let Some(effort) = &config.thinking_effort {
        body["reasoning"] = json!({ "effort": effort, "summary": "auto" });
    }
    let resp = post_with_retry(
        config,
        &format!("{}/responses", config.base_url),
        &body,
        sink.as_ref(),
    )
    .await?;
    // Mid-stream failures (output already flowed) are marked by the stream
    // driver itself, so the protocol-fallback gate won't re-run a turn whose
    // partial text is already on the transcript — while a drop before the
    // first delta stays retryable.
    read_responses_stream(resp, sink, cancel).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::Usage;

    #[derive(Clone)]
    struct MockModel;

    impl ModelClient for MockModel {
        async fn complete(
            &self,
            _messages: &[ChatMessage],
            _with_tools: bool,
            _sink: Option<mpsc::Sender<SinkLine>>,
            _cancel: &(dyn CancellationSource + Send + Sync),
        ) -> Result<Turn, Box<dyn std::error::Error + Send + Sync>> {
            Ok(Turn {
                message: ChatMessage::assistant("mock response"),
                usage: Some(Usage {
                    prompt_tokens: 3,
                    completion_tokens: 0,
                    cached_tokens: None,
                }),
                stop_reason: None,
            })
        }
    }

    #[test]
    fn authenticated_request_applies_custom_headers_and_protects_auth() {
        let mut config = crate::llm::config::tests::test_cfg();
        config.api_key = "secret".to_string();
        config
            .extra_headers
            .insert("X-Gateway-Key".to_string(), "abc".to_string());
        config
            .extra_headers
            .insert("Authorization".to_string(), "hacked".to_string());
        config
            .extra_headers
            .insert("not a header".to_string(), "bad".to_string());
        let req = authenticated_request(
            config.client.get("http://localhost/v1/chat/completions"),
            &config,
        )
        .build()
        .unwrap();
        assert_eq!(
            req.headers()
                .get("x-gateway-key")
                .map(|v| v.to_str().unwrap()),
            Some("abc")
        );
        // The api key owns `authorization`; a custom header can't hijack it.
        assert_eq!(
            req.headers()
                .get("authorization")
                .map(|v| v.to_str().unwrap()),
            Some("Bearer secret")
        );
        assert!(!req.headers().contains_key("not a header"));
    }

    #[test]
    fn provider_headers_apply_under_global_extras() {
        // Provider-scoped file headers ride along; an explicit global /
        // env / CLI extra wins on collision.
        let mut config = crate::llm::config::tests::test_cfg();
        config.api_key = "secret".to_string();
        config
            .provider_headers
            .insert("X-Prov".to_string(), "prov".to_string());
        config
            .provider_headers
            .insert("X-Both".to_string(), "prov".to_string());
        config
            .extra_headers
            .insert("X-Both".to_string(), "global".to_string());
        let req = authenticated_request(
            config.client.get("http://localhost/v1/chat/completions"),
            &config,
        )
        .build()
        .unwrap();
        assert_eq!(
            req.headers().get("x-prov").map(|v| v.to_str().unwrap()),
            Some("prov")
        );
        assert_eq!(
            req.headers().get("x-both").map(|v| v.to_str().unwrap()),
            Some("global")
        );
    }

    #[tokio::test]
    async fn model_boundary_supports_deterministic_mock() {
        let turn = MockModel
            .complete(&[], false, None, &crate::agent::state::GlobalCancellation)
            .await
            .unwrap();
        assert_eq!(turn.message.content.as_deref(), Some("mock response"));
        assert_eq!(
            turn.usage,
            Some(Usage {
                prompt_tokens: 3,
                completion_tokens: 0,
                cached_tokens: None
            })
        );
    }
}
