//! The threshold-compaction summarizer knob (`DEX_COMPACTION`).

use crate::runtime::notice::warn_once;

/// `DEX_COMPACTION` selects the threshold-compaction summarizer (`llm` =
/// model summary, default deterministic; `1` is an alias for `llm`).
pub(crate) const COMPACTION_ENV: &str = "DEX_COMPACTION";

/// How `compact_history` summarizes the archivable span.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SummaryMode {
    Deterministic,
    Llm,
}

/// Parse `DEX_COMPACTION`: `llm` (any case; `1` accepted as an alias) →
/// [`SummaryMode::Llm`], unset/explicit offs → [`SummaryMode::Deterministic`].
/// A non-empty unrecognized value warns once (a typo must not silently
/// degrade to deterministic) and likewise falls back to deterministic.
pub(crate) fn summary_mode() -> SummaryMode {
    let raw = std::env::var(COMPACTION_ENV).unwrap_or_default();
    match raw.trim().to_ascii_lowercase().as_str() {
        "llm" | "1" => SummaryMode::Llm,
        "" | "0" | "false" | "off" | "no" => SummaryMode::Deterministic,
        _ => {
            let short: String = raw.trim().chars().take(32).collect();
            warn_once(
                "env:DEX_COMPACTION",
                &format!("ignoring DEX_COMPACTION={short:?} — expected 'llm' or '0'"),
            );
            SummaryMode::Deterministic
        }
    }
}

/// `dex doctor` origin row for the threshold-compaction knob: the value
/// names the mode that runs, the source names the env var whenever it is
/// set (even to an explicit off or a warned-about typo — the value still
/// reports what runs), or the built-in default when unset/empty.
pub(crate) fn compaction_doctor() -> (String, String) {
    let value = match summary_mode() {
        SummaryMode::Llm => "LLM summary",
        SummaryMode::Deterministic => "deterministic",
    }
    .to_string();
    let source = if std::env::var(COMPACTION_ENV)
        .ok()
        .is_some_and(|v| !v.trim().is_empty())
    {
        COMPACTION_ENV.to_string()
    } else {
        "built-in default".to_string()
    };
    (value, source)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_mode_parses_compaction_knob() {
        let _lock = crate::test_env::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var_os(COMPACTION_ENV);
        std::env::set_var(COMPACTION_ENV, "llm");
        assert_eq!(summary_mode(), SummaryMode::Llm);
        std::env::set_var(COMPACTION_ENV, "LLM");
        assert_eq!(summary_mode(), SummaryMode::Llm);
        // `1` stays accepted as an alias for the LLM mode.
        std::env::set_var(COMPACTION_ENV, "1");
        assert_eq!(summary_mode(), SummaryMode::Llm);
        // Explicit offs stay silent and deterministic.
        for off in ["0", "false", "off", "no", ""] {
            std::env::set_var(COMPACTION_ENV, off);
            assert_eq!(summary_mode(), SummaryMode::Deterministic);
        }
        // Garbage (including the removed `jev` mode) warns once and stays
        // deterministic — a typo must never silently pick another mode.
        for bad in ["jev", "jve"] {
            std::env::set_var(COMPACTION_ENV, bad);
            assert_eq!(summary_mode(), SummaryMode::Deterministic);
        }
        std::env::remove_var(COMPACTION_ENV);
        assert_eq!(summary_mode(), SummaryMode::Deterministic);
        assert_eq!(compaction_doctor().1, "built-in default");
        match prev {
            Some(v) => std::env::set_var(COMPACTION_ENV, v),
            None => std::env::remove_var(COMPACTION_ENV),
        }
    }
}
