use serde_json::{Map, Value};
use std::env;
use std::path::PathBuf;

use crate::core::types::PermissionMode;

#[derive(Clone)]
pub(crate) struct Args {
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub session_path: Option<PathBuf>,
    pub no_session: bool,
    pub new_session: bool,
    pub session_name: Option<String>,
    pub skill_dirs: Vec<PathBuf>,
    pub permission: Option<PermissionMode>,
    /// Extra HTTP headers for provider requests (`--header "X-Foo: bar"`,
    /// repeatable). Same `Name: Value` / `Name=Value` / JSON-object syntax as
    /// `DEX_HEADERS`.
    pub headers: Vec<String>,
    /// P10: attach to an existing daemon session (replay its event journal)
    /// instead of creating a fresh one. `dex connect <url> --reattach <id>`.
    pub reattach: Option<String>,
    pub rest: Vec<String>,
}

/// The mode in which the binary was invoked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    /// Start a headless HTTP server (`dex serve [host:port|port]`).
    Serve { bind: String },
    /// Start the TUI connected to a remote daemon (`dex connect <url>`).
    Connect { url: String },
    /// Start both server + TUI in the same process (default `dex`).
    Default,
    /// One-shot prompt (`dex "prompt"`).
    OneShot { prompt: String },
    /// Raw tool mode (`dex --tool`).
    Tool,
    /// One-shot tool execution (`dex run <tool> <args...>`) so scripts can
    /// call tools locally and stitch pipelines without model round trips.
    RunTool { name: String, args: Vec<String> },
    /// Refresh model catalog (`dex update --models`) — like `pi update --models`.
    Update { models: bool },
    /// Print usage (`dex --help`/`-h`) without touching config, network, or LLM.
    Help,
    /// Print version (`dex --version`/`-V`).
    Version,
    /// MCP OAuth (`dex mcp [status|login <server>|logout <server>]`).
    Mcp {
        /// Subcommand (`status` when omitted).
        action: String,
        /// Server name for `login`/`logout`.
        server: Option<String>,
    },
}

pub(crate) fn parse_args() -> Args {
    let mut base_url = None;
    let mut model = None;
    let mut session_path = None;
    let mut no_session = false;
    let mut new_session = false;
    let mut session_name = None;
    let mut skill_dirs = Vec::new();
    let mut permission = None;
    let mut headers = Vec::new();
    let mut reattach = None;
    let mut rest = Vec::new();
    let mut input = env::args().skip(1);
    while let Some(arg) = input.next() {
        // Attached forms (`--header=X: Y`, `-HX: Y`) for parity with curl.
        if let Some(value) = arg.strip_prefix("--header=") {
            headers.push(value.to_string());
            continue;
        }
        if let Some(value) = arg.strip_prefix("-H") {
            if !value.is_empty() {
                headers.push(value.to_string());
                continue;
            }
        }
        match arg.as_str() {
            "--base-url" => base_url = Some(required(&mut input, "--base-url")),
            "--model" => model = Some(required(&mut input, "--model")),
            "--reattach" => reattach = Some(required(&mut input, "--reattach")),
            "--session" | "-s" => {
                session_path = Some(PathBuf::from(required(&mut input, "--session")))
            }
            "--no-session" => no_session = true,
            "--new" | "-n" => new_session = true,
            "--name" => session_name = Some(required(&mut input, "--name")),
            "--permission" => {
                permission = Some(
                    PermissionMode::parse(&required(&mut input, "--permission"))
                        .unwrap_or_else(|error| fail(&error)),
                );
            }
            "--skill" => skill_dirs.push(PathBuf::from(required(&mut input, "--skill"))),
            "--header" | "-H" => headers.push(required(&mut input, "--header")),
            _ => rest.push(arg),
        }
    }
    Args {
        base_url,
        model,
        session_path,
        no_session,
        new_session,
        session_name,
        skill_dirs,
        permission,
        headers,
        reattach,
        rest,
    }
}

/// Determine the invocation mode from parsed args.
pub(crate) fn resolve_mode(args: &Args) -> Mode {
    // Help/version win before anything else so `--help` never falls through
    // to a OneShot LLM turn (previously `dex --help` burned a network call).
    match args.rest.first().map(|s| s.as_str()) {
        Some("-h" | "--help" | "help") => return Mode::Help,
        Some("-V" | "--version" | "version") => return Mode::Version,
        _ => {}
    }
    match args.rest.first().map(|s| s.as_str()) {
        Some("update") => {
            let models = args.rest.iter().any(|a| a == "--models" || a == "--all");
            Mode::Update { models }
        }
        Some("serve") => {
            let bind = args
                .rest
                .get(1)
                .cloned()
                .unwrap_or_else(|| "127.0.0.1:8420".to_string());
            Mode::Serve { bind }
        }
        Some("connect") => {
            let url = args
                .rest
                .get(1)
                .cloned()
                .unwrap_or_else(|| "http://127.0.0.1:8420".to_string());
            Mode::Connect { url }
        }
        Some("--tool") => Mode::Tool,
        Some("mcp") => Mode::Mcp {
            action: args
                .rest
                .get(1)
                .cloned()
                .unwrap_or_else(|| "status".to_string()),
            server: args.rest.get(2).cloned(),
        },
        Some("run") if args.rest.len() >= 2 => Mode::RunTool {
            name: args.rest[1].clone(),
            args: args.rest[2..].to_vec(),
        },
        Some(prompt) if !prompt.starts_with('-') => Mode::OneShot {
            prompt: args.rest.join(" "),
        },
        None => Mode::Default,
        Some(_unknown) => {
            // Treat unknown flags as part of the prompt for backwards compat.
            Mode::OneShot {
                prompt: args.rest.join(" "),
            }
        }
    }
}

/// Parse `run` arguments: either a single JSON object string
/// (`'{"path":"a.rs"}'`) or key=value pairs (`path=a.rs limit=5`).
/// Values that parse as JSON numbers/booleans are coerced, so `limit=5` and
/// `replaceAll=true` arrive typed; anything else stays a string (a string
/// value can always be forced by quoting it as JSON: `path='"2024"'`).
pub(crate) fn parse_tool_args(raw: &[String]) -> Result<Map<String, Value>, String> {
    let mut map = Map::new();
    if raw.len() == 1 && raw[0].trim_start().starts_with('{') {
        return serde_json::from_str::<Value>(&raw[0])
            .ok()
            .and_then(|value| value.as_object().cloned())
            .ok_or_else(|| "invalid JSON arguments".to_string());
    }
    for pair in raw {
        let (key, value) = pair.split_once('=').ok_or_else(|| {
            format!("argument '{pair}' must be key=value (or a single JSON object)")
        })?;
        let value = serde_json::from_str::<Value>(value)
            .unwrap_or_else(|_| Value::String(value.to_string()));
        map.insert(key.to_string(), value);
    }
    Ok(map)
}

fn required(input: &mut impl Iterator<Item = String>, flag: &str) -> String {
    input
        .next()
        .unwrap_or_else(|| fail(&format!("{} requires a value", flag)))
}

fn fail(message: &str) -> ! {
    eprintln!("error: {}", message);
    std::process::exit(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_args_parse_json_object_or_key_value_pairs() {
        let args = parse_tool_args(&["{\"path\":\"a.rs\",\"limit\":5}".to_string()]).unwrap();
        assert_eq!(args.get("path").and_then(Value::as_str), Some("a.rs"));
        assert_eq!(args.get("limit").and_then(Value::as_u64), Some(5));

        let args = parse_tool_args(&[
            "path=a.rs".to_string(),
            "limit=5".to_string(),
            "replaceAll=true".to_string(),
            "pattern=fn main".to_string(),
        ])
        .unwrap();
        assert_eq!(args.get("limit").and_then(Value::as_u64), Some(5));
        assert_eq!(args.get("replaceAll").and_then(Value::as_bool), Some(true));
        assert_eq!(args.get("pattern").and_then(Value::as_str), Some("fn main"));
        assert_eq!(args.get("path").and_then(Value::as_str), Some("a.rs"));

        assert!(parse_tool_args(&["path".to_string()]).is_err());
        assert!(parse_tool_args(&["{broken".to_string()]).is_err());
    }

    fn args_with_rest(rest: &[&str]) -> Args {
        Args {
            base_url: None,
            model: None,
            session_path: None,
            no_session: false,
            new_session: false,
            session_name: None,
            skill_dirs: Vec::new(),
            permission: None,
            headers: Vec::new(),
            reattach: None,
            rest: rest.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn help_and_version_win_over_oneshot() {
        for flag in ["-h", "--help", "help"] {
            assert_eq!(resolve_mode(&args_with_rest(&[flag])), Mode::Help);
        }
        for flag in ["-V", "--version", "version"] {
            assert_eq!(resolve_mode(&args_with_rest(&[flag])), Mode::Version);
        }
        // A prompt mentioning --help mid-sentence stays a prompt.
        assert!(matches!(
            resolve_mode(&args_with_rest(&["explain", "--help"])),
            Mode::OneShot { .. }
        ));
    }

    #[test]
    fn mcp_resolves_to_mcp_mode() {
        assert!(matches!(
            resolve_mode(&args_with_rest(&["mcp"])),
            Mode::Mcp { .. }
        ));
        match resolve_mode(&args_with_rest(&["mcp", "login", "github"])) {
            Mode::Mcp { action, server } => {
                assert_eq!(action, "login");
                assert_eq!(server.as_deref(), Some("github"));
            }
            other => panic!("expected Mode::Mcp, got {other:?}"),
        }
    }
}
