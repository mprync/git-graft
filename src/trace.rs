//! Opt-in phase timing for performance work.
//!
//! Set `GIT_GRAFT_TRACE=1` to print the time since process start at each
//! marked phase to standard error. Marks cost one atomic load when disabled.
use std::{sync::OnceLock, time::Instant};

static START: OnceLock<Option<Instant>> = OnceLock::new();

/// Record that `phase` finished.
pub(crate) fn mark(phase: &str) {
    let start = START.get_or_init(|| std::env::var_os("GIT_GRAFT_TRACE").map(|_| Instant::now()));
    if let Some(start) = start {
        eprintln!(
            "git-graft trace {:9.2} ms  {phase}",
            start.elapsed().as_secs_f64() * 1e3
        );
    }
}
