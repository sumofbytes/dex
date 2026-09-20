//! Extension harness tests, split out of `mod.rs` (which stays facade +
//! net-fetch ceilings). Tests use the process-global manager — see
//! `TEST_GLOBAL_MANAGER_LOCK` below.

use super::manager::ExtensionManager;
use super::*;

/// Serializes tests that load fixtures into the process-global manager:
/// two concurrent fixtures would unload each other's extensions via
/// `reset_for_tests` (and their hooks would interleave mid-test).
pub(crate) static TEST_GLOBAL_MANAGER_LOCK: tokio::sync::Mutex<()> =
    tokio::sync::Mutex::const_new(());

impl ExtensionManager {
    /// Test isolation: the manager is process-global, so a fixture
    /// loaded by one test leaks its hooks into every later turn in the
    /// same process. Drops all engines and caches (prompt appendix
    /// included).
    pub(crate) async fn reset_for_tests(&self) {
        self.engines.write().await.clear();
        *self.cached.write().await = Arc::new([]);
        *self.cached_tokens.write().await = 0;
        *self.cached_costs.write().await = Arc::new([]);
        *self.shadowed.write().await = HashSet::new();
        *self.active.write().expect("active lock") = None;
        *LAST_MODEL.lock().expect("served model lock") = None;
        *LAST_ROUTING_HEADERS.lock().expect("routing headers lock") = BTreeMap::new();
        PROMPT_APPENDIX
            .lock()
            .expect("prompt appendix lock")
            .clear();
        // `dex.state` is an in-process static too: a previous test's
        // extension state (e.g. the web example's override_model) must
        // not leak into the next test's Lua.
        STATE.lock().expect("state lock").clear();
    }
}

/// Restore env vars on drop: the tests below set XDG/DEX vars and must
/// not leak them into other tests in the process (single-threaded runs
/// especially — env is global and never restored otherwise).
struct EnvRestore(Vec<(&'static str, Option<String>)>);
impl EnvRestore {
    fn take(keys: &[&'static str]) -> Self {
        Self(keys.iter().map(|k| (*k, std::env::var(k).ok())).collect())
    }
}
impl Drop for EnvRestore {
    fn drop(&mut self) {
        for (key, value) in &self.0 {
            match value {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
    }
}

#[test]
fn split_names() {
    assert_eq!(split_ext_name("ext__myext__tool"), Some(("myext", "tool")));
    assert_eq!(full_tool_name("myext", "tool"), "ext__myext__tool");
    assert!(split_ext_name("read").is_none());
    assert!(split_ext_name("mcp__a__b").is_none());
    assert!(split_ext_name("ext__a").is_none());
    assert!(split_ext_name("ext____t").is_none());
    assert!(split_ext_name("ext__a__b__c").is_none());
    // Deprecated pre-rename alias still dispatches.
    assert_eq!(split_ext_name("lua__myext__tool"), Some(("myext", "tool")));
    assert!(split_ext_name("lua__a__b__c").is_none());
}

#[test]
fn legacy_alias_names() {
    assert!(is_extension_tool("ext__web__search"));
    assert!(is_extension_tool("lua__web__search"));
    assert!(!is_extension_tool("mcp__srv__tool"));
    assert!(!is_extension_tool("read"));
    assert_eq!(normalize_tool_name("lua__web__search"), "ext__web__search");
    assert_eq!(normalize_tool_name("ext__web__search"), "ext__web__search");
}

#[test]
fn prompt_appendix_composes_sorted_by_id() {
    // Prompt-cache stability: system bytes must not depend on push order.
    // Unique ids + push/remove (no clear/save/restore) so parallel tests
    // sharing the process-global appendix never observe a wiped state.
    remove_prompt_appendix("__prompt_cache_zeta__");
    remove_prompt_appendix("__prompt_cache_alpha__");
    push_prompt_appendix(
        "__prompt_cache_zeta__",
        "second-__prompt_cache__".to_string(),
    );
    push_prompt_appendix(
        "__prompt_cache_alpha__",
        "first-__prompt_cache__".to_string(),
    );
    let composed = prompt_appendix();
    remove_prompt_appendix("__prompt_cache_zeta__");
    remove_prompt_appendix("__prompt_cache_alpha__");
    assert!(
        composed.find("first-__prompt_cache__").unwrap()
            < composed.find("second-__prompt_cache__").unwrap()
    );
}

#[test]
fn active_names_resolve() {
    assert_eq!(resolve_active_name("web", "search"), "ext__web__search");
    assert_eq!(
        resolve_active_name("web", "ext__web__search"),
        "ext__web__search"
    );
    // Legacy alias normalizes so the read-time filter still matches.
    assert_eq!(
        resolve_active_name("web", "lua__web__search"),
        "ext__web__search"
    );
}

/// Fresh temp root for a fixture, unique per process. The system clock
/// is too coarse (50 ns here) for parallel tests: two `#[tokio::test]`s
/// starting together got the same `subsec_nanos` and clobbered each
/// other's `extension.lua`, deleting the dir mid-test.
fn fixture_root(prefix: &str) -> PathBuf {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join(format!("{prefix}-{}-{n}", std::process::id()))
}

/// Write a fixture extension dir; returns the parent temp dir.
pub(crate) fn fixture_ext(manifest: &str, lua: &str) -> PathBuf {
    let root = fixture_root("dex-ext-test");
    let dir = root.join("ext");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("manifest.yaml"), manifest).unwrap();
    std::fs::write(dir.join("extension.lua"), lua).unwrap();
    root
}

pub(crate) fn hello_manifest() -> &'static str {
    r#"
manifest_version: 1
id: hello
version: 0.1.0
capabilities: [tools, workspace.read]
tools:
  - name: greet
    description: Say hi.
    parameters: {"type": "object", "properties": {"who": {"type": "string"}}}
"#
}

pub(crate) fn hello_lua() -> &'static str {
    r#"
return function(dex)
  dex.tools.register({ name = "greet", execute = function(ctx, args)
    return "hi " .. (args.who or "there")
  end })
end
"#
}

#[tokio::test]
async fn loads_tool_into_cache() {
    let root = fixture_ext(hello_manifest(), hello_lua());
    let mgr = ExtensionManager::fresh();
    mgr.refresh_with(std::slice::from_ref(&root)).await;
    let tools = mgr.cached.try_read().unwrap().clone();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].function.name, "ext__hello__greet");
    std::fs::remove_dir_all(&root).ok();
}

#[tokio::test]
async fn skips_bad_extension_whole() {
    let root = fixture_ext("not: [valid", hello_lua());
    let mgr = ExtensionManager::fresh();
    mgr.refresh_with(std::slice::from_ref(&root)).await;
    assert!(mgr.cached.try_read().unwrap().is_empty());
    std::fs::remove_dir_all(&root).ok();
}

#[tokio::test]
async fn sandbox_blocks_stdlib_and_bombs() {
    let lua = r#"
return function(dex)
  dex.tools.register({ name = "evil", execute = function(ctx, args)
    return "unreached"
  end })
  -- load-time probes: all must be nil
  assert(os == nil, "os visible");
  assert(io == nil, "io visible");
  assert(package == nil, "package visible");
  assert(debug == nil, "debug visible");
  assert(require == nil, "require visible");
  assert(load == nil, "load visible");
  assert(dofile == nil, "dofile visible");
end
"#;
    let manifest = hello_manifest()
        .replace("greet", "evil")
        .replace("Say hi.", "E.");
    let root = fixture_ext(&manifest, lua);
    let mgr = ExtensionManager::fresh();
    mgr.refresh_with(std::slice::from_ref(&root)).await;
    assert_eq!(mgr.cached.try_read().unwrap().len(), 1);
    // Runaway chunk aborted by the deadline, not the test harness.
    let lua_bomb = r#"
return function(dex)
  dex.tools.register({ name = "evil", execute = function(ctx, args)
    local i = 0
    while true do i = i + 1 end
    return "unreached"
  end })
end
"#;
    let manifest2 = hello_manifest()
        .replace("id: hello", "id: bomb")
        .replace("greet", "evil")
        .replace("Say hi.", "E.")
        .replace("    parameters:", "    timeout: 2\n    parameters:");
    let root2 = fixture_ext(&manifest2, lua_bomb);
    let mgr2 = ExtensionManager::fresh();
    mgr2.refresh_with(std::slice::from_ref(&root2)).await;
    let policy = crate::tools::Policy::trusted();
    let host = HostCtx {
        cancel: &crate::agent::state::GlobalCancellation,
        policy: &policy,
        filter: None,
    };
    let err = mgr2
        .call("ext__bomb__evil", &serde_json::Map::new(), &host)
        .await
        .unwrap_err();
    assert!(
        err.contains("deadline") || err.contains("timed out"),
        "got: {err}"
    );
    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&root2).ok();
}

#[tokio::test]
async fn lua_tools_are_shell_gated_and_filtered() {
    use crate::protocol::PermissionMode;
    use crate::tools::{Policy, ToolFilter};
    // Static metadata arm: Shell requirement, like mcp__.
    let meta = crate::tools::metadata("ext__anything__tool").unwrap();
    assert_eq!(meta.permission, crate::tools::PermissionRequirement::Shell);

    let args = serde_json::Map::new();
    // GLOBAL is empty in tests: Trusted passes the gates and fails at
    // dispatch (unknown tool) — proving the gate passed, not denied.
    let err = crate::tools::execute(
        "ext__noext__notool",
        &args,
        &crate::agent::state::GlobalCancellation,
        &Policy::trusted(),
        None,
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("unknown tool"), "got: {err}");
    // ReadOnly denies before dispatch.
    let console = crate::runtime::console::Console::none();
    let err = crate::tools::execute(
        "ext__noext__notool",
        &args,
        &crate::agent::state::GlobalCancellation,
        &Policy::turn(PermissionMode::ReadOnly, &console),
        None,
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("denied"), "got: {err}");
    // Child allowlists reject ext__ before any gate or dispatch.
    let filter = ToolFilter::new("explorer", ["read"]);
    let err = crate::tools::execute(
        "ext__noext__notool",
        &args,
        &crate::agent::state::GlobalCancellation,
        &Policy::trusted(),
        Some(&filter),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("allowlist"), "got: {err}");
}

/// Write several fixture extensions under one parent; each item is
/// (id, manifest, lua). Returns the parent dir for `refresh_with`.
pub(crate) fn fixture_exts(items: &[(&str, &str, &str)]) -> PathBuf {
    let root = fixture_root("dex-exts-test");
    for (id, manifest, lua) in items {
        let dir = root.join(id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("manifest.yaml"), manifest).unwrap();
        std::fs::write(dir.join("extension.lua"), lua).unwrap();
    }
    root
}

fn hook_manifest(id: &str, strict: bool) -> String {
    format!("manifest_version: 1\nid: {id}\nversion: 0.1.0\ncapabilities: []\nstrict: {strict}\n")
}

fn test_host() -> (
    crate::agent::state::GlobalCancellation,
    crate::tools::Policy,
) {
    (
        crate::agent::state::GlobalCancellation,
        crate::tools::Policy::trusted(),
    )
}

#[tokio::test]
async fn before_hooks_mutate_in_load_order_and_deny() {
    let aaa = hook_manifest("aaa-hook", false);
    let zzz = hook_manifest("zzz-hook", false);
    let root = fixture_exts(&[
        (
            "aaa",
            aaa.as_str(),
            r#"return function(dex)
  dex.events.on("tool.before", function(ctx, ev)
    if ev.tool == "bash" then ev.args.command = ev.args.command .. "-a" end
  end)
end
"#,
        ),
        (
            "zzz",
            zzz.as_str(),
            r#"return function(dex)
  dex.events.on("tool.before", function(ctx, ev)
    if ev.tool == "bash" and ev.args.command:find("bad") then
      return { deny = true, reason = "no bad" }
    end
    if ev.tool == "bash" then ev.args.command = ev.args.command .. "-z" end
  end)
end
"#,
        ),
    ]);
    let mgr = ExtensionManager::fresh();
    mgr.refresh_with(std::slice::from_ref(&root)).await;
    let (cancel, policy) = test_host();
    let host = HostCtx {
        cancel: &cancel,
        policy: &policy,
        filter: None,
    };
    let mut args = serde_json::Map::new();
    args.insert(
        "command".to_string(),
        serde_json::Value::String("echo ok".to_string()),
    );
    match mgr.apply_before_hooks("bash", &args, &host).await {
        BeforeOutcome::Proceed { args, mutated_by } => {
            assert_eq!(args["command"], "echo ok-a-z");
            assert_eq!(
                mutated_by,
                vec!["aaa-hook".to_string(), "zzz-hook".to_string()]
            );
        }
        BeforeOutcome::Denied { .. } => panic!("should proceed"),
    }
    args.insert(
        "command".to_string(),
        serde_json::Value::String("bad".to_string()),
    );
    match mgr.apply_before_hooks("bash", &args, &host).await {
        BeforeOutcome::Denied { by, reason } => {
            assert_eq!(by, "zzz-hook");
            assert_eq!(reason, "no bad");
        }
        BeforeOutcome::Proceed { .. } => panic!("should deny"),
    }
    std::fs::remove_dir_all(&root).ok();
}

#[tokio::test]
async fn before_hook_errors_fail_open_unless_strict() {
    let lua = r#"return function(dex)
  dex.events.on("tool.before", function(ctx, ev)
    error("boom")
  end)
end
"#;
    for (id, strict, denied) in [("loose", false, false), ("tight", true, true)] {
        let manifest = hook_manifest(id, strict);
        let root = fixture_exts(&[(id, manifest.as_str(), lua)]);
        let mgr = ExtensionManager::fresh();
        mgr.refresh_with(std::slice::from_ref(&root)).await;
        let (cancel, policy) = test_host();
        let host = HostCtx {
            cancel: &cancel,
            policy: &policy,
            filter: None,
        };
        let mut args = serde_json::Map::new();
        args.insert(
            "command".to_string(),
            serde_json::Value::String("x".to_string()),
        );
        match mgr.apply_before_hooks("bash", &args, &host).await {
            BeforeOutcome::Denied { reason, .. } => {
                assert!(denied, "fail-open hook denied: {reason}");
                assert!(reason.contains("strict"), "got: {reason}");
            }
            BeforeOutcome::Proceed { args: out, .. } => {
                assert!(!denied, "strict hook proceeded");
                assert_eq!(out["command"], "x");
            }
        }
        std::fs::remove_dir_all(&root).ok();
    }
}

#[tokio::test]
async fn reload_unloads_vanished_extensions_and_their_prompt_appendix() {
    // The appendix is process-global: serialize against the tests that
    // `reset_for_tests` (which clears it) like every other asserter.
    let _ext = TEST_GLOBAL_MANAGER_LOCK.lock().await;
    let manifest = "manifest_version: 1\nid: fleeting\nversion: 0.1.0\ncapabilities: []\n";
    let root = fixture_ext(
        manifest,
        "return function(dex)\n  dex.prompt.append(\"hello\")\nend\n",
    );
    let mgr = ExtensionManager::fresh();
    mgr.refresh_with(std::slice::from_ref(&root)).await;
    assert_eq!(mgr.engines.read().await.len(), 1);
    assert!(prompt_appendix().contains("hello"));
    // The reload reconcile drops what is no longer discovered (here:
    // nothing) and its prompt appendix with it.
    mgr.unload_missing(&std::collections::HashSet::new()).await;
    assert!(mgr.engines.read().await.is_empty());
    assert!(!prompt_appendix().contains("hello"));
    std::fs::remove_dir_all(&root).ok();
}

#[tokio::test]
async fn memory_blowup_fails_the_load_not_the_process() {
    // The per-VM memory ceiling turns a 512 MiB `string.rep` into a Lua
    // error caught at load — not an OOM kill of the whole process.
    let manifest = "manifest_version: 1\nid: hog\nversion: 0.1.0\ncapabilities: []\n";
    let root = fixture_ext(
        manifest,
        "return function(dex)\n  local s = string.rep(\"x\", 512 * 1024 * 1024)\n  return s\nend\n",
    );
    let mgr = ExtensionManager::fresh();
    mgr.refresh_with(std::slice::from_ref(&root)).await;
    assert!(
        mgr.engines.read().await.is_empty(),
        "over-limit setup must fail the whole extension"
    );
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn consent_and_remove_reject_non_segment_ids() {
    let _lock = crate::session::TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _env = EnvRestore::take(&["XDG_DATA_HOME"]);
    // Traversal shapes must never reach the marker or data dirs.
    assert!(set_enabled("../../tmp/evil", true).is_err());
    assert!(set_enabled("", true).is_err());
    assert!(set_enabled("Bad Id!", true).is_err());
    assert!(remove("../../some-other-dir").is_err());
    assert!(remove("ok_id-but/with-slash").is_err());
    // Nothing was written.
    assert!(!data_extensions_dir().join("enabled").exists());
}

#[tokio::test]
async fn bad_hook_registration_skips_the_extension_whole() {
    // Unknown event fails legibly at load.
    let unknown_event = (
        "ev",
        &hook_manifest("ev", false),
        r#"return function(dex)
  dex.events.on("context", function(ctx, ev) end)
end
"#,
    );
    // Override without the capability.
    let no_cap = (
        "nocap",
        "manifest_version: 1\nid: nocap\nversion: 0.1.0\ncapabilities: []\n",
        r#"return function(dex)
  dex.tools.register({ name = "read", override = true, execute = function(ctx, args)
    return "x"
  end })
end
"#,
    );
    // Override of a nonexistent tool passes the worker but fails the
    // manager's built-in check.
    let no_target = (
        "notarget",
        "manifest_version: 1\nid: notarget\nversion: 0.1.0\ncapabilities: [tools.override]\n",
        r#"return function(dex)
  dex.tools.register({ name = "nope", override = true, execute = function(ctx, args)
    return "x"
  end })
end
"#,
    );
    let root = fixture_exts(&[
        (unknown_event.0, unknown_event.1, unknown_event.2),
        (no_cap.0, no_cap.1, no_cap.2),
        (no_target.0, no_target.1, no_target.2),
    ]);
    let mgr = ExtensionManager::fresh();
    mgr.refresh_with(std::slice::from_ref(&root)).await;
    assert!(mgr.cached.try_read().unwrap().is_empty());
    assert!(mgr.engines.read().await.is_empty());
    std::fs::remove_dir_all(&root).ok();
}

#[tokio::test]
async fn ensure_loaded_boots_one_extension_lazily() {
    // §26: no refresh — the first addressed load boots just this
    // extension; repeats are no-ops; unknown ids error without parking.
    let root = fixture_ext(hello_manifest(), hello_lua());
    let mgr = ExtensionManager::fresh();
    let m = crate::extensions::manifest::parse_manifest(hello_manifest()).unwrap();
    mgr.ensure_loaded_found("hello", vec![(root.join("ext"), m)])
        .await
        .unwrap();
    assert!(mgr.engines.read().await.contains_key("hello"));
    mgr.ensure_loaded_found("hello", vec![]).await.unwrap();
    assert!(mgr.ensure_loaded_found("nope", vec![]).await.is_err());
    assert!(!mgr.engines.read().await.contains_key("nope"));
    std::fs::remove_dir_all(&root).ok();
}

#[tokio::test]
async fn call_original_outside_a_shadow_is_an_error() {
    let manifest = r#"
manifest_version: 1
id: noshadow
version: 0.1.0
capabilities: [tools]
tools:
  - name: oops
    description: O.
    parameters: {"type": "object"}
"#;
    let lua = r#"return function(dex)
  dex.tools.register({ name = "oops", execute = function(ctx, args)
    return dex.tools.call_original(ctx, args)
  end })
end
"#;
    let root = fixture_ext(manifest, lua);
    let mgr = ExtensionManager::fresh();
    mgr.refresh_with(std::slice::from_ref(&root)).await;
    let (cancel, policy) = test_host();
    let host = HostCtx {
        cancel: &cancel,
        policy: &policy,
        filter: None,
    };
    let err = mgr
        .call("ext__noshadow__oops", &serde_json::Map::new(), &host)
        .await
        .unwrap_err();
    assert!(err.contains("outside a shadow"), "got: {err}");
    std::fs::remove_dir_all(&root).ok();
}

#[tokio::test]
async fn shadow_wraps_ls_and_composes_via_call_original() {
    // Passthrough unless the magic flag is set, so concurrent tests
    // using `ls` normally never observe the shadow.
    let manifest =
        "manifest_version: 1\nid: shadowls\nversion: 0.1.0\ncapabilities: [tools.override]\n";
    let lua = r#"return function(dex)
  dex.tools.register({ name = "ls", override = true, execute = function(ctx, args)
    local out = dex.tools.call_original(ctx, args)
    if args.magic == true then return "wrapped:" .. out end
    return out
  end })
end
"#;
    let _lock = TEST_GLOBAL_MANAGER_LOCK.lock().await;
    let root = fixture_exts(&[("shadowls", manifest, lua)]);
    global_manager()
        .refresh_with(std::slice::from_ref(&root))
        .await;
    assert!(crate::tools::metadata("ls").is_some());
    assert_eq!(
        crate::tools::metadata("ls").unwrap().permission,
        crate::tools::PermissionRequirement::Shell
    );
    // Workspace-confined: list the test process's cwd, which is inside.
    let mut args = serde_json::Map::new();
    args.insert(
        "path".to_string(),
        serde_json::Value::String(".".to_string()),
    );
    let (cancel, policy) = test_host();
    let plain = crate::tools::execute("ls", &args, &cancel, &policy, None)
        .await
        .unwrap();
    assert!(!plain.starts_with("wrapped:"), "got: {plain}");
    args.insert("magic".to_string(), serde_json::Value::Bool(true));
    // The magic flag is not part of the ls schema: strip it for the
    // passthrough comparison by re-running plain below.
    let wrapped = crate::tools::execute("ls", &args, &cancel, &policy, None).await;
    // ls rejects unknown args — the shadow still composed (the error or
    // the wrap proves the shadow ran, not the built-in alone).
    match wrapped {
        Ok(out) => assert_eq!(out, format!("wrapped:{plain}"), "got: {out}"),
        Err(e) => panic!("shadow should pass args through: {e}"),
    }
    std::fs::remove_dir_all(&root).ok();
    // Drop the fixture: the `ls` shadow must not intercept every later
    // `ls` call (and re-gate it) in this test process.
    global_manager().reset_for_tests().await;
}

#[test]
fn config_paths_parse_and_layer_env_over_file() {
    let yaml: serde_yaml::Value =
        serde_yaml::from_str("extensions:\n  paths:\n    - /a/b\n    - /c/d\n").unwrap();
    let paths = parse_config_paths(&yaml);
    assert_eq!(paths, vec![PathBuf::from("/a/b"), PathBuf::from("/c/d")]);
    // Missing/empty shapes are fine.
    assert!(parse_config_paths(&serde_yaml::Value::Null).is_empty());
    let yaml: serde_yaml::Value = serde_yaml::from_str("extensions: {}").unwrap();
    assert!(parse_config_paths(&yaml).is_empty());
    // Non-string entries are skipped, not fatal.
    let yaml: serde_yaml::Value =
        serde_yaml::from_str("extensions:\n  paths:\n    - /ok\n    - 42\n").unwrap();
    assert_eq!(parse_config_paths(&yaml), vec![PathBuf::from("/ok")]);
}

#[test]
fn config_paths_reach_discovery_as_user_scope() {
    let _lock = crate::session::TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _env = EnvRestore::take(&["XDG_CONFIG_HOME", "XDG_DATA_HOME", "DEX_EXTENSIONS_PATHS"]);
    let root = std::env::temp_dir().join(format!("dex-ext-cfgpaths-{}", std::process::id()));
    std::env::set_var("XDG_CONFIG_HOME", root.join("config"));
    std::env::set_var("XDG_DATA_HOME", root.join("data"));
    let cfg_dir = root.join("config/dex");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    std::fs::write(
        cfg_dir.join("config.yaml"),
        "extensions:\n  paths:\n    - /tmp/dex-ext-cfgpaths-extra\n",
    )
    .unwrap();
    std::fs::create_dir_all("/tmp/dex-ext-cfgpaths-extra/myext").unwrap();
    std::fs::write(
        "/tmp/dex-ext-cfgpaths-extra/myext/manifest.yaml",
        "manifest_version: 1\nid: cfgext\nversion: 0.1.0\ncapabilities: []\n",
    )
    .unwrap();
    let dirs = scoped_extension_dirs();
    assert!(dirs
        .iter()
        .any(|(d, scope)| d.ends_with("dex-ext-cfgpaths-extra") && *scope == Scope::User));
    let found = discovered_extensions();
    assert!(found
        .iter()
        .any(|(id, _, scope, _)| id == "cfgext" && *scope == "user"));
    // Env wins over the file.
    std::env::set_var("DEX_EXTENSIONS_PATHS", "/tmp/dex-ext-cfgpaths-env");
    let dirs = scoped_extension_dirs();
    assert!(dirs
        .iter()
        .any(|(d, _)| d.ends_with("dex-ext-cfgpaths-env")));
    assert!(!dirs
        .iter()
        .any(|(d, _)| d.ends_with("dex-ext-cfgpaths-extra")));
    std::env::remove_var("DEX_EXTENSIONS_PATHS");
    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all("/tmp/dex-ext-cfgpaths-extra").ok();
}

#[test]
fn markers_gate_scopes_and_doctor_discovery() {
    let _lock = crate::session::TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _env = EnvRestore::take(&["XDG_CONFIG_HOME", "XDG_DATA_HOME"]);
    let root = std::env::temp_dir().join(format!("dex-ext-markers-{}", std::process::id()));
    std::env::set_var("XDG_DATA_HOME", &root);
    std::env::set_var("XDG_CONFIG_HOME", root.join("config"));
    // User-scope extension: enabled unless disabled.
    let user_ext = crate::extensions::user_extensions_dir().join("userext");
    std::fs::create_dir_all(&user_ext).unwrap();
    std::fs::write(
        user_ext.join("manifest.yaml"),
        "manifest_version: 1\nid: userext\nversion: 0.1.0\ncapabilities: []\n",
    )
    .unwrap();
    // Project-scope extension: needs the consent marker. Discovered
    // from the test process's cwd (the crate root) — created here and
    // removed afterwards; never committed.
    let proj_ext = std::path::PathBuf::from(".dex/extensions").join("projext");
    std::fs::create_dir_all(&proj_ext).unwrap();
    std::fs::write(
        proj_ext.join("manifest.yaml"),
        "manifest_version: 1\nid: projext\nversion: 0.2.0\ncapabilities: []\n",
    )
    .unwrap();
    let found = discovered_extensions();
    let ids: Vec<&str> = found.iter().map(|(id, ..)| id.as_str()).collect();
    assert_eq!(ids, vec!["projext", "userext"]);
    let proj = found.iter().find(|(id, ..)| id == "projext").unwrap();
    assert_eq!(proj.3, "not enabled (trust gate)");
    // Consent flips the project state; disable flips the user one.
    set_enabled("projext", true).unwrap();
    set_enabled("userext", false).unwrap();
    let found = discovered_extensions();
    assert_eq!(
        found.iter().find(|(id, ..)| id == "projext").unwrap().3,
        "enabled"
    );
    assert_eq!(
        found.iter().find(|(id, ..)| id == "userext").unwrap().3,
        "disabled"
    );
    // Cleanup.
    set_enabled("projext", false).ok();
    set_enabled("userext", true).ok();
    std::fs::remove_dir_all(".dex").ok();
    std::fs::remove_dir_all(&root).ok();
}

#[tokio::test]
async fn lifecycle_events_fire_and_compact_hooks_merge() {
    let m = hook_manifest("lifecycle", false);
    let root = fixture_exts(&[(
        "lifecycle",
        m.as_str(),
        r#"return function(dex)
  dex.events.on("turn.start", function(ctx, ev)
    dex.log.info("turn-start-seen")
  end)
  dex.events.on("session.before_compact", function(ctx, ev)
    if ev.emergency then return { cancel = true } end
    return { instructions = "keep the plan verbatim" }
  end)
end
"#,
    )]);
    let mgr = ExtensionManager::fresh();
    mgr.refresh_with(std::slice::from_ref(&root)).await;
    let (cancel, policy) = test_host();
    let host = HostCtx {
        cancel: &cancel,
        policy: &policy,
        filter: None,
    };
    // Fire-and-forget: no panic, no result.
    mgr.fire_event("turn.start", serde_json::json!({}), &host)
        .await;
    // Merge semantics: normal run -> instructions; emergency -> cancel
    // wins (merged across handlers, here a single one).
    let action = mgr.apply_before_compact(false, 10, &host).await;
    assert!(!action.cancel);
    assert_eq!(
        action.instructions,
        vec!["keep the plan verbatim".to_string()]
    );
    let action = mgr.apply_before_compact(true, 10, &host).await;
    assert!(action.cancel);
    std::fs::remove_dir_all(&root).ok();
}

#[tokio::test]
async fn compact_summary_replacement_and_tools_active_slice() {
    let m = hook_manifest("compactor", false);
    let tool_manifest = "manifest_version: 1\nid: compactor\nversion: 0.1.0\ncapabilities: [tools, tools.override]\ntools:\n  - name: probe\n    description: probe\n    parameters: {\"type\":\"object\",\"properties\":{}}\n";
    let root = fixture_exts(&[(
        "compactor",
        tool_manifest,
        r#"return function(dex)
  dex.events.on("session.before_compact", function(ctx, ev)
    return { summary = "HOOK SUMMARY" }
  end)
  dex.tools.register({ name = "probe", execute = function(ctx, args) return "probe" end })
end
"#,
    )]);
    let _ = m;
    let mgr = ExtensionManager::fresh();
    mgr.refresh_with(std::slice::from_ref(&root)).await;
    let (cancel, policy) = test_host();
    let host = HostCtx {
        cancel: &cancel,
        policy: &policy,
        filter: None,
    };
    let action = mgr.apply_before_compact(false, 10, &host).await;
    assert_eq!(action.summary.as_deref(), Some("HOOK SUMMARY"));
    // set_active slice: full name filtering, unknown names dropped.
    mgr.set_active(vec![
        "ext__compactor__probe".to_string(),
        "ext__compactor__ghost".to_string(),
    ])
    .await;
    let names: Vec<String> = mgr
        .active_cached()
        .await
        .iter()
        .map(|d| d.function.name.clone())
        .collect();
    assert_eq!(names, vec!["ext__compactor__probe".to_string()]);
    // Empty list = no extension tools.
    mgr.set_active(vec![]).await;
    assert_eq!(mgr.active_cached().await.len(), 0);
    std::fs::remove_dir_all(&root).ok();
}

#[tokio::test]
async fn after_hooks_rewrite_content_and_flip_ok_in_load_order() {
    let aaa = hook_manifest("aaa-after", false);
    let zzz = hook_manifest("zzz-after", false);
    let root = fixture_exts(&[
        (
            "aaa",
            aaa.as_str(),
            r#"return function(dex)
  dex.events.on("tool.after", function(ctx, ev)
    ev.content = ev.content .. "-a"
  end)
end
"#,
        ),
        (
            "zzz",
            zzz.as_str(),
            r#"return function(dex)
  dex.events.on("tool.after", function(ctx, ev)
    if ev.content:find("flip") then ev.is_error = true end
    ev.content = ev.content .. "-z"
  end)
end
"#,
        ),
    ]);
    let mgr = ExtensionManager::fresh();
    mgr.refresh_with(std::slice::from_ref(&root)).await;
    let (cancel, policy) = test_host();
    let host = HostCtx {
        cancel: &cancel,
        policy: &policy,
        filter: None,
    };
    let args = serde_json::Map::new();
    let out = mgr
        .apply_after_hooks("read", &args, "body", true, &host)
        .await;
    assert_eq!(out.text, "body-a-z");
    assert!(out.ok);
    let out = mgr
        .apply_after_hooks("read", &args, "flip me", true, &host)
        .await;
    assert_eq!(out.text, "flip me-a-z");
    assert!(!out.ok);
    std::fs::remove_dir_all(&root).ok();
}

#[tokio::test]
async fn hook_mutation_reaches_the_gates() {
    // The gate check (`session_approved`) runs on the rewritten args:
    // pre-approving ONLY the rewritten call lets the original through.
    let manifest = hook_manifest("gatehook", false);
    let lua = r#"return function(dex)
  dex.events.on("tool.before", function(ctx, ev)
    if ev.tool == "bash" and ev.args.command == "echo original" then
      ev.args.command = "echo rewritten"
    end
  end)
end
"#;
    let _lock = TEST_GLOBAL_MANAGER_LOCK.lock().await;
    let root = fixture_exts(&[("gatehook", manifest.as_str(), lua)]);
    global_manager()
        .refresh_with(std::slice::from_ref(&root))
        .await;
    let console = crate::runtime::console::Console::none();
    console.record_session_approval("bash", r#"{"command":"echo rewritten"}"#);
    let policy = crate::tools::Policy::turn(crate::protocol::PermissionMode::Ask, &console);
    let mut args = serde_json::Map::new();
    args.insert(
        "command".to_string(),
        serde_json::Value::String("echo original".to_string()),
    );
    let out = crate::tools::execute(
        "bash",
        &args,
        &crate::agent::state::GlobalCancellation,
        &policy,
        None,
    )
    .await
    .unwrap();
    assert!(out.contains("rewritten"), "got: {out}");
    std::fs::remove_dir_all(&root).ok();
    // Drop the fixture: the `echo original` rewriter must not see later
    // tests' bash calls.
    global_manager().reset_for_tests().await;
}

#[tokio::test]
async fn hook_deny_is_attributed_and_never_executes() {
    let manifest = hook_manifest("denyhook", false);
    let lua = r#"return function(dex)
  dex.events.on("tool.before", function(ctx, ev)
    if ev.tool == "bash" and ev.args.command:find("rm %-rf") then
      return { deny = true, reason = "dangerous command" }
    end
  end)
end
"#;
    let _lock = TEST_GLOBAL_MANAGER_LOCK.lock().await;
    let root = fixture_exts(&[("denyhook", manifest.as_str(), lua)]);
    global_manager()
        .refresh_with(std::slice::from_ref(&root))
        .await;
    let mut args = serde_json::Map::new();
    args.insert(
        "command".to_string(),
        serde_json::Value::String("rm -rf /tmp/dex-never".to_string()),
    );
    let (cancel, policy) = test_host();
    let err = crate::tools::execute("bash", &args, &cancel, &policy, None)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("permission denied"), "got: {err}");
    assert!(err.contains("denyhook"), "got: {err}");
    assert!(err.contains("dangerous command"), "got: {err}");
    std::fs::remove_dir_all(&root).ok();
    // Drop the fixture: the `rm -rf` denier must not see later tests'
    // bash calls.
    global_manager().reset_for_tests().await;
}

#[tokio::test]
async fn call_roundtrip_and_unknown_tool() {
    let root = fixture_ext(hello_manifest(), hello_lua());
    let mgr = ExtensionManager::fresh();
    mgr.refresh_with(std::slice::from_ref(&root)).await;
    let mut args = serde_json::Map::new();
    args.insert(
        "who".to_string(),
        serde_json::Value::String("bob".to_string()),
    );
    let policy = crate::tools::Policy::trusted();
    let host = HostCtx {
        cancel: &crate::agent::state::GlobalCancellation,
        policy: &policy,
        filter: None,
    };
    let out = mgr.call("ext__hello__greet", &args, &host).await.unwrap();
    assert_eq!(out, "hi bob");
    assert!(mgr.call("ext__hello__nope", &args, &host).await.is_err());
    assert!(mgr.call("read", &args, &host).await.is_err());
    std::fs::remove_dir_all(&root).ok();
}

/// A `LlmConfig` for the `model_select` tests (same shape as the turn
/// tests' mock config — no network, no catalog).
fn model_select_config(
    provider: &str,
    model: &str,
    base_url: &str,
) -> crate::llm::config::LlmConfig {
    crate::llm::config::LlmConfig {
        provider: crate::protocol::Provider::Generic(provider.to_string()),
        api_key: String::new(),
        base_url: base_url.to_string(),
        model: model.to_string(),
        available_models: vec![model.to_string()],
        endpoints: Default::default(),
        api: crate::protocol::ApiProtocol::Responses,
        account_id: None,
        thinking_effort: None,
        context_window: 128_000,
        reserve_tokens: 16_384,
        keep_recent_tokens: 20_000,
        permission: crate::protocol::PermissionMode::Trusted,
        verify_command: None,
        extra_headers: Default::default(),
        global_headers: Default::default(),
        connect_timeout_secs: 10,
        request_timeout_secs: 300,
        provider_entries: Default::default(),
        provider_headers: Default::default(),
        api_pinned: false,
    }
}

/// `model_select` fires once per `provider/model` change (first turn
/// always fires), records the served snapshot, and stays fail-open when
/// a handler errors. Holds both global locks: the snapshot static is
/// process-wide and every `process_turn` records into it.
#[allow(clippy::await_holding_lock)] // single-threaded runtime; env must stay redirected
#[tokio::test]
async fn model_select_fires_once_per_change_and_fails_open() {
    // Daemon e2e tests run real turns (which record `LAST_MODEL`) under
    // TEST_SESSIONS_ENV_LOCK; take it first (consistent order) so a
    // concurrent daemon turn can't slip a snapshot in between
    // `reset_for_tests()` and the first fire (previous != nil).
    let _sessions = crate::daemon::state::lock_map(&crate::session::TEST_SESSIONS_ENV_LOCK);
    let _turn = crate::agent::turn_loop::tests::TEST_TURN_ENV_LOCK
        .lock()
        .await;
    let _ext = TEST_GLOBAL_MANAGER_LOCK.lock().await;
    let mgr = global_manager();
    mgr.reset_for_tests().await;
    let manifest_ok = "manifest_version: 1\nid: sel-ok\nversion: 0.1.0\ncapabilities: []\n";
    let manifest_bad = "manifest_version: 1\nid: sel-bad\nversion: 0.1.0\ncapabilities: []\n";
    let root = fixture_exts(&[
        (
            "sel-ok",
            manifest_ok,
            r#"return function(dex)
  dex.events.on("model_select", function(ctx, ev)
    dex.prompt.append(ev.model .. "|" .. tostring(ev.previous) .. ";")
  end)
end
"#,
        ),
        (
            "sel-bad",
            manifest_bad,
            r#"return function(dex)
  dex.events.on("model_select", function(ctx, ev)
    error("boom")
  end)
end
"#,
        ),
    ]);
    mgr.refresh_with(std::slice::from_ref(&root)).await;
    let policy = crate::tools::Policy::trusted();
    let cancel = crate::agent::state::GlobalCancellation;
    let first = model_select_config("myprov", "m-7", "https://myprov.example/v1");
    fire_model_select_if_changed(&first, &cancel, &policy, None).await;
    // Same id again: no second fire (deduped by change detection).
    fire_model_select_if_changed(&first, &cancel, &policy, None).await;
    let second = model_select_config("myprov", "m-8", "https://myprov.example/v1");
    fire_model_select_if_changed(&second, &cancel, &policy, None).await;
    let appendix = prompt_appendix();
    assert_eq!(appendix, "myprov/m-7|nil;myprov/m-8|myprov/m-7;");
    // The served snapshot follows the last turn (even with a failing
    // subscriber in the mix — fail-open).
    assert_eq!(
        served_model_snapshot().map(|s| s.id()),
        Some("myprov/m-8".to_string())
    );
    mgr.reset_for_tests().await;
    std::fs::remove_dir_all(&root).ok();
}

/// Without subscribers the event is a snapshot record only: no fire,
/// no failure, current model still served.
#[allow(clippy::await_holding_lock)] // single-threaded runtime; env must stay redirected
#[tokio::test]
async fn model_select_is_zero_cost_without_subscribers() {
    // Daemon e2e tests run real turns (which record `LAST_MODEL`) under
    // TEST_SESSIONS_ENV_LOCK; take it first (consistent order) so a
    // concurrent daemon turn can't slip a snapshot in between
    // `reset_for_tests()` and the first fire (previous != nil).
    let _sessions = crate::daemon::state::lock_map(&crate::session::TEST_SESSIONS_ENV_LOCK);
    let _turn = crate::agent::turn_loop::tests::TEST_TURN_ENV_LOCK
        .lock()
        .await;
    let _ext = TEST_GLOBAL_MANAGER_LOCK.lock().await;
    let mgr = global_manager();
    mgr.reset_for_tests().await;
    assert!(!has_event_handlers("model_select"));
    let policy = crate::tools::Policy::trusted();
    let cancel = crate::agent::state::GlobalCancellation;
    let cfg = model_select_config("myprov", "m-7", "https://myprov.example/v1");
    fire_model_select_if_changed(&cfg, &cancel, &policy, None).await;
    assert_eq!(prompt_appendix(), "");
    assert_eq!(
        served_model_snapshot().map(|s| s.id()),
        Some("myprov/m-7".to_string())
    );
    mgr.reset_for_tests().await;
}

/// The turn records dex's own routing-affinity headers for `dex.net.fetch`:
/// only the `x-opencode-*` pair (canonical lowercase), never user headers.
#[allow(clippy::await_holding_lock)] // single-threaded runtime; env must stay redirected
#[tokio::test]
async fn model_select_records_routing_headers() {
    // Daemon e2e tests run real turns (which record `LAST_MODEL`) under
    // TEST_SESSIONS_ENV_LOCK; take it first (consistent order) so a
    // concurrent daemon turn can't slip a snapshot in between
    // `reset_for_tests()` and the first fire (previous != nil).
    let _sessions = crate::daemon::state::lock_map(&crate::session::TEST_SESSIONS_ENV_LOCK);
    let _turn = crate::agent::turn_loop::tests::TEST_TURN_ENV_LOCK
        .lock()
        .await;
    let _ext = TEST_GLOBAL_MANAGER_LOCK.lock().await;
    let mgr = global_manager();
    mgr.reset_for_tests().await;
    let policy = crate::tools::Policy::trusted();
    let cancel = crate::agent::state::GlobalCancellation;
    let mut cfg = model_select_config("myprov", "m-7", "https://myprov.example/v1");
    cfg.extra_headers
        .insert("X-Opencode-Session".to_string(), "sess-1".to_string());
    cfg.extra_headers
        .insert("x-opencode-client".to_string(), "dex".to_string());
    cfg.extra_headers
        .insert("X-Custom".to_string(), "mine".to_string());
    fire_model_select_if_changed(&cfg, &cancel, &policy, None).await;
    let routing = LAST_ROUTING_HEADERS
        .lock()
        .expect("routing headers lock")
        .clone();
    assert_eq!(
        routing.get("x-opencode-session").map(String::as_str),
        Some("sess-1")
    );
    assert_eq!(
        routing.get("x-opencode-client").map(String::as_str),
        Some("dex")
    );
    assert!(!routing.contains_key("X-Custom"));
    // A turn without affinity headers clears the record (no stale id
    // routes the next turn's extension calls).
    let plain = model_select_config("myprov", "m-7", "https://myprov.example/v1");
    fire_model_select_if_changed(&plain, &cancel, &policy, None).await;
    assert!(LAST_ROUTING_HEADERS
        .lock()
        .expect("routing headers lock")
        .is_empty());
    mgr.reset_for_tests().await;
}

/// Harvest keeps just the affinity pair, case-insensitively.
#[test]
fn routing_harvest_keeps_only_affinity_pair() {
    let extra = BTreeMap::from([
        ("X-OPENCODE-SESSION".to_string(), "s".to_string()),
        ("Authorization".to_string(), "Bearer k".to_string()),
    ]);
    let out = harvest_routing_headers(&[&extra]);
    assert_eq!(out.len(), 1);
    assert_eq!(out.get("x-opencode-session").map(String::as_str), Some("s"));
}

/// Merge appends affinity headers but never overrides a per-call one
/// (any casing).
#[test]
fn routing_merge_prefers_lua_headers() {
    let lua = vec![("X-Opencode-Session".to_string(), "call".to_string())];
    let routing = BTreeMap::from([
        ("x-opencode-session".to_string(), "turn".to_string()),
        ("x-opencode-client".to_string(), "dex".to_string()),
    ]);
    let merged = with_routing_headers(&lua, &routing);
    assert_eq!(merged.len(), 2);
    assert!(merged.contains(&("X-Opencode-Session".to_string(), "call".to_string())));
    assert!(merged.contains(&("x-opencode-client".to_string(), "dex".to_string())));
}

/// The task-local drive context shadows the process-wide fallback while
/// in scope, and the fallback returns afterwards (nested scopes restore
/// the outer turn — the subagent child pattern).
#[allow(clippy::await_holding_lock)] // single-threaded runtime; env must stay redirected
#[tokio::test]
async fn drive_model_scope_shadows_global_fallback() {
    // Daemon e2e tests run real turns (which record `LAST_MODEL`) under
    // TEST_SESSIONS_ENV_LOCK; take it first (consistent order) so a
    // concurrent daemon turn can't slip a snapshot in between
    // `reset_for_tests()` and the first fire (previous != nil).
    let _sessions = crate::daemon::state::lock_map(&crate::session::TEST_SESSIONS_ENV_LOCK);
    let _turn = crate::agent::turn_loop::tests::TEST_TURN_ENV_LOCK
        .lock()
        .await;
    let _ext = TEST_GLOBAL_MANAGER_LOCK.lock().await;
    let mgr = global_manager();
    mgr.reset_for_tests().await;
    let global = crate::llm::config::ExtensionModelSnapshot {
        provider: "gprov".to_string(),
        model: "g-1".to_string(),
        api: "openai-responses".to_string(),
        base_url: "https://gprov.example/v1".to_string(),
    };
    let scoped = crate::llm::config::ExtensionModelSnapshot {
        provider: "sprov".to_string(),
        model: "s-2".to_string(),
        api: "openai-responses".to_string(),
        base_url: "https://sprov.example/v1".to_string(),
    };
    *LAST_MODEL.lock().expect("served model lock") = Some(global);
    assert_eq!(
        served_model_snapshot().map(|s| s.id()),
        Some("gprov/g-1".to_string())
    );
    let drive = DriveModel {
        snapshot: Some(scoped),
        routing: BTreeMap::from([("x-opencode-session".to_string(), "sess-1".to_string())]),
    };
    with_drive_model(drive, async {
        assert_eq!(
            served_model_snapshot().map(|s| s.id()),
            Some("sprov/s-2".to_string())
        );
        assert_eq!(
            current_routing_headers()
                .get("x-opencode-session")
                .map(String::as_str),
            Some("sess-1")
        );
        // A scoped turn without affinity headers serves none — never
        // a stale session id from the fallback.
        let bare = DriveModel {
            snapshot: Some(crate::llm::config::ExtensionModelSnapshot {
                provider: "sprov".to_string(),
                model: "s-3".to_string(),
                api: "openai-responses".to_string(),
                base_url: "https://sprov.example/v1".to_string(),
            }),
            routing: BTreeMap::new(),
        };
        with_drive_model(bare, async {
            assert_eq!(
                served_model_snapshot().map(|s| s.id()),
                Some("sprov/s-3".to_string())
            );
            assert!(current_routing_headers().is_empty());
        })
        .await;
    })
    .await;
    assert_eq!(
        served_model_snapshot().map(|s| s.id()),
        Some("gprov/g-1".to_string())
    );
    mgr.reset_for_tests().await;
}

/// Concurrent drives keep their own model end to end: two overlapping
/// `dex.model.current()` reads through one worker each serve the
/// drive's own snapshot, never the process-wide fallback. (Pre-fix both
/// reads served whatever turn recorded last.)
#[tokio::test]
async fn concurrent_drives_keep_their_own_model() {
    let root = fixture_ext(
            "manifest_version: 1\nid: iso\nversion: 0.1.0\ncapabilities: [tools, model]\ntools:\n  - name: who\n    description: Who.\n    parameters: {\"type\": \"object\"}\n",
            r#"return function(dex)
  dex.tools.register({ name = "who", execute = function(ctx, args)
    return (dex.model.current()).id
  end })
end
"#,
        );
    let mgr = ExtensionManager::fresh();
    mgr.refresh_with(std::slice::from_ref(&root)).await;
    let policy = crate::tools::Policy::trusted();
    let cancel = crate::agent::state::GlobalCancellation;
    let host = HostCtx {
        cancel: &cancel,
        policy: &policy,
        filter: None,
    };
    let empty = serde_json::Map::new();
    let drive_for = |provider: &str, model: &str| DriveModel {
        snapshot: Some(crate::llm::config::ExtensionModelSnapshot {
            provider: provider.to_string(),
            model: model.to_string(),
            api: "openai-responses".to_string(),
            base_url: "https://example.invalid/v1".to_string(),
        }),
        routing: BTreeMap::new(),
    };
    let (left, right) = tokio::join!(
        with_drive_model(
            drive_for("prov-a", "m-a"),
            mgr.call("ext__iso__who", &empty, &host)
        ),
        with_drive_model(
            drive_for("prov-b", "m-b"),
            mgr.call("ext__iso__who", &empty, &host)
        ),
    );
    assert_eq!(left.unwrap(), "prov-a/m-a");
    assert_eq!(right.unwrap(), "prov-b/m-b");
    std::fs::remove_dir_all(&root).ok();
}

/// `dex.json` round-trips tables through strings (the fetch body/parse
/// primitive for model-endpoint calls).
#[tokio::test]
async fn dex_json_round_trips_tables() {
    let root = fixture_ext(
            "manifest_version: 1\nid: jx\nversion: 0.1.0\ncapabilities: [tools]\ntools:\n  - name: rt\n    description: Echo.\n    parameters: {\"type\": \"object\"}\n",
            r#"return function(dex)
  dex.tools.register({ name = "rt", execute = function(ctx, args)
    local back = dex.json.decode(dex.json.encode({ echo = args.x, n = 7 }))
    return dex.json.encode({ echo = back.echo, n = back.n, list = { 1, 2 } })
  end })
end
"#,
        );
    let mgr = ExtensionManager::fresh();
    mgr.refresh_with(std::slice::from_ref(&root)).await;
    let policy = crate::tools::Policy::trusted();
    let host = HostCtx {
        cancel: &crate::agent::state::GlobalCancellation,
        policy: &policy,
        filter: None,
    };
    let mut args = serde_json::Map::new();
    args.insert("x".to_string(), serde_json::Value::String("hi".to_string()));
    let out = mgr.call("ext__jx__rt", &args, &host).await.unwrap();
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        value,
        serde_json::json!({"echo": "hi", "n": 7, "list": [1, 2]})
    );
    std::fs::remove_dir_all(&root).ok();
}

/// `dex.model.current()/auth()` serve the recorded snapshot through Lua,
/// and the capability gates read attempts without it. The snapshot is
/// set directly (no env): file+env resolution is covered by the
/// `llm::config` unit tests.
#[allow(clippy::await_holding_lock)] // single-threaded runtime; env must stay redirected
#[tokio::test]
async fn dex_model_tables_read_snapshot_and_gate_capability() {
    // Daemon e2e tests run real turns (which record `LAST_MODEL`) under
    // TEST_SESSIONS_ENV_LOCK; take it first (consistent order) so a
    // concurrent daemon turn can't slip a snapshot in between
    // `reset_for_tests()` and the first fire (previous != nil).
    let _sessions = crate::daemon::state::lock_map(&crate::session::TEST_SESSIONS_ENV_LOCK);
    let _turn = crate::agent::turn_loop::tests::TEST_TURN_ENV_LOCK
        .lock()
        .await;
    let _ext = TEST_GLOBAL_MANAGER_LOCK.lock().await;
    let mgr = global_manager();
    mgr.reset_for_tests().await;
    *LAST_MODEL.lock().expect("served model lock") =
        Some(crate::llm::config::ExtensionModelSnapshot {
            provider: "myprov".to_string(),
            model: "m-7".to_string(),
            api: "openai-responses".to_string(),
            base_url: "https://myprov.example/v1".to_string(),
        });
    let root = fixture_exts(&[
            (
                "capped",
                "manifest_version: 1\nid: capped\nversion: 0.1.0\ncapabilities: [tools, model]\ntools:\n  - name: who\n    description: Who.\n    parameters: {\"type\": \"object\"}\n  - name: key\n    description: Key.\n    parameters: {\"type\": \"object\"}\n  - name: xkey\n    description: Xkey.\n    parameters: {\"type\": \"object\"}\n",
                r#"return function(dex)
  dex.tools.register({ name = "who", execute = function(ctx, args)
    return dex.json.encode(dex.model.current())
  end })
  dex.tools.register({ name = "key", execute = function(ctx, args)
      return (dex.model.auth()).api_key
    end })
    dex.tools.register({ name = "xkey", execute = function(ctx, args)
      return (dex.model.auth("anthropic")).api_key
    end })
end
"#,
            ),
            (
                "nocap",
                "manifest_version: 1\nid: nocap\nversion: 0.1.0\ncapabilities: [tools]\ntools:\n  - name: peek\n    description: Peek.\n    parameters: {\"type\": \"object\"}\n",
                r#"return function(dex)
  dex.tools.register({ name = "peek", execute = function(ctx, args)
    return dex.json.encode(dex.model.current())
  end })
end
"#,
            ),
        ]);
    let mgr = ExtensionManager::fresh();
    mgr.refresh_with(std::slice::from_ref(&root)).await;
    let policy = crate::tools::Policy::trusted();
    let host = HostCtx {
        cancel: &crate::agent::state::GlobalCancellation,
        policy: &policy,
        filter: None,
    };
    let empty = serde_json::Map::new();
    let out = mgr.call("ext__capped__who", &empty, &host).await.unwrap();
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        value,
        serde_json::json!({
            "provider": "myprov",
            "model": "m-7",
            "id": "myprov/m-7",
            "api": "openai-responses",
            "base_url": "https://myprov.example/v1",
        })
    );
    let key = mgr
        .call("ext__capped__key", &empty, &host)
        .await
        .unwrap_err();
    // No deposits for `myprov` in this process: the error names the
    // deposit places (key success is covered by the config unit tests).
    assert!(
        key.contains("no API key for provider 'myprov'"),
        "got: {key}"
    );
    // Cross-provider keys need `net.providers` on top of `model`.
    let xkey = mgr
        .call("ext__capped__xkey", &empty, &host)
        .await
        .unwrap_err();
    assert!(xkey.contains("net.providers"), "got: {xkey}");
    let err = mgr
        .call("ext__nocap__peek", &empty, &host)
        .await
        .unwrap_err();
    assert!(err.contains("without the model capability"), "got: {err}");
    global_manager().reset_for_tests().await;
    std::fs::remove_dir_all(&root).ok();
}

/// `dex.net.fetch` refuses anything outside the model endpoint before
/// touching the network — plus bad methods and host-controlled headers.
#[allow(clippy::await_holding_lock)] // single-threaded runtime; env must stay redirected
#[tokio::test]
async fn net_fetch_confines_to_model_endpoint() {
    // Daemon e2e tests run real turns (which record `LAST_MODEL`) under
    // TEST_SESSIONS_ENV_LOCK; take it first (consistent order) so a
    // concurrent daemon turn can't slip a snapshot in between
    // `reset_for_tests()` and the first fire (previous != nil).
    let _sessions = crate::daemon::state::lock_map(&crate::session::TEST_SESSIONS_ENV_LOCK);
    let _turn = crate::agent::turn_loop::tests::TEST_TURN_ENV_LOCK
        .lock()
        .await;
    let _ext = TEST_GLOBAL_MANAGER_LOCK.lock().await;
    let mgr = global_manager();
    mgr.reset_for_tests().await;
    *LAST_MODEL.lock().expect("served model lock") =
        Some(crate::llm::config::ExtensionModelSnapshot {
            provider: "myprov".to_string(),
            model: "m-7".to_string(),
            api: "openai-responses".to_string(),
            base_url: "https://myprov.example/v1".to_string(),
        });
    let foreign = net_fetch(
        "https://evil.example/x".to_string(),
        "GET".to_string(),
        Vec::new(),
        None,
        5_000,
        false,
    )
    .await
    .unwrap_err();
    assert!(
        foreign.contains("outside the model endpoint"),
        "got: {foreign}"
    );
    // A lookalike host (prefix attack) is still outside the endpoint.
    let prefix = net_fetch(
        "https://myprov.example.evil.example/x".to_string(),
        "GET".to_string(),
        Vec::new(),
        None,
        5_000,
        false,
    )
    .await
    .unwrap_err();
    assert!(
        prefix.contains("outside the model endpoint"),
        "got: {prefix}"
    );
    let scheme = net_fetch(
        "ftp://myprov.example/x".to_string(),
        "GET".to_string(),
        Vec::new(),
        None,
        5_000,
        false,
    )
    .await
    .unwrap_err();
    assert!(scheme.contains("only http(s)"), "got: {scheme}");
    let method = net_fetch(
        "https://myprov.example/v1/x".to_string(),
        "TRACE".to_string(),
        Vec::new(),
        None,
        5_000,
        false,
    )
    .await
    .unwrap_err();
    assert!(method.contains("unsupported method"), "got: {method}");
    let header = net_fetch(
        "https://myprov.example/v1/x".to_string(),
        "GET".to_string(),
        vec![("Host".to_string(), "myprov.example".to_string())],
        None,
        5_000,
        false,
    )
    .await
    .unwrap_err();
    assert!(header.contains("host-controlled"), "got: {header}");
    // The query never reaches the error: a key in `?key=` stays out of
    // logs even when the request itself fails downstream (here: DNS for
    // a nonexistent domain — confinement passes, the network does not).
    let keyed = net_fetch(
        "https://myprov.example/v1/x?key=secret".to_string(),
        "GET".to_string(),
        Vec::new(),
        None,
        5_000,
        false,
    )
    .await
    .unwrap_err();
    assert!(!keyed.contains("secret"), "got: {keyed}");
    assert!(
        keyed.contains("https://myprov.example/v1/x"),
        "got: {keyed}"
    );
    mgr.reset_for_tests().await;
}

/// `net.providers` widens the allowlist to configured provider
/// endpoints (each with its own key); lookalike hosts and anything
/// unconfigured stay confined even with the capability declared.
#[tokio::test]
#[allow(clippy::await_holding_lock)] // redirected env must stay put across the manager awaits
async fn net_fetch_allows_configured_provider_endpoints() {
    // This test redirects DEX_CONFIG/XDG_CACHE_HOME, which the
    // sessions-locked config tests also mutate — serialize against
    // them (consistent order: sessions -> turn -> manager, like the
    // other extension tests, so the global locks can't deadlock).
    let _env_lock = crate::session::TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _turn = crate::agent::turn_loop::tests::TEST_TURN_ENV_LOCK
        .lock()
        .await;
    let _ext = TEST_GLOBAL_MANAGER_LOCK.lock().await;
    let _env = EnvRestore::take(&["DEX_CONFIG", "XDG_CACHE_HOME", "DEX_MODEL", "DEX_PROVIDER"]);
    let root = std::env::temp_dir().join(format!("dex-ext-netprov-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("config.yaml"),
        "providers:\n  otherprov:\n    base_url: https://otherprov.example/v1\n    api_key: k-other\n",
    )
    .unwrap();
    std::env::set_var("DEX_CONFIG", root.join("config.yaml"));
    std::env::set_var("XDG_CACHE_HOME", root.join("cache"));
    for key in ["DEX_MODEL", "DEX_PROVIDER"] {
        std::env::remove_var(key);
    }
    let mgr = global_manager();
    mgr.reset_for_tests().await;
    *LAST_MODEL.lock().expect("served model lock") =
        Some(crate::llm::config::ExtensionModelSnapshot {
            provider: "myprov".to_string(),
            model: "m-7".to_string(),
            api: "openai-responses".to_string(),
            base_url: "https://myprov.example/v1".to_string(),
        });
    // Without the capability flag the configured provider stays outside.
    let denied = net_fetch(
        "https://otherprov.example/v1/x".to_string(),
        "GET".to_string(),
        Vec::new(),
        None,
        5_000,
        false,
    )
    .await
    .unwrap_err();
    assert!(
        denied.contains("outside the model endpoint"),
        "got: {denied}"
    );
    // With it, the request proceeds — DNS for the nonexistent domain
    // fails at the network layer, never at confinement.
    let allowed = net_fetch(
        "https://otherprov.example/v1/x".to_string(),
        "GET".to_string(),
        Vec::new(),
        None,
        5_000,
        true,
    )
    .await
    .unwrap_err();
    assert!(
        !allowed.contains("outside the model endpoint"),
        "got: {allowed}"
    );
    // A lookalike host stays outside even with the flag declared.
    let prefix = net_fetch(
        "https://otherprov.example.evil.example/x".to_string(),
        "GET".to_string(),
        Vec::new(),
        None,
        5_000,
        true,
    )
    .await
    .unwrap_err();
    assert!(
        prefix.contains("outside the model endpoint"),
        "got: {prefix}"
    );
    mgr.reset_for_tests().await;
    std::fs::remove_dir_all(&root).ok();
}

/// `dex.net.fetch` success path against a loopback stub: status +
/// headers + body come back as a value.
#[allow(clippy::await_holding_lock)] // single-threaded runtime; env must stay redirected
#[tokio::test]
async fn net_fetch_returns_values_against_loopback() {
    // Daemon e2e tests run real turns (which record `LAST_MODEL`) under
    // TEST_SESSIONS_ENV_LOCK; take it first (consistent order) so a
    // concurrent daemon turn can't slip a snapshot in between
    // `reset_for_tests()` and the first fire (previous != nil).
    let _sessions = crate::daemon::state::lock_map(&crate::session::TEST_SESSIONS_ENV_LOCK);
    let _turn = crate::agent::turn_loop::tests::TEST_TURN_ENV_LOCK
        .lock()
        .await;
    let _ext = TEST_GLOBAL_MANAGER_LOCK.lock().await;
    let mgr = global_manager();
    mgr.reset_for_tests().await;
    // Loopback must not ride a proxy, wherever the suite runs.
    let _proxy = EnvRestore::take(&["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "NO_PROXY"]);
    std::env::set_var("NO_PROXY", "127.0.0.1,localhost");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (seen_tx, seen_rx) = tokio::sync::oneshot::channel::<Vec<u8>>();
    tokio::spawn(async move {
        let Ok((mut stream, _)) = listener.accept().await else {
            return;
        };
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut buf = vec![0u8; 4096];
        let n = stream.read(&mut buf).await.unwrap_or(0);
        let _ = seen_tx.send(buf[..n].to_vec());
        let _ = stream
                  .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\nX-Mark: yes\r\nConnection: close\r\n\r\nhello world")
                  .await;
    });
    *LAST_MODEL.lock().expect("served model lock") =
        Some(crate::llm::config::ExtensionModelSnapshot {
            provider: "loop".to_string(),
            model: "m".to_string(),
            api: "openai-responses".to_string(),
            base_url: format!("http://127.0.0.1:{port}"),
        });
    // Recorded routing headers ride along; a per-call header wins.
    *LAST_ROUTING_HEADERS.lock().expect("routing headers lock") = BTreeMap::from([
        ("x-opencode-session".to_string(), "sess-9".to_string()),
        ("x-opencode-client".to_string(), "dex".to_string()),
    ]);
    let out = net_fetch(
        format!("http://127.0.0.1:{port}/v1/search"),
        "POST".to_string(),
        vec![("X-Test".to_string(), "1".to_string())],
        Some("{}".to_string()),
        5_000,
        false,
    )
    .await
    .unwrap();
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(value["status"], 200);
    assert_eq!(value["body"], "hello world");
    assert_eq!(value["headers"]["x-mark"], "yes");
    let seen = String::from_utf8_lossy(&seen_rx.await.unwrap()).to_lowercase();
    assert!(seen.contains("x-opencode-session: sess-9"), "got:\n{seen}");
    assert!(seen.contains("x-opencode-client: dex"), "got:\n{seen}");
    mgr.reset_for_tests().await;
}

/// `dex.net.fetch` never follows redirects: a 302 to a closed port
/// surfaces as a 3xx value instead of a followed (failed) fetch.
/// Confinement is checked against the requested URL only.
#[allow(clippy::await_holding_lock)] // single-threaded runtime; env must stay redirected
#[tokio::test]
async fn net_fetch_does_not_follow_redirects() {
    // Daemon e2e tests run real turns (which record `LAST_MODEL`) under
    // TEST_SESSIONS_ENV_LOCK; take it first (consistent order) so a
    // concurrent daemon turn can't slip a snapshot in between
    // `reset_for_tests()` and the first fire (previous != nil).
    let _sessions = crate::daemon::state::lock_map(&crate::session::TEST_SESSIONS_ENV_LOCK);
    let _turn = crate::agent::turn_loop::tests::TEST_TURN_ENV_LOCK
        .lock()
        .await;
    let _ext = TEST_GLOBAL_MANAGER_LOCK.lock().await;
    let mgr = global_manager();
    mgr.reset_for_tests().await;
    let _proxy = EnvRestore::take(&["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "NO_PROXY"]);
    std::env::set_var("NO_PROXY", "127.0.0.1,localhost");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let Ok((mut stream, _)) = listener.accept().await else {
            return;
        };
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut buf = vec![0u8; 1024];
        let _ = stream.read(&mut buf).await;
        let _ = stream
                .write_all(b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:9/gone\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .await;
    });
    *LAST_MODEL.lock().expect("served model lock") =
        Some(crate::llm::config::ExtensionModelSnapshot {
            provider: "loop".to_string(),
            model: "m".to_string(),
            api: "openai-responses".to_string(),
            base_url: format!("http://127.0.0.1:{port}"),
        });
    let out = net_fetch(
        format!("http://127.0.0.1:{port}/old"),
        "GET".to_string(),
        Vec::new(),
        None,
        5_000,
        false,
    )
    .await
    .unwrap();
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(value["status"], 302);
    mgr.reset_for_tests().await;
}

/// The shipped web example loads whole and gates its tools on the
/// served model: both tools on Gemini, search only elsewhere. The
/// snapshot is set directly (no env): file resolution is covered by the
/// `llm::config` unit tests. Visibility syncs through the `model_select`
/// event (load-time host calls don't exist), exactly like production.
#[allow(clippy::await_holding_lock)] // single-threaded runtime; env must stay redirected
#[tokio::test]
async fn web_example_loads_and_gates_tools() {
    // Daemon e2e tests run real turns (which record `LAST_MODEL`) under
    // TEST_SESSIONS_ENV_LOCK; take it first (consistent order) so a
    // concurrent daemon turn can't slip a snapshot in between
    // `reset_for_tests()` and the first fire (previous != nil).
    let _sessions = crate::daemon::state::lock_map(&crate::session::TEST_SESSIONS_ENV_LOCK);
    let _turn = crate::agent::turn_loop::tests::TEST_TURN_ENV_LOCK
        .lock()
        .await;
    let _ext = TEST_GLOBAL_MANAGER_LOCK.lock().await;
    let example = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/extensions");
    let policy = crate::tools::Policy::trusted();
    let cancel = crate::agent::state::GlobalCancellation;
    let host = HostCtx {
        cancel: &cancel,
        policy: &policy,
        filter: None,
    };
    for (provider, base_url, want) in [
        (
            "gemini",
            "https://generativelanguage.googleapis.com",
            vec![
                "ext__web__fetch".to_string(),
                "ext__web__search".to_string(),
            ],
        ),
        (
            "myprov",
            "https://myprov.example/v1",
            vec!["ext__web__search".to_string()],
        ),
    ] {
        let mgr = global_manager();
        mgr.reset_for_tests().await;
        mgr.refresh_with(std::slice::from_ref(&example)).await;
        *LAST_MODEL.lock().expect("served model lock") =
            Some(crate::llm::config::ExtensionModelSnapshot {
                provider: provider.to_string(),
                model: "m".to_string(),
                api: "openai-responses".to_string(),
                base_url: base_url.to_string(),
            });
        mgr.fire_event(
            "model_select",
            serde_json::json!({"model": format!("{provider}/m"), "previous": null}),
            &host,
        )
        .await;
        let mut names: Vec<String> = mgr
            .active_cached()
            .await
            .iter()
            .map(|d| d.function.name.clone())
            .collect();
        names.sort();
        assert_eq!(names, want, "provider {provider}");
        let events: Vec<String> = mgr
            .engines
            .try_read()
            .ok()
            .and_then(|e| e.get("web").map(|ext| ext.events.clone()))
            .unwrap_or_default();
        assert!(
            events.contains(&"model_select".to_string()),
            "got: {events:?}"
        );
    }
    global_manager().reset_for_tests().await;
}

/// Model-independent search: an unsupported served model (no search
/// API) gets no tools at all, while `/search-model` arms a configured
/// override provider — `search` then becomes visible again and serves
/// through that provider's endpoint + key over `net.providers`.
/// Visibility re-syncs inside the command itself (no `model_select`
/// round-trip — that event only fires on provider/model change).
#[tokio::test]
#[allow(clippy::await_holding_lock)] // redirected env must stay put across the manager awaits
async fn web_example_falls_back_to_override_model() {
    // Same race as above: this test redirects DEX_CONFIG/XDG_* while
    // sessions-locked config tests assume exclusive env access. Take the
    // sessions lock first (consistent order: sessions -> turn -> manager).
    let _env_lock = crate::session::TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _turn = crate::agent::turn_loop::tests::TEST_TURN_ENV_LOCK
        .lock()
        .await;
    let _ext = TEST_GLOBAL_MANAGER_LOCK.lock().await;
    let example = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/extensions");
    let policy = crate::tools::Policy::trusted();
    let cancel = crate::agent::state::GlobalCancellation;
    let host = HostCtx {
        cancel: &cancel,
        policy: &policy,
        filter: None,
    };
    let active = |mgr: &std::sync::Arc<ExtensionManager>| {
        let mgr = std::sync::Arc::clone(mgr);
        async move {
            let mut names: Vec<String> = mgr
                .active_cached()
                .await
                .iter()
                .map(|d| d.function.name.clone())
                .collect();
            names.sort();
            names
        }
    };
    // Hermetic XDG paths from the start: dex.state writes a JSON file
    // under XDG_DATA_HOME, and discovery must not find an installed
    // copy of `web` (ensure_loaded re-boots from the installed dirs).
    let _env = EnvRestore::take(&[
        "DEX_CONFIG",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_CACHE_HOME",
        "DEX_MODEL",
        "DEX_PROVIDER",
    ]);
    let root = std::env::temp_dir().join(format!("dex-ext-webfall-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("data")).unwrap();
    std::fs::create_dir_all(root.join("config")).unwrap();
    std::env::set_var("XDG_CONFIG_HOME", root.join("config"));
    std::env::set_var("XDG_DATA_HOME", root.join("data"));
    std::env::set_var("XDG_CACHE_HOME", root.join("cache"));
    // Loopback must not ride a proxy, wherever the suite runs.
    let _proxy = EnvRestore::take(&["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "NO_PROXY"]);
    std::env::set_var("NO_PROXY", "127.0.0.1,localhost");
    for key in ["DEX_MODEL", "DEX_PROVIDER"] {
        std::env::remove_var(key);
    }

    // 1. A codex-served model rides the OpenAI wire but serves no
    //    search tool: the extension must show nothing, not a tool
    //    that always errors.
    let mgr = global_manager();
    mgr.reset_for_tests().await;
    mgr.refresh_with(std::slice::from_ref(&example)).await;
    *LAST_MODEL.lock().expect("served model lock") =
        Some(crate::llm::config::ExtensionModelSnapshot {
            provider: "openai-codex".to_string(),
            model: "m".to_string(),
            api: "openai-responses".to_string(),
            base_url: "https://chatgpt.example/backend".to_string(),
        });
    mgr.fire_event(
        "model_select",
        serde_json::json!({"model": "openai-codex/m", "previous": null}),
        &host,
    )
    .await;
    assert!(
        active(&mgr).await.is_empty(),
        "codex must not see search/fetch, got: {:?}",
        active(&mgr).await
    );
    global_manager().reset_for_tests().await;

    // 2. The served model has no search API, but another configured
    //    provider does. /search-model arms it; the search falls back
    //    to that provider's endpoint + key (anthropic wire stub).
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    std::fs::write(
        root.join("config.yaml"),
        format!(
            "providers:\n  anthropic:\n    base_url: http://127.0.0.1:{port}\n    api_key: k-fallback\n    api: anthropic-messages\n"
        ),
    )
    .unwrap();
    std::env::set_var("DEX_CONFIG", root.join("config.yaml"));
    let body = serde_json::json!({
        "content": [{
            "type": "text",
            "text": "fallback search works",
            "citations": [{"url": "https://example.test/a", "document_title": "A"}]
        }]
    });
    let payload = body.to_string();
    let (seen_tx, seen_rx) = tokio::sync::oneshot::channel::<Vec<u8>>();
    let server = tokio::spawn(async move {
        let Ok((mut stream, _)) = listener.accept().await else {
            return;
        };
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut buf = vec![0u8; 4096];
        let _ = stream.read(&mut buf).await;
        let _ = seen_tx.send(buf);
        let _ = stream
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    payload.len(),
                    payload
                )
                .as_bytes(),
            )
            .await;
    });
    let mgr = global_manager();
    mgr.reset_for_tests().await;
    mgr.refresh_with(std::slice::from_ref(&example)).await;
    *LAST_MODEL.lock().expect("served model lock") =
        Some(crate::llm::config::ExtensionModelSnapshot {
            provider: "glmprov".to_string(),
            model: "glm-5".to_string(),
            api: "openai-completions".to_string(),
            base_url: "https://glm.example/v1".to_string(),
        });
    // Arm the override through the user-facing command.
    let out = crate::extensions::run_command_global(
        "web",
        "search-model",
        "anthropic/claude-fallback",
        &cancel,
    )
    .await
    .unwrap();
    assert!(
        out.contains("override set to anthropic/claude-fallback"),
        "got: {out}"
    );
    // No `model_select` fire: the command re-syncs visibility itself
    // (that event only fires on provider/model change).
    assert_eq!(
        active(&mgr).await,
        vec!["ext__web__search".to_string()],
        "the override must make search visible again"
    );
    let result = mgr
        .call(
            "ext__web__search",
            &serde_json::json!({"query": "hi"})
                .as_object()
                .unwrap()
                .clone(),
            &host,
        )
        .await
        .unwrap();
    assert!(result.contains("fallback search works"), "got: {result}");
    assert!(result.contains("example.test/a"), "got: {result}");
    // The request went to the override provider's endpoint with the
    // override model id and its key — the whole point of the fallback.
    let seen = String::from_utf8_lossy(&seen_rx.await.unwrap()).to_lowercase();
    assert!(seen.contains("claude-fallback"), "got: {seen}");
    assert!(seen.contains("x-api-key: k-fallback"), "got: {seen}");
    let _ = server.await;

    // 3. A gemini override serves both tools, even when the current
    //    model serves neither. Then clear it (no state leaks). The
    //    engines stay loaded here — `ensure_loaded` would otherwise
    //    boot `web` from the installed dirs, not the example.
    //    (The override must be a configured provider — set-time
    //    validation rejects keyless ones, so declare gemini here.)
    std::fs::write(
          root.join("config.yaml"),
          format!(
              "providers:\n  anthropic:\n    base_url: http://127.0.0.1:{port}\n    api_key: k-fallback\n    api: anthropic-messages\n  gemini:\n    base_url: http://127.0.0.1:{port}\n    api_key: k-gemini\n"
          ),
      )
      .unwrap();
    let out = crate::extensions::run_command_global("web", "search-model", "gemini/gm-1", &cancel)
        .await
        .unwrap();
    assert!(out.contains("family gemini"), "got: {out}");
    assert_eq!(
        active(&mgr).await,
        vec![
            "ext__web__fetch".to_string(),
            "ext__web__search".to_string()
        ],
        "a gemini override serves both tools"
    );
    let off = crate::extensions::run_command_global("web", "search-model", "off", &cancel)
        .await
        .unwrap();
    assert!(off.contains("cleared"), "got: {off}");
    assert!(
        active(&mgr).await.is_empty(),
        "cleared override hides the tools again, got: {:?}",
        active(&mgr).await
    );
    global_manager().reset_for_tests().await;
    std::fs::remove_dir_all(&root).ok();
}
