//! Re-exports from the kernel (`runtime::http`): process-wide async
//! runtime + shared HTTP clients. Existing `client::http::...` and
//! `client::runtime::...` paths keep working.

pub(crate) use crate::runtime::http::{block_on, shared_async_client, shared_streaming_client};
