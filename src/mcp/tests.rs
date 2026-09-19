//! MCP client tests.

use super::client::McpClient;
use super::config::{sanitize_server_name, split_mcp_name, McpServerConfig};
use super::manager::McpManager;
use super::mapping::{content_to_text, McpTool};
use super::transport::McpTransport;
use super::*;

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use serde_json::Value;

struct FakeTransport {
    tools: Vec<McpTool>,
    resources: bool,
    fail: bool,
}

#[cfg(test)]
impl McpTransport for FakeTransport {
    fn request<'a>(
        &'a self,
        method: &'a str,
        _params: Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value, String>> + Send + 'a>>
    {
        Box::pin(async move {
            if self.fail {
                return Err("boom".to_string());
            }
            match method {
                "tools/list" => Ok(
                    serde_json::json!({"tools": self.tools.iter().map(|t| serde_json::json!({"name": t.name, "description": t.description, "inputSchema": t.input_schema})).collect::<Vec<_>>() }),
                ),
                "tools/call" => {
                    Ok(serde_json::json!({"content": [{"type": "text", "text": "fake-ok"}]}))
                }
                "resources/list" => {
                    if self.resources {
                        Ok(serde_json::json!({"resources": [{"uri": "doc://a"}]}))
                    } else {
                        Ok(serde_json::json!({}))
                    }
                }
                "resources/read" => {
                    Ok(serde_json::json!({"contents": [{"uri": "doc://a", "text": "hello"}]}))
                }
                _ => Ok(serde_json::json!({})),
            }
        })
    }
}

#[test]
fn sse_stream_decode_returns_first_result_with_early_exit() {
    // §20: notifications and blanks scan past; a result split across
    // chunks still matches once its newline arrives.
    let mut buf = Vec::new();
    let scan = |buf: &mut Vec<u8>, chunk: &[u8]| {
        buf.extend_from_slice(chunk);
        sse_scan_buffered_id(buf, 1)
    };
    assert!(scan(&mut buf, b": keep-alive\n\n").is_none());
    assert!(scan(
        &mut buf,
        b"data: {\"jsonrpc\":\"2.0\",\"method\":\"progress\"}\n"
    )
    .is_none());
    assert!(scan(&mut buf, b"data: {\"jsonrpc\":\"2.0\",\"id\":1,\"res").is_none());
    let r = scan(&mut buf, b"ult\":{\"ok\":true}}\n").expect("split result matches");
    assert_eq!(r, serde_json::json!({"ok": true}));
    // Error envelopes match too; [DONE] and blanks don't.
    assert!(sse_result_id("data: [DONE]", 1).is_none());
    assert!(sse_result_id("", 1).is_none());
    assert!(sse_result_id(": comment", 1).is_none());
    let e = sse_result_id("data: {\"id\":1,\"error\":{\"code\":-1}}", 1).expect("error matches");
    assert!(e.get("__mcp_error").is_some());
    // Id-less envelopes are never this call's result: under concurrent
    // calls on one transport an id-less broadcast carrying `result`
    // would otherwise be stolen and misattributed to whoever scans first.
    assert!(sse_result_id("data: {\"error\":{\"code\":-1}}", 1).is_none());
    assert!(sse_result_id("data: {\"result\":{\"ok\":true}}", 1).is_none());
}

fn fake_client(tools: Vec<McpTool>, resources: bool, fail: bool) -> McpClient {
    McpClient::new(
        Box::new(FakeTransport {
            tools,
            resources,
            fail,
        }),
        5,
    )
}

static NO_CANCEL: crate::agent::state::GlobalCancellation = crate::agent::state::GlobalCancellation;

fn tool(name: &str) -> McpTool {
    McpTool {
        name: name.to_string(),
        description: "d".to_string(),
        input_schema: serde_json::json!({"type": "object"}),
    }
}

#[test]
fn tool_names_roundtrip() {
    assert_eq!(mcp_tool_name("GitHub", "my-tool"), "mcp__github__my-tool");
    assert_eq!(
        split_mcp_name("mcp__github__search"),
        Some(("github".to_string(), "search".to_string()))
    );
    assert_eq!(
        split_mcp_name("mcp__gh_read_resource"),
        Some(("gh".to_string(), "\0resource".to_string()))
    );
    assert!(split_mcp_name("read").is_none());
    assert!(split_mcp_name("mcp__a__b__c").is_none());
}

#[test]
fn server_names_are_sanitized() {
    assert_eq!(sanitize_server_name("GitHub-Prod"), "github_prod");
    assert_eq!(sanitize_server_name("a b.c"), "a_b_c");
}

#[test]
fn env_vars_expand() {
    unsafe { std::env::set_var("DEX_MCP_TEST_X", "hello") };
    assert_eq!(expand_env("$DEX_MCP_TEST_X/world").unwrap(), "hello/world");
    assert_eq!(expand_env("${DEX_MCP_TEST_X}!").unwrap(), "hello!");
    // Fail-closed: a missing variable is an error, never silent `""`
    // (which would turn a missing key into an unauthenticated request).
    assert!(expand_env("$DEX_MCP_TEST_MISSING!").is_err());
}

#[test]
fn env_vars_expand_non_ascii() {
    unsafe { std::env::set_var("DEX_MCP_TEST_UNICODE", "héllo") };
    // Multi-byte chars adjacent to an expansion must pass through
    // unchanged; `bytes[i] as char` used to mojibake `é` into `Ã©`.
    assert_eq!(
        expand_env("café $DEX_MCP_TEST_UNICODE bar").unwrap(),
        "café héllo bar"
    );
    assert_eq!(
        expand_env("café${DEX_MCP_TEST_UNICODE}!").unwrap(),
        "caféhéllo!"
    );
    // Fail-closed missing-var error still fires next to multi-byte text.
    assert_eq!(
        expand_env("café $DEX_MCP_TEST_MISSING bar").unwrap_err(),
        "mcp config: env var $DEX_MCP_TEST_MISSING is not set"
    );
}

#[test]
fn config_parses_stdio_and_http() {
    unsafe { std::env::set_var("DEX_MCP_TEST_X", "hello") };
    let yaml: serde_yaml::Value = serde_yaml::from_str(
        r#"
mcp_servers:
  gh:
    command: npx
    args: ["-y", "server"]
    env: {TOK: $DEX_MCP_TEST_X}
    allow: [search]
    deny: [exec]
  short: "uvx server --foo"
  web:
    url: https://example.com/mcp
    headers: {Authorization: Bearer x}
    timeout_secs: 5
  off:
    command: foo
    disabled: true
  empty: {}
"#,
    )
    .unwrap();
    let map = parse_mcp_servers(&yaml);
    assert!(map.contains_key("gh"));
    assert_eq!(map["gh"].args, vec!["-y", "server"]);
    assert_eq!(map["gh"].env.get("TOK").map(String::as_str), Some("hello"));
    assert_eq!(map["gh"].allow, vec!["search"]);
    assert_eq!(map["gh"].deny, vec!["exec"]);
    assert!(map["gh"].tool_allowed("search"));
    assert!(!map["gh"].tool_allowed("exec"));
    assert!(!map["gh"].tool_allowed("other"));
    assert_eq!(map["short"].command.as_deref(), Some("uvx"));
    assert_eq!(map["short"].args, vec!["server", "--foo"]);
    assert!(map.contains_key("web"));
    assert!(map["web"].is_http());
    assert_eq!(map["web"].timeout_secs, 5);
    assert!(!map.contains_key("off"));
    assert!(!map.contains_key("empty"));
}

#[test]
fn bad_entries_are_skipped_not_fatal() {
    unsafe { std::env::remove_var("DEX_MCP_TEST_MISSING_2") };
    let yaml: serde_yaml::Value = serde_yaml::from_str(
        r#"
mcp_servers:
  broken:
    command: foo
    env: {TOK: $DEX_MCP_TEST_MISSING_2}
  also_broken: 42
  good:
    command: bar
"#,
    )
    .unwrap();
    let map = parse_mcp_servers(&yaml);
    assert!(!map.contains_key("broken"));
    assert!(!map.contains_key("also_broken"));
    assert!(map.contains_key("good"));
}

#[test]
fn kill_switch_disables_mcp() {
    let _env = TEST_ENV_LOCK.blocking_lock();
    let prev = std::env::var("DEX_MCP").ok();
    unsafe { std::env::set_var("DEX_MCP", "0") };
    assert!(!mcp_enabled());
    assert!(load_server_configs().is_empty());
    unsafe {
        match prev {
            Some(v) => std::env::set_var("DEX_MCP", v),
            None => std::env::remove_var("DEX_MCP"),
        }
    }
    assert!(mcp_enabled());
}

#[test]
fn secrets_are_redacted() {
    let err = "mcp http 401: tools/call\nAuthorization: Bearer abc123\nx-api-key=zzz";
    let out = redact_secrets(err);
    assert!(!out.contains("abc123"), "{out}");
    assert!(!out.contains("zzz"), "{out}");
    assert!(out.contains("Authorization: [redacted]"), "{out}");
    assert!(out.contains("x-api-key=[redacted]"), "{out}");
    assert_eq!(redact_secrets("plain boom"), "plain boom");
}

#[test]
fn redaction_is_non_ascii_safe() {
    // Multi-byte chars around the marker survive redaction untouched.
    let out = redact_line("café: authorization: Bearer sk-café123 rest");
    assert_eq!(out, "café: authorization: [redacted]");
    // `to_lowercase` expands some chars (`İ` -> i + U+0307) and desynced
    // the byte offsets applied to `line`; ASCII markers must stay aligned.
    let out = redact_line("İ: authorization: Bearer sk-secret");
    assert_eq!(out, "İ: authorization: [redacted]");
}

#[test]
fn tool_names_are_clamped() {
    assert!(mcp_tool_name("srv", &"x".repeat(500)).len() <= MCP_TOOL_NAME_LIMIT);
}

#[test]
fn allow_deny_gate_tools() {
    let cfg = McpServerConfig {
        deny: vec!["rm".to_string()],
        ..Default::default()
    };
    assert!(!cfg.tool_allowed("rm"));
    assert!(cfg.tool_allowed("ls"));
    // Deny wins over allow.
    let cfg = McpServerConfig {
        allow: vec!["ls".to_string()],
        deny: vec!["ls".to_string()],
        ..Default::default()
    };
    assert!(!cfg.tool_allowed("ls"));
    assert!(!cfg.tool_allowed("other"));
}

#[test]
fn tool_definition_clamps_and_fixes_schema() {
    let t = McpTool {
        name: "search".to_string(),
        description: "x".repeat(2000),
        input_schema: Value::Null,
    };
    let def = t.to_definition("gh");
    assert_eq!(def.function.name, "mcp__gh__search");
    assert!(def.function.description.len() <= MCP_DESC_LIMIT + 3);
    assert!(def.function.parameters.is_object());
}

#[test]
fn content_mapping_covers_shapes() {
    let v = serde_json::json!({"content": [
        {"type": "text", "text": "hi"},
        {"type": "image", "mimeType": "image/png"},
        {"type": "resource", "resource": {"uri": "f://a", "text": "body"}},
    ]});
    let text = content_to_text(&v);
    assert!(text.contains("hi"));
    assert!(text.contains("omitted"));
    assert!(text.contains("body"));
    let err = serde_json::json!({"content": [{"type": "text", "text": "nope"}], "isError": true});
    assert!(content_to_text(&err).starts_with("Error:"));
}

#[tokio::test]
async fn manager_lists_calls_and_isolates_failures() {
    let mgr = Arc::new(McpManager::new(BTreeMap::new()));
    mgr.insert_client(
        "good",
        fake_client(
            vec![McpTool {
                name: "search".to_string(),
                description: "s".to_string(),
                input_schema: serde_json::json!({"type": "object"}),
            }],
            true,
            false,
        ),
    )
    .await;
    mgr.insert_client("bad", fake_client(vec![], false, true))
        .await;
    mgr.rebuild_cache().await;
    let defs = mgr.tool_definitions().await;
    // 1 real tool + 1 synthetic resource reader from `good`; `bad` adds none.
    assert_eq!(defs.len(), 2);
    let mut args = serde_json::Map::new();
    let out = mgr
        .call_tool("mcp__good__search", &args, &NO_CANCEL)
        .await
        .unwrap();
    assert_eq!(out, "fake-ok");
    args.insert("uri".to_string(), Value::String("doc://a".to_string()));
    let res = mgr
        .call_tool("mcp__good_read_resource", &args, &NO_CANCEL)
        .await
        .unwrap();
    assert!(res.contains("hello"));
    // Unknown server reports down, never panics.
    assert!(mgr
        .call_tool("mcp__nope__x", &args, &NO_CANCEL)
        .await
        .is_err());
}

#[tokio::test]
async fn name_collisions_rename_instead_of_shadow() {
    let mgr = Arc::new(McpManager::new(BTreeMap::new()));
    for srv in ["one", "two"] {
        mgr.insert_client(srv, fake_client(vec![tool("same")], false, false))
            .await;
    }
    mgr.rebuild_cache().await;
    let defs = mgr.tool_definitions().await;
    assert_eq!(defs.len(), 2);
    let names: Vec<String> = defs.iter().map(|d| d.function.name.clone()).collect();
    // Both survive; the loser keeps the `mcp__` prefix with a `~2`
    // suffix and still dispatches through the cache.
    assert_eq!(names.iter().filter(|n| *n == "mcp__one__same").count(), 1);
    assert_eq!(names.iter().filter(|n| *n == "mcp__two__same").count(), 1);
    let args = serde_json::Map::new();
    for n in &names {
        assert_eq!(
            mgr.call_tool(n, &args, &NO_CANCEL).await.unwrap(),
            "fake-ok"
        );
    }
}

#[tokio::test]
async fn allow_deny_filter_schema() {
    let mut configs = BTreeMap::new();
    configs.insert(
        "s".to_string(),
        McpServerConfig {
            deny: vec!["nope".to_string()],
            ..Default::default()
        },
    );
    let mgr = Arc::new(McpManager::new(configs));
    mgr.insert_client(
        "s",
        fake_client(vec![tool("ok"), tool("nope")], false, false),
    )
    .await;
    mgr.rebuild_cache().await;
    let defs = mgr.tool_definitions().await;
    assert_eq!(defs.len(), 1);
    assert_eq!(defs[0].function.name, "mcp__s__ok");
    let args = serde_json::Map::new();
    assert!(mgr
        .call_tool("mcp__s__nope", &args, &NO_CANCEL)
        .await
        .is_err());
}

#[tokio::test]
async fn schema_cap_drops_and_counts() {
    // Direct `enforce_cap` with an explicit max: no env mutation, so no
    // cross-test race on the shared env table (see TEST_ENV_LOCK).
    let mgr = McpManager::new(BTreeMap::new());
    let mut tools = vec![tool("b").to_definition("s"), tool("a").to_definition("s")];
    let mut names = HashMap::new();
    names.insert("mcp__s__a".to_string(), ("s".to_string(), "a".to_string()));
    names.insert("mcp__s__b".to_string(), ("s".to_string(), "b".to_string()));
    mgr.enforce_cap(&mut tools, &mut names, 1).await;
    // Sorted by name, head kept.
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].function.name, "mcp__s__a");
    assert_eq!(*mgr.cached_truncated.read().await, 1);
}

#[tokio::test]
async fn reconnect_smoke_unknown_server_errors() {
    // No transport involved: an unknown name fails before any spawn.
    let mgr = McpManager::new(BTreeMap::new());
    let err = mgr.reconnect("nope").await.unwrap_err();
    assert!(err.contains("unknown mcp server"), "{err}");
}

#[tokio::test]
async fn statuses_carry_down_reason() {
    let mut configs = BTreeMap::new();
    configs.insert(
        "bad".to_string(),
        McpServerConfig {
            command: Some("dex-definitely-missing-binary".to_string()),
            timeout_secs: 5,
            ..Default::default()
        },
    );
    let mgr = McpManager::new(configs);
    assert!(mgr.reconnect("bad").await.is_err());
    assert!(mgr.reconnect("ghost").await.is_err());
    let st = mgr.statuses().await;
    assert_eq!(st.len(), 1);
    assert_eq!(st[0].state, "down");
    assert!(
        st[0].error.as_deref().unwrap_or("").contains("spawn"),
        "{:?}",
        st[0].error
    );
}

#[tokio::test]
async fn sweep_drops_dead_clients() {
    let mgr = Arc::new(McpManager::new(BTreeMap::new()));
    mgr.insert_client("dead", fake_client(vec![], false, true))
        .await;
    mgr.sweep_once().await;
    assert!(!mgr.clients.read().await.contains_key("dead"));
    assert!(mgr.down.read().await.contains_key("dead"));
}

#[tokio::test]
async fn statuses_report_down_without_clients() {
    let mut configs = BTreeMap::new();
    configs.insert("a".to_string(), McpServerConfig::default());
    let mgr = McpManager::new(configs);
    let st = mgr.statuses().await;
    assert_eq!(st.len(), 1);
    assert_eq!(st[0].state, "down");
}

#[test]
fn mcp_status_line_empty_when_no_servers() {
    assert_eq!(super::status_line(&[]), None);
}

#[test]
fn mcp_status_line_names_up_and_down_servers() {
    let line = super::status_line(&[
        ServerStatus {
            name: "gh".to_string(),
            state: "up".to_string(),
            tools: 3,
            error: None,
        },
        ServerStatus {
            name: "db".to_string(),
            state: "down".to_string(),
            tools: 0,
            error: Some("refused".to_string()),
        },
    ])
    .expect("non-empty statuses produce a line");
    assert!(line.contains("gh (3 tools)"), "{line}");
    assert!(line.contains("db (down)"), "{line}");
}
