//! Client-side usage totals collected from daemon stream events.

#[derive(Clone, Default)]
pub(crate) struct UsageState {
    pub(crate) last_usage: Option<u64>,
    pub(crate) last_cached: Option<u64>,
    pub(crate) total_usage: u64,
    pub(crate) total_output: u64,
    pub(crate) total_cost: f64,
    pub(crate) last_tok_s: Option<f64>,
    pub(crate) verify_dirty: bool,
}
