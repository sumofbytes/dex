//! Experiment registry — the single seam between dex's core and the
//! opt-in experiments (online context compaction, observation pack,
//! evidence-preserving reducer).
//!
//! Every experiment module owns its own gate, tool schemas and doctor row;
//! host files (`loop.rs`, `protocol.rs`, `config.rs`) never name an
//! experiment's env var or gate. Adding an experiment means adding one
//! directory under `src/agent/` and one entry here.

use crate::core::types::ToolDefinition;
use crate::llm::config::LlmConfig;

/// One row of `dex doctor` output, rendered by `row()` in
/// `llm/config.rs::doctor`.
pub(crate) struct DoctorRow {
    pub(crate) label: &'static str,
    pub(crate) value: String,
    pub(crate) source: String,
}

/// What a doctor row may need from the resolved configuration. Rows are
/// printed per provider, so `live` is the config that provider would use
/// and `model` the model id shown on the model row.
pub(crate) struct DoctorCtx<'a> {
    pub(crate) live: Option<&'a LlmConfig>,
    pub(crate) model: &'a str,
}

pub(crate) struct Experiment {
    /// Doctor label and registry key (`online compaction`, `obs pack`,
    /// `evidence reducer`); also the lookup for `is_enabled`/
    /// `dependencies_met`.
    pub(crate) name: &'static str,
    pub(crate) enabled: fn() -> bool,
    /// Tool schemas; the registry registers them only while the
    /// experiment is enabled (empty when the experiment adds no tools).
    pub(crate) tool_defs: fn() -> Vec<ToolDefinition>,
    /// `dex doctor` row; `None` when the experiment prints no row (e.g. a
    /// gate-dependent row that is off).
    pub(crate) doctor_row: fn(&DoctorCtx<'_>) -> Option<DoctorRow>,
    /// Other experiments this one requires to be useful.
    pub(crate) depends_on: &'static [&'static str],
}

/// Registry keys — each experiment's doctor row renders the same label
/// (the doctor table pads keys to an 18-column width to fit them).
pub(crate) const ONLINE_COMPACTION: &str = "online compaction";
pub(crate) const OBS_PACK: &str = "obs pack";
pub(crate) const EVIDENCE_REDUCER: &str = "evidence reducer";

/// Registry order is also the tool-schema order.
static EXPERIMENTS: &[Experiment] = &[
    Experiment {
        name: ONLINE_COMPACTION,
        enabled: crate::agent::online_compaction::online_compaction_enabled,
        tool_defs: crate::agent::online_compaction::tool_defs,
        doctor_row: crate::agent::online_compaction::doctor_row,
        depends_on: &[],
    },
    Experiment {
        name: OBS_PACK,
        enabled: crate::agent::obs_pack::observation_pack_enabled,
        tool_defs: crate::agent::obs_pack::tool_defs,
        doctor_row: crate::agent::obs_pack::doctor_row,
        depends_on: &[],
    },
    Experiment {
        name: EVIDENCE_REDUCER,
        enabled: crate::agent::evidence_reducer::enabled,
        tool_defs: crate::agent::evidence_reducer::tool_defs,
        doctor_row: crate::agent::evidence_reducer::doctor_row,
        depends_on: &[OBS_PACK],
    },
];

pub(crate) fn is_enabled(key: &str) -> bool {
    EXPERIMENTS
        .iter()
        .find(|e| e.name == key)
        .is_some_and(|e| (e.enabled)())
}

/// Whether an experiment's declared dependencies are enabled. Experiments
/// consult this instead of naming another experiment's gate.
pub(crate) fn dependencies_met(key: &str) -> bool {
    EXPERIMENTS
        .iter()
        .find(|e| e.name == key)
        .is_none_or(|e| e.depends_on.iter().all(|d| is_enabled(d)))
}

/// Tool schemas for every enabled experiment, in registry order. The
/// gate is checked once here; experiment `tool_defs` only build schemas.
pub(crate) fn tool_definitions() -> Vec<ToolDefinition> {
    let mut out = Vec::new();
    for experiment in EXPERIMENTS {
        if (experiment.enabled)() {
            out.extend((experiment.tool_defs)());
        }
    }
    out
}

/// Doctor rows for every experiment, in registry order.
pub(crate) fn doctor_rows(ctx: DoctorCtx<'_>) -> Vec<DoctorRow> {
    EXPERIMENTS
        .iter()
        .filter_map(|e| (e.doctor_row)(&ctx))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_schema_contains_all_tools() {
        // Serializes against tests in other modules that flip the env vars
        // these gates read (online compaction, extra tools).
        let _lock = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _env = crate::session::EnvGuard(vec![
            (
                crate::agent::online_compaction::ONLINE_COMPACTION_ENV,
                std::env::var_os(crate::agent::online_compaction::ONLINE_COMPACTION_ENV),
            ),
            ("DEX_EXTRA_TOOLS", std::env::var_os("DEX_EXTRA_TOOLS")),
            (
                crate::agent::obs_pack::OBSERVATION_PACK_ENV,
                std::env::var_os(crate::agent::obs_pack::OBSERVATION_PACK_ENV),
            ),
        ]);
        std::env::remove_var(crate::agent::online_compaction::ONLINE_COMPACTION_ENV);
        std::env::remove_var("DEX_EXTRA_TOOLS");
        std::env::remove_var(crate::agent::obs_pack::OBSERVATION_PACK_ENV);
        let schema = crate::llm::protocol::tools_schema();
        let names: Vec<_> = schema.iter().map(|t| t.function.name.as_str()).collect();
        let expected: Vec<&str> = vec!["read", "bash", "write", "edit", "grep", "find", "ls"];
        assert_eq!(names, expected);

        // The online compaction gate adds the plan tool.
        std::env::set_var(crate::agent::online_compaction::ONLINE_COMPACTION_ENV, "1");
        let names: Vec<String> = crate::llm::protocol::tools_schema()
            .iter()
            .map(|t| t.function.name.clone())
            .collect();
        assert!(names.iter().any(|n| n == "update_plan"), "{names:?}");

        // The observation pack gate adds the recall tool.
        std::env::set_var(crate::agent::obs_pack::OBSERVATION_PACK_ENV, "1");
        let names: Vec<String> = crate::llm::protocol::tools_schema()
            .iter()
            .map(|t| t.function.name.clone())
            .collect();
        assert!(names.iter().any(|n| n == "obs_recall"), "{names:?}");
    }

    #[test]
    fn reducer_declares_obs_pack_dependency() {
        assert_eq!(
            EXPERIMENTS
                .iter()
                .find(|e| e.name == EVIDENCE_REDUCER)
                .map(|e| e.depends_on),
            Some(&[OBS_PACK][..])
        );
        let _lock = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _env = crate::session::EnvGuard(vec![
            (
                crate::agent::evidence_reducer::GATE_ENV,
                std::env::var_os(crate::agent::evidence_reducer::GATE_ENV),
            ),
            (
                crate::agent::obs_pack::OBSERVATION_PACK_ENV,
                std::env::var_os(crate::agent::obs_pack::OBSERVATION_PACK_ENV),
            ),
        ]);
        std::env::set_var(crate::agent::evidence_reducer::GATE_ENV, "1");
        std::env::remove_var(crate::agent::obs_pack::OBSERVATION_PACK_ENV);
        assert!(!dependencies_met(EVIDENCE_REDUCER));
        std::env::set_var(crate::agent::obs_pack::OBSERVATION_PACK_ENV, "1");
        assert!(dependencies_met(EVIDENCE_REDUCER));
    }
}
