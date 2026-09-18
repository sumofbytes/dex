//! Runtime: process-global ish (console, logging, unwind).
pub(crate) mod console {
    pub(crate) use crate::core::console::*;
}
pub(crate) mod logging {
    pub(crate) use crate::core::logging::*;
}
