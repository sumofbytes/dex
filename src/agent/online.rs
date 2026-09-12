//! Online context compaction — port of the SoL-Pi `online-context-compact`
//! extension (NVlabs/SoL-Pi). The model maintains a working plan through the
//! `update_plan` tool; completing a plan step is a *boundary* — a safe point
//! where history can be compacted. Instead of only a fixed token cap, a
//! compaction fires when it is economical: the cache re-write cost of the
//! retained prefix must pay for itself within the projected remaining work,
//! where the horizon is learned from observed requests-per-boundary and the
//! observed context growth rate (mechanism C4: "use observed context pressure
//! instead of global caps").

use serde_json::{json, Value};
use std::collections::HashSet;

/// Opt-in switch: `DEX_ONLINE_COMPACTION=1` registers `update_plan` and
/// enables boundary economics. Off by default — the tool costs prompt tokens
/// every request and only pays off on long-horizon work.
pub(crate) const ONLINE_COMPACTION_ENV: &str = "DEX_ONLINE_COMPACTION";

pub(crate) fn online_compaction_enabled() -> bool {
    std::env::var(ONLINE_COMPACTION_ENV).as_deref() == Ok("1")
}

/// Rough token estimate for the summary a compaction leaves behind.
pub(crate) const NATIVE_SUMMARY_TOKEN_ESTIMATE: u64 = 1_000;

/// Fallback cache-write/read ratio when the catalog doesn't price cache
/// writes; matches Anthropic 5m pricing (1.25x write / 0.1x read).
pub(crate) const DEFAULT_CACHE_WRITE_READ_RATIO: f64 = 12.5;

// ---------------------------------------------------------------------------
// Plan steps
// ---------------------------------------------------------------------------

pub(crate) const MAX_PLAN_STEPS: usize = 128;
const MAX_PLAN_STRING_BYTES: usize = 16_384;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PlanStatus {
    Pending,
    InProgress,
    Completed,
}

impl PlanStatus {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "in_progress" => Some(Self::InProgress),
            "completed" => Some(Self::Completed),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PlanStep {
    pub(crate) id: String,
    pub(crate) goal: String,
    pub(crate) status: PlanStatus,
}

/// Validate a `steps` array: bounded strings, exactly three keys per step,
/// known status, unique ids.
pub(crate) fn parse_plan_steps(value: &Value) -> Result<Vec<PlanStep>, String> {
    let Some(items) = value.as_array() else {
        return Err("steps must be an array".into());
    };
    if items.len() > MAX_PLAN_STEPS {
        return Err(format!("steps must have at most {MAX_PLAN_STEPS} items"));
    }
    if items.is_empty() {
        return Err("steps must contain at least one step".into());
    }
    let mut steps = Vec::with_capacity(items.len());
    for item in items {
        let Some(obj) = item.as_object() else {
            return Err("each step must be an object".into());
        };
        if obj.len() != 3 {
            return Err("each step must have exactly id, goal, and status".into());
        }
        let bounded = |key: &str| -> Result<String, String> {
            let raw = obj
                .get(key)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty() && s.len() <= MAX_PLAN_STRING_BYTES)
                .ok_or_else(|| format!("step {key} must be a non-empty string"))?;
            Ok(raw.to_string())
        };
        let id = bounded("id")?;
        let goal = bounded("goal")?;
        let status = obj
            .get("status")
            .and_then(Value::as_str)
            .and_then(PlanStatus::parse)
            .ok_or_else(|| "step status must be pending, in_progress, or completed".to_string())?;
        steps.push(PlanStep { id, goal, status });
    }
    let unique = steps.iter().map(|s| s.id.as_str()).collect::<Vec<_>>();
    if unique.len() != unique.iter().collect::<HashSet<_>>().len() {
        return Err("step ids must be unique".into());
    }
    Ok(steps)
}

pub(crate) struct PlanTransition {
    /// Steps newly marked completed by this update (the boundary trigger).
    pub(crate) completed: Vec<PlanStep>,
    pub(crate) advice: Vec<String>,
}

/// Diff the previous plan against the next: which steps just completed, plus
/// gentle advice for plan-hygiene violations (goal reuse, parallel progress).
pub(crate) fn analyze_plan_transition(prev: &[PlanStep], next: &[PlanStep]) -> PlanTransition {
    let mut completed = Vec::new();
    let mut advice = Vec::new();
    for step in next {
        let prior = prev.iter().find(|p| p.id == step.id);
        if prior.is_none_or(|p| p.status != PlanStatus::Completed)
            && step.status == PlanStatus::Completed
        {
            completed.push(step.clone());
        }
        if let Some(prior) = prior {
            if prior.goal != step.goal {
                advice.push(format!(
                    "Plan step {:?} changed goal; reuse an id only for the same goal.",
                    step.id
                ));
            }
        }
    }
    let in_progress = next
        .iter()
        .filter(|s| s.status == PlanStatus::InProgress)
        .count();
    if in_progress > 1 {
        advice.push("Keep at most one plan step in_progress.".into());
    }
    if in_progress == 0 && next.iter().any(|s| s.status == PlanStatus::Pending) {
        advice.push("Mark one pending plan step in_progress before starting it.".into());
    }
    PlanTransition { completed, advice }
}

/// The plan snapshot echoed in every `update_plan` result — a stable anchor
/// that survives compaction inside the keep-recent window.
pub(crate) fn format_plan_snapshot(steps: &[PlanStep]) -> String {
    let steps_json: Vec<Value> = steps
        .iter()
        .map(|s| {
            json!({
                "id": s.id,
                "goal": s.goal,
                "status": s.status.as_str(),
            })
        })
        .collect();
    format!(
        "<dex-plan task_status=\"active\">{}</dex-plan>",
        serde_json::to_string(&json!({ "steps": steps_json })).unwrap_or_default()
    )
}

/// Post-compaction reminder: the parent task is still active; re-plan before
/// continuing. Lists the remaining goals so the fresh plan starts informed.
pub(crate) fn post_compaction_reminder(steps: &[PlanStep]) -> String {
    let mut text = "Online context compaction finished. The parent task is still active. \
        Before continuing work, call update_plan with a fresh plan for the remaining work."
        .to_string();
    let remaining: Vec<&str> = steps
        .iter()
        .filter(|s| s.status != PlanStatus::Completed)
        .map(|s| s.goal.as_str())
        .collect();
    if !remaining.is_empty() {
        text.push_str("\nRemaining work:");
        for goal in remaining {
            text.push_str("\n- ");
            text.push_str(goal);
        }
    }
    text
}

// ---------------------------------------------------------------------------
// Online state — request/boundary/context-pressure bookkeeping
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct OnlineState {
    pub(crate) plan: Vec<PlanStep>,
    /// Provider requests issued since the session (or last reset) started.
    request_count: u64,
    /// `request_count` at the last completed boundary.
    last_boundary_request_count: u64,
    /// Requests spent per completed boundary — the horizon sample.
    completed_boundary_request_counts: Vec<u64>,
    /// Context tokens at the previous request, for the growth-rate estimate.
    last_context_tokens: Option<u64>,
    positive_context_delta_total: u64,
    positive_context_delta_count: u64,
    native_compaction_count: u64,
    /// Outstanding cache re-write debt (tokens-equivalent) from past
    /// compactions, repaid `cache_debt_repayment_tokens` per request.
    cache_debt_tokens: f64,
    cache_debt_repayment_tokens: f64,
}

impl OnlineState {
    /// Compactions recorded so far this session (first compaction gets a
    /// doubled horizon allowance — the economics tests pin this).
    #[cfg(test)]
    pub(crate) fn native_compaction_count(&self) -> u64 {
        self.native_compaction_count
    }

    /// One provider request is about to go out at `context_tokens`.
    pub(crate) fn record_request(&mut self, context_tokens: u64) {
        let delta = self
            .last_context_tokens
            .map_or(0, |last| context_tokens.saturating_sub(last));
        let debt = (self.cache_debt_tokens - self.cache_debt_repayment_tokens).max(0.0);
        self.request_count += 1;
        self.last_context_tokens = Some(context_tokens);
        self.positive_context_delta_total += delta;
        self.positive_context_delta_count += u64::from(delta > 0);
        self.cache_debt_tokens = debt;
        if debt == 0.0 {
            self.cache_debt_repayment_tokens = 0.0;
        }
    }

    /// A plan step completed: record the request interval and adopt the plan.
    pub(crate) fn record_boundary(&mut self, plan: Vec<PlanStep>) {
        let interval = self
            .request_count
            .saturating_sub(self.last_boundary_request_count);
        self.plan = plan;
        self.last_boundary_request_count = self.request_count;
        self.completed_boundary_request_counts.push(interval);
    }

    /// A compaction happened: reset pressure samples, carry the cache debt.
    pub(crate) fn record_compaction(&mut self, debt_tokens: f64, repayment_tokens: f64) {
        self.plan.clear();
        self.last_context_tokens = None;
        self.positive_context_delta_total = 0;
        self.positive_context_delta_count = 0;
        self.native_compaction_count += 1;
        self.cache_debt_tokens = debt_tokens.max(0.0);
        self.cache_debt_repayment_tokens = repayment_tokens.max(0.0);
    }

    /// The user steered/corrected mid-task: the old horizon sample no longer
    /// describes the remaining work, so reset it.
    pub(crate) fn record_correction(&mut self) {
        self.plan.clear();
        self.last_boundary_request_count = self.request_count;
        self.completed_boundary_request_counts.clear();
        self.last_context_tokens = None;
        self.positive_context_delta_total = 0;
        self.positive_context_delta_count = 0;
        self.cache_debt_tokens = 0.0;
        self.cache_debt_repayment_tokens = 0.0;
    }

    fn average_context_token_increment(&self) -> Option<f64> {
        if self.positive_context_delta_count == 0 {
            return None;
        }
        #[allow(clippy::cast_precision_loss)]
        Some(self.positive_context_delta_total as f64 / self.positive_context_delta_count as f64)
    }

    fn remaining_boundaries(&self) -> usize {
        self.plan
            .iter()
            .filter(|s| s.status != PlanStatus::Completed)
            .count()
    }
}

// ---------------------------------------------------------------------------
// Compaction economics (port of economics.ts)
// ---------------------------------------------------------------------------

pub(crate) struct CompactionEconomics {
    pub(crate) remaining_request_scale: f64,
    pub(crate) remaining_request_stddev_k: f64,
    pub(crate) window_reserve_tokens: u64,
    pub(crate) first_compaction_request_scale: f64,
    pub(crate) subsequent_compaction_margin: f64,
}

pub(crate) const DEFAULT_COMPACTION_ECONOMICS: CompactionEconomics = CompactionEconomics {
    remaining_request_scale: 1.0,
    remaining_request_stddev_k: 0.0,
    window_reserve_tokens: 16_384,
    first_compaction_request_scale: 2.0,
    subsequent_compaction_margin: 1.5,
};

pub(crate) struct RequestHorizonEstimate {
    pub(crate) window_request_upper_bound: Option<f64>,
    pub(crate) expected_remaining_requests: f64,
}

/// Project how many provider requests remain: `1 + lower_bound × remaining
/// boundaries`, capped by how many more requests fit in the window at the
/// observed average context growth rate.
pub(crate) fn estimate_remaining_requests(
    completed_boundary_request_counts: &[u64],
    remaining_boundaries: usize,
    context_tokens: u64,
    context_window_tokens: Option<u64>,
    average_context_token_increment: Option<f64>,
    economics: &CompactionEconomics,
) -> RequestHorizonEstimate {
    #[allow(clippy::cast_precision_loss)]
    let mean = completed_boundary_request_counts
        .iter()
        .map(|c| *c as f64)
        .sum::<f64>()
        / completed_boundary_request_counts.len().max(1) as f64;
    let mut lower_bound = mean;
    if economics.remaining_request_stddev_k != 0.0 {
        const MINIMUM_VARIANCE_SAMPLES: usize = 3;
        const SMALL_SAMPLE_SCALE: f64 = 0.5;
        if completed_boundary_request_counts.len() < MINIMUM_VARIANCE_SAMPLES {
            lower_bound *= SMALL_SAMPLE_SCALE;
        } else {
            #[allow(clippy::cast_precision_loss)]
            let variance = completed_boundary_request_counts
                .iter()
                .map(|c| {
                    let d = *c as f64 - mean;
                    d * d
                })
                .sum::<f64>()
                / (completed_boundary_request_counts.len() - 1) as f64;
            lower_bound = (mean - economics.remaining_request_stddev_k * variance.sqrt()).max(0.0);
        }
    }
    let unbounded_expected_remaining_requests = 1.0
        + (lower_bound * remaining_boundaries as f64 * economics.remaining_request_scale).floor();
    let window_request_upper_bound = match (context_window_tokens, average_context_token_increment)
    {
        (Some(window), Some(increment)) if increment > 0.0 => Some(
            ((window.saturating_sub(context_tokens)) as f64 / increment)
                .floor()
                .max(0.0),
        ),
        _ => None,
    };
    let expected_remaining_requests = match window_request_upper_bound {
        Some(upper) => unbounded_expected_remaining_requests.min(upper),
        None => unbounded_expected_remaining_requests,
    };
    RequestHorizonEstimate {
        window_request_upper_bound,
        expected_remaining_requests,
    }
}

pub(crate) struct CompactionDecision {
    pub(crate) compact: bool,
    /// Why the decision came out the way it did (tests + debug logging).
    pub(crate) reason: &'static str,
    pub(crate) write_tokens: u64,
    pub(crate) archive_tokens: u64,
    /// `cacheWriteReadRatio - 1`: the extra per-request cost of re-writing
    /// the retained prefix at cache-write price instead of cache-read price.
    pub(crate) incremental_cache_cost_ratio: Option<f64>,
}

impl CompactionDecision {
    /// Cache re-write debt a compaction at this decision carries: the
    /// retained prefix is re-written at cache-write price (`debt`), repaid
    /// by the per-request token saving (`repayment`) on each subsequent
    /// request. Lives here — next to the economics — not in the turn loop.
    pub(crate) fn cache_debt(&self) -> (f64, f64) {
        let ratio = self.incremental_cache_cost_ratio.unwrap_or(0.0);
        #[allow(clippy::cast_precision_loss)]
        let debt = self.write_tokens as f64 * ratio;
        #[allow(clippy::cast_precision_loss)]
        let repayment = self
            .archive_tokens
            .saturating_sub(NATIVE_SUMMARY_TOKEN_ESTIMATE) as f64;
        (debt, repayment)
    }
}

/// Decide whether compacting now is worth it. Window protection always wins
/// when the context is at the reserve edge; otherwise the economic gate
/// compares the cache re-write cost against the per-request savings over the
/// projected remaining horizon.
#[allow(clippy::too_many_arguments)]
pub(crate) fn decide_compaction(
    write_tokens: u64,
    archive_tokens: u64,
    memo_tokens: u64,
    context_tokens: u64,
    state: &OnlineState,
    context_window_tokens: Option<u64>,
    cache_write_read_ratio: Option<f64>,
    economics: &CompactionEconomics,
) -> CompactionDecision {
    let saving_tokens = archive_tokens.saturating_sub(memo_tokens);
    let incremental_cache_cost_ratio = cache_write_read_ratio.map(|ratio| (ratio - 1.0).max(0.0));
    #[allow(clippy::cast_precision_loss)]
    let breakeven_requests = match (saving_tokens > 0, incremental_cache_cost_ratio) {
        (true, Some(ratio)) => Some(write_tokens as f64 * ratio / saving_tokens as f64),
        _ => None,
    };
    #[allow(clippy::cast_precision_loss)]
    let combined_breakeven_requests = match (saving_tokens > 0, incremental_cache_cost_ratio) {
        (true, Some(ratio)) => {
            Some((state.cache_debt_tokens + write_tokens as f64 * ratio) / saving_tokens as f64)
        }
        _ => None,
    };

    let horizon = estimate_remaining_requests(
        &state.completed_boundary_request_counts,
        state.remaining_boundaries(),
        context_tokens,
        context_window_tokens,
        state.average_context_token_increment(),
        economics,
    );
    let first_compaction = state.native_compaction_count == 0;
    // Reference: min(expected * scale, windowUpper ?? ∞) — the doubled
    // first-compaction allowance is itself capped by the window bound.
    let effective_horizon_requests = if first_compaction {
        let scaled = horizon.expected_remaining_requests * economics.first_compaction_request_scale;
        match horizon.window_request_upper_bound {
            Some(upper) => scaled.min(upper),
            None => scaled,
        }
    } else {
        horizon.expected_remaining_requests
    };

    let window_protection = context_window_tokens.is_some_and(|window| {
        context_tokens >= window.saturating_sub(economics.window_reserve_tokens)
    });
    let base_economic = horizon.expected_remaining_requests > 0.0
        && breakeven_requests.is_some_and(|b| b <= horizon.expected_remaining_requests);
    let first_economic = effective_horizon_requests > 0.0
        && breakeven_requests.is_some_and(|b| b <= effective_horizon_requests);
    let subsequent_margin_open = !first_compaction
        && breakeven_requests.is_some_and(|b| {
            b * economics.subsequent_compaction_margin <= horizon.expected_remaining_requests
        });
    let carried_debt_gate_open = !first_compaction
        && combined_breakeven_requests.is_some_and(|b| b <= horizon.expected_remaining_requests);
    let economic = if first_compaction {
        first_economic
    } else {
        base_economic && subsequent_margin_open && carried_debt_gate_open
    };
    let compressible = saving_tokens > 0;
    let compact = compressible && (window_protection || economic);
    let reason = if !compressible {
        "non_positive_saving"
    } else if window_protection {
        "window_protection"
    } else if economic {
        "economic"
    } else if breakeven_requests.is_none() {
        "cache_ratio_unavailable"
    } else if !first_compaction && base_economic && !subsequent_margin_open {
        "deferred_subsequent_margin"
    } else if !first_compaction && base_economic && !carried_debt_gate_open {
        "deferred_carried_debt"
    } else {
        "deferred_economic"
    };

    CompactionDecision {
        compact,
        reason,
        write_tokens,
        archive_tokens,
        incremental_cache_cost_ratio,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn step(id: &str, status: PlanStatus) -> PlanStep {
        PlanStep {
            id: id.to_string(),
            goal: format!("goal {id}"),
            status,
        }
    }

    #[test]
    fn parse_plan_steps_validates() {
        let ok = json!([
            {"id": "1", "goal": "do a thing", "status": "completed"},
            {"id": "2", "goal": "do another", "status": "in_progress"}
        ]);
        let steps = parse_plan_steps(&ok).unwrap();
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].status, PlanStatus::Completed);

        // duplicate ids
        let dup = json!([
            {"id": "1", "goal": "a", "status": "pending"},
            {"id": "1", "goal": "b", "status": "pending"}
        ]);
        assert!(parse_plan_steps(&dup).is_err());
        // unknown status
        let bad_status = json!([{"id": "1", "goal": "a", "status": "done"}]);
        assert!(parse_plan_steps(&bad_status).is_err());
        // extra keys
        let extra = json!([{"id": "1", "goal": "a", "status": "pending", "x": 1}]);
        assert!(parse_plan_steps(&extra).is_err());
        // empty
        assert!(parse_plan_steps(&json!([])).is_err());
        assert!(parse_plan_steps(&json!("nope")).is_err());
    }

    #[test]
    fn transition_detects_newly_completed_and_advises() {
        let prev = vec![
            step("1", PlanStatus::InProgress),
            step("2", PlanStatus::Pending),
        ];
        let next = vec![
            step("1", PlanStatus::Completed),
            step("2", PlanStatus::InProgress),
            step("3", PlanStatus::Pending),
        ];
        let t = analyze_plan_transition(&prev, &next);
        assert_eq!(t.completed, vec![step("1", PlanStatus::Completed)]);
        // Exactly one in_progress and no goal churn: no advice.
        assert!(t.advice.is_empty(), "{:?}", t.advice);

        // Two in_progress steps draws the hygiene advice.
        let next = vec![
            step("1", PlanStatus::Completed),
            step("2", PlanStatus::InProgress),
            step("3", PlanStatus::InProgress),
        ];
        let t = analyze_plan_transition(&prev, &next);
        assert!(t.advice.iter().any(|a| a.contains("at most one")));
    }

    #[test]
    fn transition_ignores_already_completed() {
        let prev = vec![step("1", PlanStatus::Completed)];
        let next = vec![step("1", PlanStatus::Completed)];
        assert!(analyze_plan_transition(&prev, &next).completed.is_empty());
    }

    #[test]
    fn boundary_records_request_interval() {
        let mut state = OnlineState {
            plan: vec![step("1", PlanStatus::InProgress)],
            ..OnlineState::default()
        };
        for _ in 0..7 {
            state.record_request(100);
        }
        state.record_boundary(vec![step("1", PlanStatus::Completed)]);
        assert_eq!(state.completed_boundary_request_counts, vec![7]);
        assert_eq!(state.last_boundary_request_count, 7);
    }

    #[test]
    fn request_records_positive_deltas_and_repays_debt() {
        let mut state = OnlineState::default();
        state.record_request(100);
        state.record_request(300); // +200
        state.record_request(250); // no growth
        assert_eq!(state.positive_context_delta_total, 200);
        assert_eq!(state.positive_context_delta_count, 1);
        assert_eq!(state.average_context_token_increment(), Some(200.0));

        state.cache_debt_tokens = 100.0;
        state.cache_debt_repayment_tokens = 40.0;
        state.record_request(300);
        assert!((state.cache_debt_tokens - 60.0).abs() < f64::EPSILON);
        state.record_request(300);
        assert!((state.cache_debt_tokens - 20.0).abs() < f64::EPSILON);
        state.record_request(300);
        assert_eq!(state.cache_debt_tokens, 0.0);
        assert_eq!(state.cache_debt_repayment_tokens, 0.0);
    }

    #[test]
    fn compaction_resets_pressure_and_carries_debt() {
        let mut state = OnlineState::default();
        state.record_request(100);
        state.record_boundary(vec![step("1", PlanStatus::Completed)]);
        state.record_compaction(500.0, 100.0);
        assert_eq!(state.native_compaction_count, 1);
        assert!(state.plan.is_empty());
        assert_eq!(state.last_context_tokens, None);
        assert_eq!(state.cache_debt_tokens, 500.0);

        state.record_correction();
        assert_eq!(state.completed_boundary_request_counts, Vec::<u64>::new());
        assert_eq!(state.cache_debt_tokens, 0.0);
    }

    #[test]
    fn horizon_uses_mean_and_window_cap() {
        // One boundary took 10 requests, 2 remain → 1 + 10*2 = 21.
        let econ = &DEFAULT_COMPACTION_ECONOMICS;
        let h = estimate_remaining_requests(&[10], 2, 1_000, None, None, econ);
        assert_eq!(h.expected_remaining_requests, 21.0);

        // Window cap: 9_000 tokens of headroom at 100 tokens/request → 90.
        let h = estimate_remaining_requests(&[10], 2, 1_000, Some(10_000), Some(100.0), econ);
        assert_eq!(h.window_request_upper_bound, Some(90.0));
        assert_eq!(h.expected_remaining_requests, 21.0);

        // Headroom smaller than the horizon caps it.
        let h = estimate_remaining_requests(&[10], 2, 1_000, Some(2_500), Some(100.0), econ);
        assert_eq!(h.expected_remaining_requests, 15.0);
    }

    #[test]
    fn window_protection_fires_at_the_reserve_edge() {
        let state = OnlineState::default();
        let d = decide_compaction(
            100_000,
            80_000,
            NATIVE_SUMMARY_TOKEN_ESTIMATE,
            120_000,
            &state,
            Some(128_000),
            Some(12.5),
            &DEFAULT_COMPACTION_ECONOMICS,
        );
        assert!(d.compact);
        assert_eq!(d.reason, "window_protection");
    }

    #[test]
    fn first_compaction_needs_a_demonstrated_horizon() {
        // No completed boundaries yet: horizon is 1, doubled to 2 for the
        // first compaction. breakeven = write*11.5/saving is far above 2, so
        // the economic gate stays shut even with a large archive.
        let state = OnlineState {
            plan: vec![
                step("1", PlanStatus::InProgress),
                step("2", PlanStatus::Pending),
            ],
            ..OnlineState::default()
        };
        let d = decide_compaction(
            40_000,
            20_000,
            NATIVE_SUMMARY_TOKEN_ESTIMATE,
            40_000,
            &state,
            Some(128_000),
            Some(12.5),
            &DEFAULT_COMPACTION_ECONOMICS,
        );
        assert!(!d.compact);
        assert_eq!(d.reason, "deferred_economic");
    }

    #[test]
    fn first_compaction_horizon_is_capped_by_the_window() {
        // expected = 1 + 10×2 = 21, doubled to 42 for the first compaction,
        // but the window only fits 4 more requests at the observed growth
        // rate. The reference caps the doubled horizon at the window bound —
        // min(42, 4) = 4 — so a breakeven of ~6 stays shut. (Scaling the cap
        // instead, min(21, 8) = 8, would wrongly open the gate.)
        let state = OnlineState {
            completed_boundary_request_counts: vec![10],
            positive_context_delta_total: 5_000,
            positive_context_delta_count: 1,
            plan: vec![
                step("1", PlanStatus::Completed),
                step("2", PlanStatus::Pending),
                step("3", PlanStatus::Pending),
            ],
            ..OnlineState::default()
        };
        // breakeven = 5_217 * 11.5 / 10_000 ≈ 6.0; window headroom 20_000 at
        // 5_000 tokens/request → upper bound 4.
        let d = decide_compaction(
            5_217,
            11_000,
            NATIVE_SUMMARY_TOKEN_ESTIMATE,
            5_217,
            &state,
            Some(25_217),
            Some(12.5),
            &DEFAULT_COMPACTION_ECONOMICS,
        );
        assert!(!d.compact);
        assert_eq!(d.reason, "deferred_economic");
    }

    #[test]
    fn economic_gate_opens_on_a_long_horizon() {
        // Boundary took 30 requests, 2 remain → horizon 61. write 40k,
        // saving 19k → breakeven = 40_000*11.5/19_000 ≈ 24.2 ≤ 61.
        let state = OnlineState {
            completed_boundary_request_counts: vec![30],
            plan: vec![
                step("1", PlanStatus::Completed),
                step("2", PlanStatus::Pending),
            ],
            ..OnlineState::default()
        };
        let d = decide_compaction(
            40_000,
            20_000,
            NATIVE_SUMMARY_TOKEN_ESTIMATE,
            40_000,
            &state,
            Some(128_000),
            Some(12.5),
            &DEFAULT_COMPACTION_ECONOMICS,
        );
        assert!(d.compact);
        assert_eq!(d.reason, "economic");
    }

    #[test]
    fn subsequent_compactions_demand_a_margin_and_clear_debt() {
        // After one compaction the margin gate demands breakeven*1.5 ≤ horizon.
        // breakeven ≈ 24.2; counts=[12], 2 remaining → horizon 25 passes the
        // base gate but not the margin (36.3) → deferred on the margin.
        let state = OnlineState {
            completed_boundary_request_counts: vec![12],
            plan: vec![
                step("2", PlanStatus::Pending),
                step("3", PlanStatus::Pending),
            ],
            native_compaction_count: 1,
            ..OnlineState::default()
        };
        let d = decide_compaction(
            40_000,
            20_000,
            NATIVE_SUMMARY_TOKEN_ESTIMATE,
            40_000,
            &state,
            Some(128_000),
            Some(12.5),
            &DEFAULT_COMPACTION_ECONOMICS,
        );
        assert!(!d.compact);
        assert_eq!(d.reason, "deferred_subsequent_margin");

        // Long horizon clears the margin; carried debt raises the bar but
        // 61 still covers (debt 5_000 + 40_000*11.5)/19_000 ≈ 26.8.
        let state = OnlineState {
            completed_boundary_request_counts: vec![30],
            plan: vec![
                step("2", PlanStatus::Pending),
                step("3", PlanStatus::Pending),
            ],
            native_compaction_count: 1,
            cache_debt_tokens: 5_000.0,
            ..OnlineState::default()
        };
        let d = decide_compaction(
            40_000,
            20_000,
            NATIVE_SUMMARY_TOKEN_ESTIMATE,
            40_000,
            &state,
            Some(128_000),
            Some(12.5),
            &DEFAULT_COMPACTION_ECONOMICS,
        );
        assert!(d.compact);
        assert_eq!(d.reason, "economic");
    }

    #[test]
    fn non_positive_saving_never_compacts() {
        let state = OnlineState::default();
        // archive <= memo: nothing to save even at the window edge.
        let d = decide_compaction(
            100_000,
            900,
            NATIVE_SUMMARY_TOKEN_ESTIMATE,
            120_000,
            &state,
            Some(128_000),
            Some(12.5),
            &DEFAULT_COMPACTION_ECONOMICS,
        );
        assert!(!d.compact);
        assert_eq!(d.reason, "non_positive_saving");
    }

    #[test]
    fn missing_cache_ratio_keeps_window_protection_only() {
        let state = OnlineState {
            completed_boundary_request_counts: vec![30],
            plan: vec![step("2", PlanStatus::Pending)],
            ..OnlineState::default()
        };
        let d = decide_compaction(
            40_000,
            20_000,
            NATIVE_SUMMARY_TOKEN_ESTIMATE,
            40_000,
            &state,
            Some(128_000),
            None,
            &DEFAULT_COMPACTION_ECONOMICS,
        );
        assert!(!d.compact);
        assert_eq!(d.reason, "cache_ratio_unavailable");
    }

    #[test]
    fn reminder_lists_remaining_work() {
        let steps = vec![
            step("1", PlanStatus::Completed),
            step("2", PlanStatus::InProgress),
        ];
        let text = post_compaction_reminder(&steps);
        assert!(text.contains("call update_plan"));
        assert!(text.contains("- goal 2"));
    }
}
