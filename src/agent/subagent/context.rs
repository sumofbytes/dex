use std::path::PathBuf;

/// The isolated input a child is given (plan §5). The parent's transcript
/// is never copied — whatever the child needs arrives here, written by the
/// parent model into the `delegate` call.
#[derive(Clone, Debug)]
pub(crate) struct ContextSeed {
    /// Required: what the child must do, in the parent model's own words.
    pub(crate) task: String,
    /// Optional workspace-relative file hints.
    pub(crate) file_hints: Vec<PathBuf>,
    /// Optional model-written background the child can't get otherwise.
    pub(crate) parent_summary: Option<String>,
}
