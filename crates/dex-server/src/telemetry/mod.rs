//! Telemetry: spend summary, session plot, self-update.

pub mod self_update;
pub mod session_plot;

pub fn spend_summary(usage: u64, output: u64, cost: f64) -> Option<String> {
    if usage == 0 && output == 0 {
        return None;
    }
    let mut s = format!("[dex] {usage} prompt / {output} output tokens");
    if cost > 5e-4 {
        s.push_str(&format!(" · ${cost:.3}"));
    }
    Some(s)
}
