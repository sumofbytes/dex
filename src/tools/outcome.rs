#[derive(Clone, Debug)]
pub(crate) struct ToolOutcome {
    pub(crate) text: String,
    pub(crate) ok: bool,
    pub(crate) diff: Option<String>,
}
