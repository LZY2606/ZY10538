//! Boundary: the process never installs a global logger.
//!
//! This scenario lives in its own integration test binary so that no other
//! test can call `set_logger` first; execution order within the binary is a
//! single test, so the result never depends on scheduling.

use log::{Level, LevelFilter, Metadata, STATIC_MAX_LEVEL};
use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};

static FMT_CALLS: AtomicUsize = AtomicUsize::new(0);
static PROBE_CALLS: AtomicUsize = AtomicUsize::new(0);

/// Side-effecting `Display`: runs only if a logger renders the message.
struct Counted;

impl fmt::Display for Counted {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        FMT_CALLS.fetch_add(1, Ordering::SeqCst);
        f.write_str("counted")
    }
}

/// Side-effecting argument expression: runs exactly when the macro evaluates
/// the format arguments, i.e. after the static and runtime level gates pass.
fn probe() -> &'static str {
    PROBE_CALLS.fetch_add(1, Ordering::SeqCst);
    "probe"
}

fn fmt_calls() -> usize {
    FMT_CALLS.load(Ordering::SeqCst)
}

fn probe_calls() -> usize {
    PROBE_CALLS.load(Ordering::SeqCst)
}

#[test]
fn uninitialized_global_logger_boundary() {
    // Phase 1: fresh process state. The runtime filter defaults to Off, so
    // the macro short-circuits before evaluating any format argument.
    assert_eq!(log::max_level(), LevelFilter::Off);
    log::error!("{}", probe());
    log::error!("{}", Counted);
    assert_eq!(probe_calls(), 0, "args evaluated although max_level() == Off");
    assert_eq!(fmt_calls(), 0, "message rendered although max_level() == Off");

    // The global facade falls back to a no-op logger: enabled() is false and
    // flush() is a harmless no-op.
    assert!(!log::logger().enabled(
        &Metadata::builder()
            .level(Level::Error)
            .target("probe")
            .build()
    ));
    assert!(!log::log_enabled!(Level::Error));
    log::logger().flush();

    // Phase 2: raising the runtime filter opens the macro gates. The argument
    // expression is now evaluated and the record is built and dispatched,
    // but the no-op logger drops it without rendering.
    log::set_max_level(LevelFilter::Trace);
    let dispatched = (Level::Info <= STATIC_MAX_LEVEL) as usize;
    log::info!("{}", probe());
    log::info!("{}", Counted);
    assert_eq!(
        probe_calls(),
        dispatched,
        "argument expression must be evaluated exactly once per dispatched macro call"
    );
    assert_eq!(
        fmt_calls(),
        0,
        "no-op logger must never render the message"
    );
    // log_enabled! still bottoms out at the no-op logger's enabled().
    assert!(!log::log_enabled!(Level::Info));
}
