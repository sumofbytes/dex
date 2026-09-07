//! Stdio smoke: real transport against an embedded python fake server.
//! The fake interleaves an unsolicited notification before the
//! `tools/call` reply, pinning the client's id-matching (one stray line
//! must not desync later calls).
#[tokio::test]
async fn mcp_stdio_end_to_end() {
    const FAKE: &str = r#"
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    req = json.loads(line)
    mid, method = req.get("id"), req.get("method")
    if mid is None:
        continue  # notification: never reply
    if method == "initialize":
        resp = {"jsonrpc":"2.0","id":mid,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"fake","version":"0"}}}
    elif method == "tools/list":
        resp = {"jsonrpc":"2.0","id":mid,"result":{"tools":[{"name":"echo","description":"echo it","inputSchema":{"type":"object","properties":{"text":{"type":"string"}}}}]}}
    elif method == "tools/call":
        sys.stdout.write(json.dumps({"jsonrpc":"2.0","method":"notifications/progress","params":{}})+"\n"); sys.stdout.flush()
        txt = req["params"]["arguments"].get("text","")
        resp = {"jsonrpc":"2.0","id":mid,"result":{"content":[{"type":"text","text":"echo:"+txt}]}}
    elif method == "resources/list":
        resp = {"jsonrpc":"2.0","id":mid,"result":{"resources":[{"uri":"doc://a","name":"a"}]}}
    elif method == "resources/read":
        resp = {"jsonrpc":"2.0","id":mid,"result":{"contents":[{"uri":"doc://a","text":"hello-resource"}]}}
    else:
        resp = {"jsonrpc":"2.0","id":mid,"result":{}}
    sys.stdout.write(json.dumps(resp)+"\n"); sys.stdout.flush()
"#;
    let dir = std::env::temp_dir().join(format!("dex-mcp-smoke-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let py = dir.join("fake_mcp.py");
    std::fs::write(&py, FAKE).expect("write fake");
    // Drive the binary's own machinery via `dex run` (lazy connect path).
    let cfg = format!(
        r#"{{"fake":{{"command":"python3","args":["{}"]}}}}"#,
        py.display()
    );
    let out = tokio::process::Command::new(env!("CARGO_BIN_EXE_dex"))
        .env("DEX_MCP_SERVERS_JSON", cfg)
        .args(["run", "mcp__fake__echo", "text=hello"])
        .output()
        .await
        .expect("spawn dex");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    std::fs::remove_dir_all(&dir).ok();
    assert!(
        stdout.contains("echo:hello"),
        "stdout={stdout} stderr={stderr}"
    );
}
