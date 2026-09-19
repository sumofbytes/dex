//! SSE stream readers. A shared driver owns framing, printing, cancellation,
//! and usage threading; one thin parser per wire protocol translates raw
//! `data:` lines into protocol-agnostic [`StreamEvent`]s. Protocol state
//! (tool-call merging, reasoning replay) stays parser-local, so the driver
//! is written once and a new protocol plugs in as another [`StreamParser`].

#[cfg(test)]
use crate::protocol::SinkLine;
#[cfg(test)]
#[cfg(test)]
use crate::protocol::StreamDelta;
#[cfg(test)]
use crate::protocol::Usage;
#[cfg(test)]
#[cfg(test)]
#[cfg(test)]
use tokio::sync::mpsc;

mod parser;
mod turn;
#[cfg(test)]
pub(crate) use parser::{
    delta_thought, AnthropicParser, ChatCompletionsParser, ResponsesParser, StreamParser,
};
#[cfg(test)]
pub(crate) use turn::{
    driver_err, stream_err, MidStreamError, SseDriver, StreamPrinter, StreamTransportError,
    DEFAULT_STREAM_IDLE_TIMEOUT_SECS, REASONING_STREAM_IDLE_TIMEOUT_SECS,
};
pub(crate) use turn::{
    is_dropped_connection, is_mid_stream, is_stream_idle_error, is_transport_error,
    read_anthropic_stream, read_responses_stream, read_stream, stream_idle_timeout_for, Turn,
};

/// Test-only driver over in-memory lines (no HTTP): parser tests need no
/// server, per plan Phase 1.
#[cfg(test)]
async fn run_sse_lines<P: StreamParser>(
    lines: &[&str],
    sink: Option<mpsc::Sender<SinkLine>>,
    cancel: &(dyn crate::agent::state::CancellationSource + Send + Sync),
    mut parser: P,
) -> Result<Turn, Box<dyn std::error::Error + Send + Sync>> {
    let sink_is_some = sink.is_some();
    if cancel.is_cancelled() {
        return Err(stream_err("cancelled", false));
    }
    let mut driver = SseDriver::new(sink.clone());
    for line in lines {
        if cancel.is_cancelled() {
            return Err(driver_err(&mut driver, "cancelled"));
        }
        // Re-add newline: `feed` expects raw SSE lines.
        let owned = format!("{line}\n");
        if driver.feed_raw_async(&owned, &mut parser).await? {
            break;
        }
    }
    driver.finish_turn_async(parser, sink_is_some).await
}

#[cfg(test)]
mod tests;
