//! Tracks metadata for a loading/rendering session.

use std::sync::Arc;

use crate::log;

/// Metadata for a loading/rendering session.
///
/// When the calling program first uses one of the API entry points (e.g. `Loader::new()`
/// in the Rust API, or `rsvg_handle_new()` in the C API), there is no context yet where
/// librsvg's code may start to track things.  This struct provides that context.
#[derive(Clone)]
pub struct Session {
    inner: Arc<SessionInner>,
}

struct SessionInner {
    log_enabled: bool,
}

impl Session {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(SessionInner {
                log_enabled: log::log_enabled(),
            }),
        }
    }

    #[cfg(test)]
    pub fn new_for_test_suite() -> Self {
        Self {
            inner: Arc::new(SessionInner { log_enabled: false }),
        }
    }

    pub fn log_enabled(&self) -> bool {
        self.inner.log_enabled
    }
}
