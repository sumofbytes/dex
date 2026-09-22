use super::{pinned_key_env, stored_thinking_effort};

use super::agent_wake_origin;
use super::catalog_query::catalog_env_vars;
use super::config_file_path;
use super::context_window_from_env;
use super::ctx_index::catalog_context_window;
use super::ctx_index::ctx_from_index;
use super::ctx_index::dex_catalog_cache_path;
use super::headers::config_headers_map;
use super::headers::custom_headers_from_env;
use super::headers::insert_parsed_headers;
use super::load_config_file;
use super::load_config_num;
use super::load_config_str;
use super::prompt_source::system_prompt_origin_with_label;
use super::provider::known_providers;
use super::provider::load_provider_entries;
use super::provider::model_api_from_env;
use super::provider::resolve_provider;
use super::provider::ProviderEntry;
use super::routing::classify::Tier;
use super::routing::routing_effort_display;
use super::routing::routing_resolution;
use super::routing::routing_tier_display;
use super::selection::classify_selection;
use super::selection::provider_fallback_with_origin;
use super::selection::provider_without_prefix;
use super::selection::resolve_selection;
use super::selection::split_selection;
use super::selection::SelectionRoute;
use super::LlmConfig;
use crate::protocol::ApiProtocol;
use crate::protocol::PermissionMode;
use crate::protocol::Provider;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use unicode_width::UnicodeWidthStr;

use std::env;

/// One `dex doctor` row: three fixed columns — key (KEY_COLS), value
/// (VALUE_COLS), origin. Widths count display columns (CJK chars render 2
/// wide), so padded values still line up. A value that overflows its column
/// wraps: the value prints in full on its own line and the origin hangs at the
/// origin column, so long paths never run into the origin text.
pub(crate) fn row(out: &mut String, key: &str, value: &str, source: &str) {
    const KEY_COLS: usize = 23;
    const VALUE_COLS: usize = 46;
    // A key wider than its column would collapse the padding and shift every
    // origin column: fail in debug builds instead.
    debug_assert!(
        UnicodeWidthStr::width(key) <= KEY_COLS,
        "doctor key {key:?} exceeds its {KEY_COLS}-column field"
    );
    let origin_indent = " ".repeat(KEY_COLS + VALUE_COLS);
    let fits = UnicodeWidthStr::width(value) <= VALUE_COLS;
    let mut origin_lines = source.split('\n');
    let inline = if fits {
        origin_lines.next().unwrap_or_default()
    } else {
        ""
    };
    if inline.is_empty() {
        out.push_str(&format!("{key:<KEY_COLS$}{value}\n"));
    } else {
        let pad = " ".repeat(VALUE_COLS - UnicodeWidthStr::width(value));
        out.push_str(&format!("{key:<KEY_COLS$}{value}{pad}{inline}\n"));
    }
    for line in origin_lines {
        if !line.is_empty() {
            out.push_str(&format!("{origin_indent}{line}\n"));
        }
    }
}

fn permission_name(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::ReadOnly => "read-only",
        PermissionMode::Ask => "ask",
        PermissionMode::Trusted => "trusted",
    }
}

/// Everything `doctor`'s provider section needs: the resolved selection, the
/// live build (when it succeeded), and the raw config sources it explains.
struct ProviderDoctor<'a> {
    pub(crate) file: &'a Option<serde_yaml::Value>,
    pub(crate) provider_entries: &'a BTreeMap<String, ProviderEntry>,
    pub(crate) known: &'a BTreeSet<String>,
    pub(crate) provider_name: &'a str,
    pub(crate) provider_source: &'a str,
    pub(crate) selection_source: &'a str,
    pub(crate) has_selection: bool,
    pub(crate) routing_selection: Option<&'a str>,
    pub(crate) routing_selection_source: &'a str,
    pub(crate) pre_model: &'a str,
    pub(crate) cfg_result: &'a Result<LlmConfig, Box<dyn std::error::Error>>,
    pub(crate) flag_base_url: Option<&'a str>,
    pub(crate) permission_override: Option<PermissionMode>,
    pub(crate) header_overrides: &'a [String],
    pub(crate) custom_route: bool,
}

/// Where the resolved `base_url` came from: an explicit pin wins, then catalog
/// routing, then the provider entry, then the built-in/catalog default.
fn base_url_source(
    flag_base_url: Option<&str>,
    file_base_url: Option<&str>,
    live: Option<&LlmConfig>,
    landing: Option<&str>,
    base_url: &str,
    entry: Option<&ProviderEntry>,
    provider: &Provider,
) -> String {
    if flag_base_url.is_some() {
        "--base-url (pins endpoint)".to_string()
    } else if file_base_url.is_some() {
        "config base_url: (deprecated)".to_string()
    } else if live.is_some() && landing != Some(base_url) {
        match live.and_then(|c| {
            c.endpoints
                .iter()
                .find(|(_, url)| url.as_str() == base_url)
                .map(|(name, _)| name.clone())
        }) {
            Some(name) => format!("endpoint {name} (models.dev routing)"),
            None => "models.dev catalog routing".to_string(),
        }
    } else if entry.and_then(|e| e.base_url.clone()).is_some() {
        "config providers.<name>.base_url".to_string()
    } else if matches!(
        provider,
        Provider::OpenCode | Provider::OpenAiCodex | Provider::Anthropic
    ) {
        "built-in default".to_string()
    } else {
        "models.dev catalog".to_string()
    }
}

/// Where the API key resolves from — the deposit place, never the value.
fn api_key_source(provider: &Provider, entry: Option<&ProviderEntry>) -> String {
    if matches!(provider, Provider::OpenAiCodex) {
        if env::var_os("CODEX_ACCESS_TOKEN").is_some() {
            "CODEX_ACCESS_TOKEN".to_string()
        } else {
            let home = env::var_os("CODEX_HOME")
                .map(std::path::PathBuf::from)
                .or_else(|| env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".codex")))
                .unwrap_or_default();
            format!("{} (codex auth)", home.join("auth.json").display())
        }
    } else if entry
        .and_then(|e| e.api_key.clone())
        .filter(|k| !k.is_empty())
        .is_some()
    {
        format!("config providers.{}.api_key", provider.name())
    } else {
        let mut names = catalog_env_vars(provider.name());
        // Mirror `resolve_credentials`: pinned builtin vars resolve cache-less,
        // ahead of any catalog `env` discovery.
        if let Some(pinned) = pinned_key_env(provider) {
            if !names.iter().any(|v| v == pinned) {
                names.insert(0, pinned.to_string());
            }
        }
        match names
            .iter()
            .find(|n| env::var(n).map(|v| !v.trim().is_empty()).unwrap_or(false))
        {
            Some(name) => format!("{name} (environment)"),
            None => "MISSING — set providers.<name>.api_key or the provider's env var".to_string(),
        }
    }
}

/// The wire protocol and where it came from. Lookup keys mirror `from_env`:
/// the post-split remainder first, then the stripped model.
fn protocol_source(
    entry: Option<&ProviderEntry>,
    file: &Option<serde_yaml::Value>,
    model_api: Option<ApiProtocol>,
    base_url: &str,
    model: &str,
    provider: &Provider,
) -> (String, &'static str) {
    if let Some(api) = entry.and_then(|e| e.api) {
        (api.name().to_string(), "config providers.<name>.api")
    } else if let Some(name) = load_config_str(file, "api") {
        (
            ApiProtocol::parse(&name)
                .map(|a| a.name().to_string())
                .unwrap_or_else(|| format!("INVALID '{name}'")),
            "config api: (deprecated)",
        )
    } else if let Some(api) = model_api {
        (api.name().to_string(), "DEX_MODEL_APIS")
    } else if let Some(api) = crate::llm::learned::lookup(base_url, model) {
        (api.name().to_string(), "learned (learned-apis.json)")
    } else if let Some(api) = provider.default_api() {
        (api.name().to_string(), "built-in provider default")
    } else {
        (
            "openai-responses".to_string(),
            "default (auto-fallback to completions)",
        )
    }
}

/// Context window and its origin: env > file > index > catalog (no built-in
/// default — a model nothing sizes is a config error, not a 128k guess).
fn context_source(file: &Option<serde_yaml::Value>, model: &str) -> (String, &'static str) {
    if let Some(v) = context_window_from_env() {
        (v.to_string(), "DEX_CONTEXT_WINDOW")
    } else if let Some(ctx) = load_config_num(file, "context_window") {
        (ctx.to_string(), "config context_window:")
    } else if let Some(ctx) = ctx_from_index(model) {
        (ctx.to_string(), "cached context index")
    } else {
        match catalog_context_window(model) {
            Some(ctx) => (ctx.to_string(), "models.dev catalog"),
            None => (
                "UNKNOWN".to_string(),
                "no catalog entry for this model — set context_window: or DEX_CONTEXT_WINDOW",
            ),
        }
    }
}

/// Thinking-effort and its origin: stored choice > env > file > model default.
fn thinking_source(
    file: &Option<serde_yaml::Value>,
    base_url: &str,
    model: &str,
) -> (String, &'static str) {
    if let Some(e) = stored_thinking_effort(base_url, model) {
        (e, "stored /thinking choice")
    } else if let Some(e) = env::var("DEX_THINKING_EFFORT")
        .ok()
        .filter(|v| !v.trim().is_empty())
    {
        (e, "DEX_THINKING_EFFORT")
    } else if let Some(e) = load_config_str(file, "thinking_effort") {
        (e, "config thinking_effort:")
    } else {
        ("(unset)".to_string(), "model default")
    }
}

/// The provider/model/endpoint/key/protocol rows. Values come from the live
/// build when it succeeds; otherwise the derived values still explain the
/// setup (e.g. missing key). Origins mirror `from_env` exactly.
fn provider_section(out: &mut String, d: &ProviderDoctor<'_>) {
    let provider_opt = Provider::parse_known(d.provider_name, d.known).or_else(|| {
        d.custom_route
            .then(|| Provider::Generic("custom".to_string()))
    });
    let Some(provider) = provider_opt else {
        row(
            out,
            "provider",
            d.provider_name,
            "UNSUPPORTED — use opencode, openai-codex, anthropic, or add it under 'providers:'",
        );
        return;
    };
    let live = d
        .cfg_result
        .as_ref()
        .ok()
        .filter(|c| c.provider == provider);
    row(out, "provider", provider.name(), d.provider_source);
    let model = live.map(|c| c.model.clone()).unwrap_or_else(|| {
        if d.pre_model.is_empty() {
            "(unset)".to_string()
        } else {
            d.pre_model.to_string()
        }
    });
    // An unset model despite a selection is a bare provider pick: name the
    // selection as origin and why the row is empty (the resolve row carries
    // the fix).
    let model_source = if d.has_selection && d.pre_model.is_empty() {
        format!("{} (no model id)", d.selection_source)
    } else {
        d.selection_source.to_string()
    };
    row(out, "model", &model, &model_source);

    let resolved = resolve_provider(&provider, d.provider_entries);
    let entry = d.provider_entries.get(provider.name());
    let file_base_url = load_config_str(d.file, "base_url");
    let derived_base = d
        .flag_base_url
        .map(str::to_string)
        .or(file_base_url.clone())
        .or(resolved.landing.clone())
        .unwrap_or_default();
    let base_url = live.map(|c| c.base_url.clone()).unwrap_or(derived_base);
    let base_source = base_url_source(
        d.flag_base_url,
        file_base_url.as_deref(),
        live,
        resolved.landing.as_deref(),
        &base_url,
        entry,
        &provider,
    );
    row(out, "base_url", &base_url, &base_source);

    // Credentials — name the deposit place, never the value.
    let key_source = api_key_source(&provider, entry);
    row(out, "api key", "(hidden)", &key_source);

    // Wire protocol and its origin. Lookup keys mirror `from_env`: the
    // post-split remainder first, then the stripped model.
    let model_api = model_api_from_env(d.pre_model, &model);
    let (chain_api, api_source) =
        protocol_source(entry, d.file, model_api, &base_url, &model, &provider);
    let api = live.map(|c| c.api.name().to_string()).unwrap_or(chain_api);
    row(out, "protocol", &api, api_source);
    let (chain_ctx, ctx_source) = context_source(d.file, &model);
    let ctx = live
        .map(|c| c.context_window.to_string())
        .unwrap_or(chain_ctx);
    row(out, "context", &format!("{ctx} tokens"), ctx_source);

    // Threshold-compaction mode: value and origin owned by the Jev module
    // next to its parse, so the row cannot drift from runtime behavior.
    // Printed unconditionally.
    let (compaction, compaction_source) = crate::agent::compaction::verbatim::compaction_doctor();
    row(out, "compaction", &compaction, &compaction_source);

    // Live Jev scorer: key comes only from the environment; the config
    // `jev:` table is the opt-in. Print the key masked, like `api key`.
    let jev_row = match (
        env::var(crate::agent::compaction::verbatim::JEV_KEY_ENV)
            .ok()
            .filter(|k| !k.trim().is_empty()),
        crate::agent::compaction::verbatim::live_credentials(),
    ) {
        (_, Some((url, _))) => {
            let value = if compaction.contains("jev") {
                "live jev scorer".to_string()
            } else {
                "live jev scorer (inactive — compaction not =jev)".to_string()
            };
            (value, url)
        }
        (Some(_), None) => (
            "heuristic scorer".to_string(),
            "TYPESAFE_API_KEY set — add a config 'jev:' table to opt in".to_string(),
        ),
        (None, None) => (
            "heuristic scorer".to_string(),
            "no TYPESAFE_API_KEY".to_string(),
        ),
    };
    row(out, "jev scorer", &jev_row.0, &jev_row.1);

    let (chain_effort, effort_source) = thinking_source(d.file, &base_url, &model);
    let effort = live
        .and_then(|c| c.thinking_effort.clone())
        .unwrap_or(chain_effort);
    row(out, "thinking", &effort, effort_source);

    let (perm, perm_source) = match live.map(|c| c.permission) {
        Some(mode) => {
            let source = if d.permission_override.is_some() {
                "--permission"
            } else if env::var("DEX_PERMISSION")
                .ok()
                .filter(|v| !v.trim().is_empty())
                .is_some()
            {
                "DEX_PERMISSION"
            } else {
                "built-in default"
            };
            (permission_name(mode).to_string(), source.to_string())
        }
        // The build failed (e.g. missing key): still show the origin.
        None => match env::var("DEX_PERMISSION")
            .ok()
            .filter(|v| PermissionMode::parse(v).is_ok())
        {
            Some(v) => (v, "DEX_PERMISSION".to_string()),
            None => ("trusted".to_string(), "built-in default".to_string()),
        },
    };
    row(out, "permission", &perm, &perm_source);
    let (wake, wake_source) = agent_wake_origin();
    row(
        out,
        "agent wake",
        if wake { "on" } else { "off" },
        wake_source,
    );
    // Complexity router: one row per value — the switch plus each tier's
    // resolved (model, effort) tuple (model: tier miss → routing.balanced: →
    // top-level model:; effort: tier miss → routing.balanced_effort: → keep
    // thinking_effort:), sharing `from_env`'s resolution.
    let routing = routing_resolution(d.file);
    row(
        out,
        "routing",
        if routing.enabled { "on" } else { "off" },
        routing.enabled_origin,
    );
    for tier in Tier::ALL {
        let (value, origin) = routing_tier_display(
            tier,
            &routing,
            d.routing_selection,
            d.routing_selection_source,
        );
        row(out, &format!("routing {}", tier.key()), &value, &origin);
        let (effort, effort_origin) = routing_effort_display(tier, &routing);
        row(
            out,
            &format!("routing {} effort", tier.key()),
            &effort,
            &effort_origin,
        );
    }

    // Headers: count per layer, sources joined.
    let mut header_count = 0;
    let mut header_sources: Vec<&str> = Vec::new();
    let http_headers = config_headers_map(d.file, "http_headers");
    let file_headers = config_headers_map(d.file, "headers");
    let entry_headers = entry.map(|e| e.headers.clone()).unwrap_or_default();
    let env_headers = custom_headers_from_env();
    let mut cli_headers = BTreeMap::new();
    for raw in d.header_overrides {
        insert_parsed_headers(&mut cli_headers, raw);
    }
    for (headers, source) in [
        (&http_headers, "config http_headers: (deprecated)"),
        (&file_headers, "config headers: (deprecated)"),
        (&entry_headers, "config providers.<name>.headers"),
        (&env_headers, "DEX_HEADERS"),
        (&cli_headers, "--header"),
    ] {
        header_count += headers.len();
        if !headers.is_empty() {
            header_sources.push(source);
        }
    }
    row(
        out,
        "headers",
        &header_count.to_string(),
        &if header_sources.is_empty() {
            "none".to_string()
        } else {
            header_sources.join(" + ")
        },
    );

    if !resolved.endpoints.is_empty() {
        let names: Vec<&str> = resolved.endpoints.keys().map(String::as_str).collect();
        row(
            out,
            "endpoints",
            &names.join(", "),
            "available to /model routing",
        );
    }
    if !d.provider_entries.is_empty() {
        let names: Vec<&str> = d.provider_entries.keys().map(String::as_str).collect();
        row(
            out,
            "providers",
            &names.join(", "),
            "configured in providers:",
        );
    }
}

/// `doctor`'s view of the selection: (provider prefix, pre-routing model id,
/// bare-provider-pick flag). A bare provider pick (`--model anthropic`) or
/// provider-only prefix (`model: zai/`) is not a model id — `split_selection`
/// rejects both, so surface the provider on the provider row and leave the
/// model row unset instead of reading the provider name as a model id (the
/// resolve row carries the fix).
fn doctor_selection_parts(
    selection: Option<&str>,
    known: &BTreeSet<String>,
) -> (Option<String>, String, bool) {
    let Some(selection) = selection else {
        return (None, String::new(), false);
    };
    match split_selection(selection, known) {
        Ok((provider, model)) => (provider, model, false),
        Err(_) => match classify_selection(selection, known) {
            SelectionRoute::BareProvider { provider } => (Some(provider), String::new(), true),
            SelectionRoute::ProviderQualified { provider, .. } => {
                (Some(provider), String::new(), false)
            }
            // Not a provider shape: echo the raw selection as the model id
            // (endpoint prefixes, gateway model ids).
            SelectionRoute::Model(_) => (None, selection.to_string(), false),
        },
    }
}

/// Provider-independent `doctor` rows: system prompt, discovered extensions,
/// extra extension dirs, and the final build status.
fn tail_rows(
    out: &mut String,
    system_prompt_override: &Option<(String, &'static str)>,
    cfg_result: &Result<LlmConfig, Box<dyn std::error::Error>>,
) {
    // Base system prompt override (DEX-13): provider-independent, so it prints
    // even when the provider row is unsupported. Value is a char count — the
    // full text would flood doctor — with the same origin
    // `system_prompt_origin` reports at runtime.
    let (text, label) = match system_prompt_override {
        Some((text, label)) => (Some(text.as_str()), *label),
        None => (None, "--system-prompt"),
    };
    let (custom, origin) = system_prompt_origin_with_label(text, label);
    let value = match custom {
        Some(text) => format!("custom ({} chars)", text.chars().count()),
        None => "default".to_string(),
    };
    row(out, "system prompt", &value, origin);
    // Lua extensions: disk discovery + consent state (deterministic — the
    // load-state detail lives in `dex extensions list`).
    let discovered = crate::extensions::discovered_extensions();
    if discovered.is_empty() {
        row(out, "extensions", "none", "cwd/.dex, XDG config dirs");
    } else {
        for (id, version, scope, state) in &discovered {
            row(
                out,
                "extensions",
                &format!("{id} {version}"),
                &format!("{scope}, {state}"),
            );
        }
    }
    // Extra dirs from config/env (origin per the precedence rules).
    let ext_paths = crate::extensions::config_extension_paths();
    if !ext_paths.is_empty() {
        let origin = if std::env::var("DEX_EXTENSIONS_PATHS")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            "DEX_EXTENSIONS_PATHS (environment)"
        } else {
            "extensions.paths (config)"
        };
        row(
            out,
            "ext paths",
            &ext_paths
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", "),
            origin,
        );
    }
    out.push('\n');
    match cfg_result {
        Ok(_) => row(out, "resolve", "OK", "config builds cleanly"),
        Err(e) => row(out, "resolve", "ERROR", &e.to_string()),
    }
}

/// `dex doctor`: print the resolved provider/model/endpoint/protocol/key
/// configuration and where each value came from — `git config
/// --show-origin` for the LLM wiring. Read-only, no network, no daemon;
/// answers "why is dex using X?" without archaeology. Takes the same
/// overrides as `from_env` so flags (`--model`, `--base-url`,
/// `--permission`, `--header`, `--system-prompt` / `--system-prompt-file`
/// resolved text) are reflected, not silently dropped.
pub(crate) fn doctor(
    base_url_override: Option<String>,
    model_override: Option<String>,
    permission_override: Option<PermissionMode>,
    header_overrides: &[String],
    system_prompt_override: Option<(String, &'static str)>,
) -> String {
    let mut out = String::new();
    out.push_str(&format!("dex {}\n\n", env!("CARGO_PKG_VERSION")));

    match config_file_path() {
        Some(path) => {
            let state = if load_config_file().is_some() {
                "valid"
            } else {
                "missing or invalid — ignored (env/defaults still apply)"
            };
            row(&mut out, "config", &path.display().to_string(), state);
        }
        None => row(&mut out, "config", "-", "no config file (defaults only)"),
    }
    match dex_catalog_cache_path() {
        Some(path) => match std::fs::metadata(&path) {
            Ok(meta) => {
                let size = format!("{} KB", meta.len() / 1024);
                row(&mut out, "catalog", &path.display().to_string(), &size);
            }
            Err(_) => row(
                &mut out,
                "catalog",
                &path.display().to_string(),
                "missing — run `dex update --models`",
            ),
        },
        None => row(&mut out, "catalog", "-", "unavailable (no HOME/XDG)"),
    }
    out.push('\n');

    let file = load_config_file();
    let provider_entries = load_provider_entries(&file);
    let known = known_providers(&provider_entries);

    // Selection + origin, mirroring `from_env` precedence:
    // `--model` > `DEX_MODEL` > file `model:` — nothing set anywhere is
    // reported as unconfigured, not silently defaulted.
    let flag_model = model_override.clone().filter(|m| !m.trim().is_empty());
    let flag_base_url = base_url_override.clone().filter(|u| !u.trim().is_empty());
    let selection_resolved = resolve_selection(flag_model.clone(), &file);
    let selection = selection_resolved.as_ref().ok().map(|r| r.value.clone());
    let selection_source = match &selection_resolved {
        Ok(r) => r.origin.to_string(),
        // Short form here: the resolve row prints the full setup guide.
        Err(_) => "UNCONFIGURED — set 'model: <provider>/<model>'".to_string(),
    };
    let (selection_provider, pre_model, bare_pick) =
        doctor_selection_parts(selection.as_deref(), &known);
    // Provider fallback shares `from_env`'s chain (and its deprecation
    // warning, deduped) so the origin row cannot drift from routing.
    let (fallback_provider, fallback_origin) = provider_fallback_with_origin(&file);
    let provider_name = selection_provider.clone().unwrap_or_else(|| {
        provider_without_prefix(
            (fallback_provider.clone(), fallback_origin),
            flag_base_url.as_deref(),
        )
    });
    let provider_source = match selection_provider {
        // A bare pick carries no prefix; the selection origin says it all.
        Some(_) if bare_pick => selection_source.clone(),
        Some(_) => format!("{selection_source} prefix"),
        // Same routing as `from_env`: an explicit --base-url with no
        // selection prefix lands on providers.custom, not the builtin.
        None if provider_name == "custom" => "--base-url (pins endpoint)".to_string(),
        None => fallback_origin.unwrap_or("built-in default").to_string(),
    };
    // The real build, once: rows below take values from it so `doctor`
    // agrees with runtime routing (catalog endpoint moves, prefix
    // stripping, per-model/learned protocol). Origins stay informational.
    let cfg_result = LlmConfig::from_env(
        flag_base_url.clone(),
        flag_model.clone(),
        permission_override,
        header_overrides,
    );
    // The `--base-url` route can land on `custom` before the entry exists;
    // it is a real (user-URL-backed) provider then, not a typo.
    provider_section(
        &mut out,
        &ProviderDoctor {
            file: &file,
            provider_entries: &provider_entries,
            known: &known,
            provider_name: &provider_name,
            provider_source: &provider_source,
            selection_source: &selection_source,
            has_selection: selection.is_some(),
            pre_model: &pre_model,
            cfg_result: &cfg_result,
            flag_base_url: flag_base_url.as_deref(),
            permission_override,
            header_overrides,
            routing_selection: selection.as_deref(),
            routing_selection_source: &selection_source,
            custom_route: selection_provider.is_none() && provider_name == "custom",
        },
    );
    tail_rows(&mut out, &system_prompt_override, &cfg_result);
    out
}
