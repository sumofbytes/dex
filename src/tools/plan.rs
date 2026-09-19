//! Working-plan types + validation/snapshot: moved down from `agent::online_compaction`
//! so `tools` no longer imports `agent` (direction `agent→tools`).

use serde_json::{json, Value};
use std::collections::HashSet;

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

/// Progress evidence attached to a plan update (schema-optional). Kept
/// beside the plan so a compaction reminder can carry it forward.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct PlanProgress {
    pub(crate) files_changed: Vec<String>,
    pub(crate) verification: Vec<String>,
    pub(crate) decisions: Vec<String>,
}

impl PlanProgress {
    fn is_empty(&self) -> bool {
        self.files_changed.is_empty() && self.verification.is_empty() && self.decisions.is_empty()
    }
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

/// Validate a `progress` object: bounded strings, string arrays only.
/// Evidence attached to a plan update; surfaced in the snapshot and kept
/// through compaction so the post-compaction reminder retains it.
pub(crate) fn parse_plan_progress(value: Option<&Value>) -> Result<PlanProgress, String> {
    const MAX_ITEMS: usize = 64;
    let Some(value) = value else {
        return Ok(PlanProgress::default());
    };
    let Some(obj) = value.as_object() else {
        return Err("progress must be an object".into());
    };
    let list = |key: &str| -> Result<Vec<String>, String> {
        match obj.get(key) {
            None => Ok(Vec::new()),
            Some(Value::Array(items)) if items.len() <= MAX_ITEMS => items
                .iter()
                .map(|item| {
                    item.as_str()
                        .filter(|s| !s.is_empty() && s.len() <= MAX_PLAN_STRING_BYTES)
                        .map(str::to_string)
                        .ok_or_else(|| format!("progress {key} items must be non-empty strings"))
                })
                .collect(),
            Some(_) => Err(format!("progress {key} must be an array of strings")),
        }
    };
    Ok(PlanProgress {
        files_changed: list("files_changed")?,
        verification: list("verification")?,
        decisions: list("decisions")?,
    })
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
/// that survives compaction inside the keep-recent window. Progress evidence
/// rides along when the model supplied it, so the reminder after a
/// compaction retains what was done and how it was verified.
pub(crate) fn format_plan_snapshot(steps: &[PlanStep], progress: &PlanProgress) -> String {
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
    let mut body = json!({ "steps": steps_json });
    if !progress.is_empty() {
        body["progress"] = json!({
            "files_changed": progress.files_changed,
            "verification": progress.verification,
            "decisions": progress.decisions,
        });
    }
    format!(
        "<dex-plan task_status=\"active\">{}</dex-plan>",
        serde_json::to_string(&body).unwrap_or_default()
    )
}

/// Post-compaction reminder: the parent task is still active; re-plan before
/// continuing. Lists the remaining goals so the fresh plan starts informed,
/// plus the accumulated progress evidence (files changed, verification run,
/// decisions made) so the re-plan doesn't lose what was already done.
pub(crate) fn post_compaction_reminder(steps: &[PlanStep], progress: &PlanProgress) -> String {
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
    if !progress.is_empty() {
        text.push_str("\nProgress so far:");
        for f in &progress.files_changed {
            text.push_str("\nFiles changed: ");
            text.push_str(f);
        }
        for v in &progress.verification {
            text.push_str("\nVerified: ");
            text.push_str(v);
        }
        for d in &progress.decisions {
            text.push_str("\nDecided: ");
            text.push_str(d);
        }
    }
    text
}

// ---------------------------------------------------------------------------
// Online state — request/boundary/context-pressure bookkeeping
// ---------------------------------------------------------------------------
