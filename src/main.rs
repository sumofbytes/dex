mod agent;
mod cli;
mod client;
mod core;
mod daemon;
mod extensions;
mod llm;
mod mcp;
mod protocol;
mod session;
mod skills;
mod tools;
mod ui;
mod update;
mod usage;

use crate::cli::{Args, Mode};
use crate::session::{load_llm_messages_from_session, Session};
use crate::tools::execute_sync as execute;

use crate::agent::r#loop::{process_turn, AgentRuntime};
use crate::agent::state::{GlobalCancellation, ToolState};
use crate::core::console::install_sigint_handler;
use crate::core::types::ChatMessage;
use crate::llm::config::LlmConfig;
use crate::llm::prompt::system_prompt_with_override;
use crate::skills::{discover_skills, skill_dirs};

use serde_json::{json, Map, Value};
use std::env;
use std::io::{self, Write};

/// Cost below this is noise on the one-shot summary line; also guards
/// float equality when pricing data is missing (cost rounds to 0.0).
const COST_SUMMARY_MIN_USD: f64 = 5e-4;

/// One-shot stderr spend summary (`None` when nothing was reported): total
/// prompt/output tokens, plus USD once it clears [`COST_SUMMARY_MIN_USD`].
/// Output-only usage still summarizes — some providers omit prompt tokens.
fn spend_summary(usage: u64, output: u64, cost: f64) -> Option<String> {
    if usage == 0 && output == 0 {
        return None;
    }
    let mut summary = format!("[dex] {usage} prompt / {output} output tokens");
    if cost > COST_SUMMARY_MIN_USD {
        summary.push_str(&format!(" · ${cost:.3}"));
    }
    Some(summary)
}

/// Per-request overrides built from CLI flags — shared by the TUI's remote
/// client and the `dex connect <url> "prompt"` one-shot, so daemon-backed
/// runs keep flag parity with in-process mode.
pub(crate) fn chat_options_from_args(args: &Args) -> client::http::ChatOptions {
    client::http::ChatOptions {
        skill_dirs: args
            .skill_dirs
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect(),
        base_url: args.base_url.clone(),
        model: args.model.clone(),
        permission: args.permission.map(|mode| mode.as_str().to_string()),
        headers: if args.headers.is_empty() {
            None
        } else {
            let mut merged = std::collections::BTreeMap::new();
            for raw in &args.headers {
                for (k, v) in crate::llm::config::parse_headers_str(raw) {
                    crate::llm::config::insert_extra_header(&mut merged, &k, &v);
                }
            }
            Some(merged)
        },
        plan: None,
        system_prompt: cli_system_prompt(args).map(|(text, _)| text),
        thinking_effort: None,
        idempotency_key: None,
    }
}

/// Resolve `--system-prompt` / `--system-prompt-file` for client-side
/// forwarding. The file is read here so remote daemons work; a miss fails
/// fast instead of silently running the default. Returns the text plus the
/// flag it came from so `doctor` names the right origin.
fn cli_system_prompt(args: &Args) -> Option<(String, &'static str)> {
    match crate::llm::config::resolve_cli_system_prompt(
        args.system_prompt.clone(),
        args.system_prompt_file.clone(),
    ) {
        Ok(text) => text,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}

/// Shared session-open ladder: `--new` creates a fresh session, otherwise
/// open/continue `--session` (or the newest one). On failure, warn to
/// stderr and return `None` so the caller continues without persistence;
/// `--name` renaming stays with the callers that do it.
fn open_session(args: &Args, cwd: &str) -> Option<Session> {
    let opened = if args.new_session {
        Session::new(cwd.to_string(), args.session_name.clone())
    } else {
        Session::open_or_continue(cwd.to_string(), args.session_path.as_deref(), false)
    };
    match opened {
        Ok(session) => Some(session),
        Err(error) => {
            eprintln!(
                "[session] could not open session ({}); continuing without persistence",
                error
            );
            None
        }
    }
}

fn run_one_shot(prompt: &str, args: &Args) -> Result<(), Box<dyn std::error::Error>> {
    // `!`/`!!` shell escape: run directly, no agent turn, no
    // approval — the `!` itself is the approval. A bare `!`/`!!` falls
    // through to the agent. The run is saved to the session so a
    // later turn sees it (`!` in context, `!!` excluded); `--no-session`
    // keeps it ephemeral.
    if let Some((command, excluded)) = crate::tools::parse_shell_escape(prompt.trim()) {
        let mut map = Map::new();
        map.insert("command".to_string(), Value::String(command.clone()));
        let result = execute("bash", &map, &GlobalCancellation);
        let (output, success, code) = match &result {
            Ok(out) => (out.clone(), true, Some(0)),
            Err(error) => {
                let code = match error {
                    crate::tools::ToolError::Shell { code, .. } => *code,
                    _ => None,
                };
                (format!("Error: {error}"), false, code)
            }
        };
        if !args.no_session {
            let cwd = env::current_dir()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();
            if let Some(mut session) = open_session(args, &cwd) {
                // Same text the daemon persists for shell runs;
                // `!!` is tagged out of the model-bound history.
                let text = crate::core::format::bash_context_text(
                    &command,
                    &output,
                    success,
                    code,
                    crate::core::console::is_interrupted(),
                );
                let msg = if excluded {
                    ChatMessage::user_named(text, crate::core::types::BASH_EXCLUDED_NAME)
                } else {
                    ChatMessage::user(text)
                };
                if let Err(error) = session.append_message(&msg) {
                    eprintln!("[session] could not persist shell run ({error})");
                }
            }
        }
        return match result {
            Ok(out) => {
                print!("{out}");
                Ok(())
            }
            Err(error) => Err(format!("Error: {error}").into()),
        };
    } // MCP bootstrap (background connect; schema merges whatever is cached).
      // Daemon paths bootstrap in `run_daemon`; one-shot turns run in-process.
    crate::mcp::global_manager();
    // Cold-init fan-out (perf doc §26): extension Lua loads, the config
    // build (cold catalog parse + index), the skills dir walk, and the
    // session open + history parse are independent — scoped threads overlap
    // them instead of summing them. The daemon instead fills the extension
    // cache in the background; one-shot turns need the schema inline.
    let mut skill_dirs = skill_dirs();
    skill_dirs.extend(args.skill_dirs.iter().cloned());
    let cwd = env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let (config_result, skills, (mut session, history)) = std::thread::scope(|s| {
        let ext = s
            .spawn(|| crate::client::http::block_on(crate::extensions::global_manager().refresh()));
        // `from_env`'s boxed error is not `Send`; stringify it at the
        // thread boundary (same pattern as `from_env_async`).
        let cfg = s.spawn(|| {
            LlmConfig::from_env(
                args.base_url.clone(),
                args.model.clone(),
                args.permission,
                &args.headers,
            )
            .map_err(|e| e.to_string())
        });
        let sk = s.spawn(|| discover_skills(&skill_dirs));
        // Extension refresh is best-effort; a failed join must not fail the turn.
        let _ = ext.join();
        let config_result: Result<LlmConfig, Box<dyn std::error::Error>> = cfg
            .join()
            .unwrap_or_else(|_| Err("config init thread failed".to_string()))
            .map_err(|e| e.into());
        // Open the session only after the config validates: `Session::new`
        // writes its header immediately, so resolving config first keeps a
        // config error (bad key/model) from leaving a stray empty session
        // that shows up in `/resume`.
        let sess = if config_result.is_ok() {
            s.spawn(|| {
                if args.no_session {
                    return (None, Vec::new());
                }
                let session = open_session(args, &cwd);
                // Model-bound load: `!!` shell runs stay out of the LLM context.
                let history = session
                    .as_ref()
                    .and_then(|sess| sess.path())
                    .map(load_llm_messages_from_session)
                    .unwrap_or_else(|| Ok(Vec::new()))
                    .unwrap_or_default();
                (session, history)
            })
            .join()
            .unwrap_or((None, Vec::new()))
        } else {
            (None, Vec::new())
        };
        let skills = sk.join().unwrap_or_default();
        (config_result, skills, sess)
    });
    let mut config = config_result?;
    // Complexity router (V1): an explicit `--model` always wins;
    // otherwise the classified tier resolves through routing.balanced: →
    // top-level model:, rebuilding once with the tier's model.
    let routed = if args.model.as_deref().is_some_and(|m| !m.trim().is_empty()) {
        None
    } else {
        crate::llm::config::route_turn(prompt, &history)
    };
    let routed_tier = routed.as_ref().map(|r| r.tier.to_string());
    let routed_reason = routed.as_ref().map(|r| r.reason);
    if let Some(model) = routed.and_then(|r| r.model_override) {
        config = LlmConfig::from_env(
            args.base_url.clone(),
            Some(model),
            args.permission,
            &args.headers,
        )
        .map_err(|e| -> Box<dyn std::error::Error> { e.to_string().into() })?;
    }
    // Surface the routed tier: headless users get no other signal that the
    // model changed under them (the tier is also journaled on `turn_start`).
    if let Some(tier) = routed_tier.as_deref() {
        let why = routed_reason.unwrap_or("ordinary work");
        eprintln!("dex: routing → {tier} ({why}; model {})", config.model);
    }
    // No TUI here, so stderr is safe: keep the mismatch hint CLI users had.
    if let Some(warning) = config.thinking_mismatch_warning() {
        eprintln!("dex: {warning}");
    }
    // Verification opt-in only — see llm/config.rs.
    crate::llm::config::apply_verify_optin(&mut config);
    if let Some(name) = &args.session_name {
        if let Some(session) = session.as_mut() {
            let _ = session.set_name(name.clone());
        }
    }
    let mut messages = vec![ChatMessage::system(system_prompt_with_override(
        &skills,
        cli_system_prompt(args)
            .as_ref()
            .map(|(text, _)| text.as_str()),
    ))];
    // History already loaded on the session thread above (`!!` runs
    // excluded there); the turn below appends the new user message.
    messages.extend(history);
    let user = ChatMessage::user(prompt);
    if let Some(session) = session.as_mut() {
        let _ = session.turn_event_with_tier("turn_start", routed_tier.as_deref());
        let _ = session.append_message(&user);
    }
    messages.push(user);
    // Console Go routing requires `x-opencode-session`;
    // explicit `--header` flags already baked into `extra_headers` still win.
    if let Some(session) = session.as_ref() {
        crate::llm::config::apply_opencode_session_headers(&mut config, session.id());
    }
    let mut state = ToolState::load();
    let console = crate::core::console::Console::none();
    let result = crate::client::http::block_on(process_turn(AgentRuntime {
        config: &config,
        messages: &mut messages,
        state: &mut state,
        steering_rx: None,
        steering_accepted_tx: None,
        session: session.as_mut(),
        client: &config,
        cancel: &crate::agent::state::GlobalCancellation,
        console: &console,
        filter: None,
        agent_ctx: None,
        tool_budget: None,
    }));
    if let Some(session) = session.as_mut() {
        let _ = session.turn_event(if result.is_ok() {
            "turn_complete"
        } else {
            "turn_failed"
        });
    }
    // Spend summary for scripts — stderr only, so stdout stays model prose.
    if let Some(summary) = spend_summary(state.total_usage, state.total_output, state.total_cost) {
        eprintln!("{summary}");
    }
    result
        .map(|_| ())
        .map_err(|e| -> Box<dyn std::error::Error> { e.to_string().into() })
}

fn run_interactive() {
    eprintln!("dex raw tool mode");
    eprintln!("tools: read, ls, bash, write, edit, grep, find (aliases ffgrep/fffind), git");
    eprintln!("send JSON lines like: {{\"name\":\"read\",\"args\":{{\"path\":\"Cargo.toml\"}}}}");
    eprintln!("empty line quits");

    let stdin = io::stdin();
    let mut stdout = io::stdout();
    for line in stdin.lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                println!("{}", json!({"err": format!("read error: {}", e)}));
                continue;
            }
        };
        if line.trim().is_empty() {
            break;
        }
        let parsed: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                println!("{}", json!({"err": format!("json error: {}", e)}));
                continue;
            }
        };
        let name = match parsed.get("name").and_then(Value::as_str) {
            Some(n) => n,
            None => {
                println!("{}", json!({"err": "missing 'name'"}));
                continue;
            }
        };
        let args = match parsed.get("args").and_then(Value::as_object) {
            Some(a) => a.clone(),
            None => Map::new(),
        };
        let result = match execute(name, &args, &GlobalCancellation) {
            Ok(out) => json!({"ok": out}),
            Err(e) => json!({"err": e.to_string()}),
        };
        println!("{}", result);
        let _ = stdout.flush();
    }
}

/// Start the daemon server on a background thread with a pre-bound listener
/// (no window for another process to steal the port), and wait until it is
/// ready to serve. Returns the address it listens on.
fn start_daemon_background() -> std::io::Result<std::net::SocketAddr> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?;

    std::thread::spawn(move || {
        // Single-threaded: this runtime only pumps the local HTTP server
        // (async I/O plus `spawn_blocking` file work). A default
        // multi-thread pool spins up one worker per core (~10-50ms) on the
        // TUI's critical path for no concurrent-CPU gain.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to create tokio runtime");
        rt.block_on(async {
            if let Err(e) = daemon::run_daemon(listener).await {
                eprintln!("daemon error: {e}");
            }
        });
    });

    // Block until /health answers so the TUI never races server startup.
    let url = format!("http://{addr}");
    let client = client::http::DaemonClient::new(&url)
        .map_err(|e| std::io::Error::other(format!("failed to reach daemon: {e}")))?;
    client
        .wait_until_ready(std::time::Duration::from_secs(10))
        .map_err(|e| std::io::Error::other(format!("daemon startup failed: {e}")))?;
    Ok(addr)
}

fn print_help() {
    println!(
        "dex {version}\n\
        \n\
        Usage: dex [OPTIONS] [COMMAND|PROMPT]\n\
        \n\
        Commands:\n  \
        serve [bind]              daemon on 127.0.0.1:8420\n  \
        connect <url> [prompt]    TUI or one-shot against a daemon\n  \
        run <tool> k=v...         one-shot tool (read, ls, bash, write, edit, grep, find)\n  \
          doctor                    show resolved provider/model config + origins\n  \
          usage <id|path>           plot token usage per model call from a session's event journal\n  \
          mcp [status|login|logout]   MCP OAuth for HTTP servers (status|login <server>|logout <server>)\n  \
        update [--models|--all]   update dex itself; --models refreshes the model catalog\n  \
        --tool                    raw JSON tool mode (stdin)\n\
        \n\
        Options:\n  \
        --model <name>            --base-url <url>  --permission <mode>\n  \
        --system-prompt <text>  --system-prompt-file <path>  custom base prompt\n  \
        -H/--header <\"Name: Value\">  (repeatable) extra provider headers\n  \
        -s/--session <path>  --no-session  -n/--new  --name <name>  --skill <dir>\n  \
        --extensions-dir <dir>  (repeatable) extra extension search dirs\n  \
        -h/--help  -V/--version",
        version = env!("CARGO_PKG_VERSION")
    );
}

/// Run the `dex serve` daemon: resolve the bind address, warn when it is
/// unspecified, prepare the bearer token, bind, and block on the server.
/// Parse, bind, and daemon failures exit 1.
fn run_serve(bind: &str) {
    let addr: std::net::SocketAddr = if bind.contains(':') {
        bind.parse().unwrap_or_else(|_| {
            eprintln!("error: invalid bind address '{bind}' (use [host:]port)");
            std::process::exit(1);
        })
    } else {
        ([127, 0, 0, 1], bind.parse().unwrap_or(8420)).into()
    };
    if addr.ip().is_unspecified() {
        eprintln!(
            "warning: daemon listening on {addr} is exposed on all interfaces — a bearer token is required (set DEX_DAEMON_TOKEN or copy the generated daemon.token); prefer 127.0.0.1 for local use"
        );
    }
    // Non-loopback binds (or an explicit DEX_DAEMON_TOKEN) get a
    // bearer token: the daemon runs tools in a workspace, so an
    // unauthenticated reachable endpoint is remote code execution.
    daemon::prepare_daemon_token(&addr);
    let listener = match std::net::TcpListener::bind(addr) {
        Ok(listener) => listener,
        Err(e) => {
            eprintln!("daemon error: cannot bind {addr}: {e}");
            std::process::exit(1);
        }
    };
    println!(
        "dex daemon listening on {}",
        listener.local_addr().unwrap_or(addr)
    );
    let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
    rt.block_on(async {
        if let Err(e) = daemon::run_daemon(listener).await {
            eprintln!("daemon error: {e}");
            std::process::exit(1);
        }
    });
}

/// Shared `dex mcp <action>` outcome plumbing: print the action's Ok
/// message; on Err, report with `fail_prefix` and exit 1; a missing server
/// argument (`None`) prints the arm's usage instead.
fn run_or_exit(result: Option<Result<String, String>>, usage: &str, fail_prefix: &str) {
    match result {
        Some(Ok(message)) => println!("{message}"),
        Some(Err(error)) => {
            eprintln!("{fail_prefix}{error}");
            std::process::exit(1);
        }
        None => {
            eprintln!("{usage}");
            std::process::exit(1);
        }
    }
}

/// `dex run <tool>`: extension tools resolve from the manager cache —
/// lazy-load just the addressed extension (§26) so `dex run ext__...` boots
/// one Lua VM instead of every installed one (MCP tools have the same race;
/// out of scope here). The deprecated `lua__` alias preloads too, so old
/// one-liners keep working. A name that doesn't split keeps the old full
/// refresh so the dispatch error below stays the authority on what exists.
fn run_run_tool(name: &str, raw_args: &[String]) {
    let parsed = match cli::parse_tool_args(raw_args) {
        Ok(parsed) => parsed,
        Err(error) => {
            eprintln!("error: {error}");
            std::process::exit(1);
        }
    };
    if crate::extensions::is_extension_tool(name) {
        let normalized = crate::extensions::normalize_tool_name(name);
        match crate::extensions::split_ext_name(&normalized) {
            Some((ext, _)) => {
                if let Err(e) = crate::client::http::block_on(
                    crate::extensions::global_manager().ensure_loaded(ext),
                ) {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            }
            None => {
                crate::client::http::block_on(crate::extensions::global_manager().refresh());
            }
        }
    }
    match execute(name, &parsed, &GlobalCancellation) {
        Ok(out) => print!("{out}"),
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    }
}

/// `dex extensions <action>` — list, toggle, install, remove.
fn run_extensions(action: &str, name: Option<&str>) {
    match action {
        "list" => {
            crate::client::http::block_on(crate::extensions::global_manager().refresh());
            crate::extensions::list_command();
        }
        "enable" | "disable" => {
            let Some(id) = name else {
                eprintln!("usage: dex extensions {action} <id>");
                std::process::exit(2);
            };
            match crate::extensions::set_enabled(id, action == "enable") {
                Ok(()) => println!("{action}d extension '{id}' (takes effect on next load)"),
                Err(e) => {
                    eprintln!("Error: {e}");
                    std::process::exit(1);
                }
            }
        }
        "install" => {
            let Some(src) = name else {
                eprintln!("usage: dex extensions install <dir>");
                std::process::exit(2);
            };
            match crate::extensions::install(src) {
                Ok(id) => println!("installed '{id}' — run `dex extensions list`"),
                Err(e) => {
                    eprintln!("Error: {e}");
                    std::process::exit(1);
                }
            }
        }
        "remove" => {
            let Some(id) = name else {
                eprintln!("usage: dex extensions remove <id>");
                std::process::exit(2);
            };
            match crate::extensions::remove(id) {
                Ok(()) => println!("removed '{id}'"),
                Err(e) => {
                    eprintln!("Error: {e}");
                    std::process::exit(1);
                }
            }
        }
        _ => {
            eprintln!(
                "usage: dex extensions [list|enable <id>|disable <id>|install <dir>|remove <id>]"
            );
            std::process::exit(2);
        }
    }
}

/// `dex mcp <action>` — OAuth status/login/logout.
fn run_mcp(action: &str, server: Option<&str>) {
    match action {
        "status" => {
            let lines = crate::mcp::oauth::auth_lines();
            if lines.is_empty() {
                println!("no HTTP MCP servers configured");
            }
            for line in lines {
                println!("{line}");
            }
        }
        "login" => run_or_exit(
            server.map(|server| crate::client::http::block_on(crate::mcp::oauth::login(server))),
            "usage: dex mcp login <server>",
            "mcp login failed: ",
        ),
        "logout" => run_or_exit(
            server.map(crate::mcp::oauth::logout),
            "usage: dex mcp logout <server>",
            "mcp logout failed: ",
        ),
        _ => run_or_exit(
            None,
            "usage: dex mcp [status|login <server>|logout <server>]",
            "",
        ),
    }
}

fn main() {
    crate::core::logging::init();
    crate::ui::mark_launch_start();
    install_sigint_handler();
    let args = cli::parse_args();
    let mode = cli::resolve_mode(&args);
    if let Err(error) = cli::check_reattach_mode(&args, &mode) {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
    // Delegation tools register only in daemon-backed processes (§10): a
    // one-shot run has no manager to spawn into, so the tools stay out of
    // its schema entirely. Dispatch rejects them there regardless (§11).
    crate::agent::subagent::set_daemon_linked(matches!(mode, Mode::Serve { .. } | Mode::Default));
    // Extension search dirs: process-global, read by the manager at
    // bootstrap (daemon) and before the in-process one-shot turn.
    crate::extensions::set_extra_dirs(args.extension_dirs.clone());
    // CLI model overrides for the worker-thread `dex.model` view, which
    // resolves from file+env and never sees flags (daemon per-request
    // overrides instead flow through `process_turn`'s served snapshot).
    crate::llm::config::set_cli_model_overrides(
        args.model.clone(),
        args.base_url.clone(),
        args.headers.clone(),
    );

    match mode {
        Mode::Help => {
            print_help();
        }
        Mode::Version => {
            println!("dex {}", env!("CARGO_PKG_VERSION"));
        }
        Mode::Serve { bind } => {
            run_serve(&bind);
        }
        Mode::Connect { url } => {
            // `dex connect <url>` opens the TUI; `dex connect <url> "prompt"`
            // runs a one-shot turn against the daemon.
            let prompt = args
                .rest
                .get(2..)
                .map(|rest| rest.join(" "))
                .filter(|p| !p.trim().is_empty());
            let result = match prompt {
                Some(prompt) => client::http::DaemonClient::new(&url).and_then(|client| {
                    client.wait_until_ready(std::time::Duration::from_secs(10))?;
                    client::repl::one_shot(
                        &client,
                        &prompt,
                        &chat_options_from_args(&args),
                        args.session_name.as_deref(),
                    )
                }),
                // `false`: a loopback URL here may still be an SSH port-forward
                // or a container's daemon, so never assume the session is
                // reachable by a bare local `dex --reattach`.
                None => ui::run_ratatui_repl_with_remote(&args, &url, false).map_err(Into::into),
            };
            if let Err(e) = result {
                eprintln!("client error: {}", e);
                std::process::exit(1);
            }
        }
        Mode::Doctor => {
            let system_prompt = cli_system_prompt(&args);
            print!(
                "{}",
                crate::llm::config::doctor(
                    args.base_url.clone(),
                    args.model.clone(),
                    args.permission,
                    &args.headers,
                    system_prompt,
                )
            );
        }
        Mode::Update { models, all } => {
            // Bare `dex update` self-updates the binary; `--models` keeps the
            // old catalog refresh; `--all` does both.
            if all || !models {
                match crate::update::self_update() {
                    Ok(message) => println!("{message}"),
                    Err(e) => {
                        eprintln!("self-update failed: {e}");
                        std::process::exit(1);
                    }
                }
            }
            if models || all {
                if let Err(e) = crate::llm::config::refresh_models_cache() {
                    eprintln!("update --models failed: {e}");
                    std::process::exit(1);
                }
            }
        }
        Mode::Usage { session } => {
            if let Err(e) = crate::usage::run(&session) {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
        Mode::Default => {
            // Start server in background, then launch TUI connected to it.
            let addr = match start_daemon_background() {
                Ok(addr) => addr,
                Err(e) => {
                    eprintln!("daemon error: {}", e);
                    std::process::exit(1);
                }
            };
            let url = format!("http://{addr}");
            // `true`: this process owns the daemon it just started, so its cwd
            // is the workspace a later `dex --reattach <id>` would resolve.
            if let Err(e) = ui::run_ratatui_repl_with_remote(&args, &url, true) {
                eprintln!("ui error: {}", e);
                std::process::exit(1);
            }
        }
        Mode::OneShot { prompt } => {
            if let Err(e) = run_one_shot(&prompt, &args) {
                eprintln!("agent error: {}", e);
                std::process::exit(1);
            }
        }
        Mode::Tool => {
            run_interactive();
        }
        Mode::RunTool { name, args } => run_run_tool(&name, &args),
        Mode::Extensions { action, name } => run_extensions(&action, name.as_deref()),
        Mode::Mcp { action, server } => run_mcp(&action, server.as_deref()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spend_summary_gates_and_formats() {
        assert_eq!(spend_summary(0, 0, 0.0), None);
        // Output-only usage still summarizes (prompt may be unreported).
        assert_eq!(
            spend_summary(0, 12, 0.0).as_deref(),
            Some("[dex] 0 prompt / 12 output tokens")
        );
        // Sub-threshold cost is hidden; above it, three decimals.
        assert!(!spend_summary(10, 2, 0.0001).unwrap().contains('$'));
        assert_eq!(
            spend_summary(10, 2, 0.0123).as_deref(),
            Some("[dex] 10 prompt / 2 output tokens · $0.012")
        );
    }
}
