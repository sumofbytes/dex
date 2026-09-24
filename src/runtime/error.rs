//! Shared error formatting for user-facing diagnostics and logs.

/// Format an error together with its full source chain.
pub(crate) fn chain_message(error: &(dyn std::error::Error + 'static)) -> String {
    let mut message = error.to_string();
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    message
}
