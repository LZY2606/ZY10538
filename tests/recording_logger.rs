//! Boundary tests for the `log!` call chain: macro expansion, static level
//! pruning, runtime level filtering, and dispatch into the global logger.
//!
//! Determinism notes:
//! * Every expectation that depends on compile-time pruning is derived from
//!   `log::STATIC_MAX_LEVEL`, so the same assertions hold under
//!   `cargo test` (static max = Trace) and `cargo test --all-features`
//!   (static max = Off, because `max_level_off` wins the feature match).
//! * Tests that touch the global logger or the runtime max level are
//!   serialized through `GATE`, so results never depend on test scheduling.

use log::{log, log_enabled, Level, LevelFilter, Log, Metadata, Record, STATIC_MAX_LEVEL};
use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, Once};

/// Serializes tests that mutate global logging state.
static GATE: Mutex<()> = Mutex::new(());

/// Locks a mutex, recovering from poisoning. A poisoned mutex here only
/// means another test already panicked (failing the run); the guarded data
/// is a plain event log with no invariants to protect.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// Serializes tests that mutate global logging state.
fn gate() -> MutexGuard<'static, ()> {
    lock(&GATE)
}

/// How many times the side-effecting `Display` was rendered.
static FMT_CALLS: AtomicUsize = AtomicUsize::new(0);

/// How many times the probe argument expression was evaluated.
static PROBE_CALLS: AtomicUsize = AtomicUsize::new(0);

/// A `Display` impl with an observable side effect: it only runs when a
/// logger actually renders the record's message.
struct Counted;

impl fmt::Display for Counted {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        FMT_CALLS.fetch_add(1, Ordering::SeqCst);
        f.write_str("counted")
    }
}

/// An argument expression with an observable side effect: it runs exactly
/// when the macro evaluates the format arguments (after the static and
/// runtime level gates pass), regardless of what the logger does later.
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

/// True when `level` survives compile-time pruning in this build.
fn static_allows(level: Level) -> bool {
    level <= STATIC_MAX_LEVEL
}

#[derive(Debug, PartialEq)]
enum Event {
    Enabled { level: Level, target: String },
    Log {
        level: Level,
        target: String,
        module_path: Option<String>,
        file: Option<String>,
        line: Option<u32>,
        message: String,
        keys: Vec<String>,
    },
    Flush,
}

/// A logger that records every callback it receives.
struct Recorder {
    events: Mutex<Vec<Event>>,
}

static RECORDER: Recorder = Recorder {
    events: Mutex::new(Vec::new()),
};

impl Log for Recorder {
    fn enabled(&self, metadata: &Metadata) -> bool {
        lock(&self.events).push(Event::Enabled {
            level: metadata.level(),
            target: metadata.target().to_string(),
        });
        true
    }

    fn log(&self, record: &Record) {
        lock(&self.events).push(Event::Log {
            level: record.level(),
            target: record.target().to_string(),
            module_path: record.module_path().map(str::to_string),
            file: record.file().map(str::to_string),
            line: record.line(),
            // Rendering the message is what triggers `Counted::fmt`.
            message: record.args().to_string(),
            keys: collect_keys(record),
        });
    }

    fn flush(&self) {
        lock(&self.events).push(Event::Flush);
    }
}

#[cfg(feature = "kv")]
fn collect_keys(record: &Record) -> Vec<String> {
    struct Pairs(Vec<String>);

    impl<'kvs> log::kv::VisitSource<'kvs> for Pairs {
        fn visit_pair(
            &mut self,
            key: log::kv::Key<'kvs>,
            value: log::kv::Value<'kvs>,
        ) -> Result<(), log::kv::Error> {
            self.0.push(format!("{}={}", key, value));
            Ok(())
        }
    }

    let mut pairs = Pairs(Vec::new());
    record.key_values().visit(&mut pairs).unwrap();
    pairs.0
}

#[cfg(not(feature = "kv"))]
fn collect_keys(_: &Record) -> Vec<String> {
    Vec::new()
}

/// Installs the recorder as the global logger exactly once per test binary.
fn init() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        log::set_logger(&RECORDER).expect("first set_logger call must succeed");
        log::set_max_level(LevelFilter::Trace);
    });
}

/// Empties the recorder's event log.
fn drain() -> Vec<Event> {
    lock(&RECORDER.events).drain(..).collect()
}

#[test]
fn global_logger_initializes_exactly_once() {
    let _gate = gate();
    init();
    drain();

    // Negative: the global logger slot is write-once.
    let err = log::set_logger(&RECORDER)
        .expect_err("second set_logger must fail once initialized");
    assert_eq!(
        err.to_string(),
        "attempted to set a logger after the logging system was already initialized"
    );
    // Safety: the logger is already initialized and no other initialization
    // is in flight, so this cannot race; it must report the slot as taken.
    assert!(unsafe { log::set_logger_racy(&RECORDER) }.is_err());

    // The installed logger (not the no-op fallback) now answers enabled().
    assert!(log::logger().enabled(
        &Metadata::builder()
            .level(Level::Error)
            .target("probe")
            .build()
    ));
    assert_eq!(
        drain(),
        [Event::Enabled {
            level: Level::Error,
            target: "probe".to_string()
        }]
    );

    // The runtime filter is observable and updatable.
    log::set_max_level(LevelFilter::Info);
    assert_eq!(log::max_level(), LevelFilter::Info);
    log::set_max_level(LevelFilter::Trace);
    assert_eq!(log::max_level(), LevelFilter::Trace);
}

#[test]
fn runtime_filter_gates_format_argument_evaluation() {
    let _gate = gate();
    init();
    drain();

    // Runtime filter rejects Info: the macro must not evaluate the argument
    // expression, must not render anything, and must not call the logger.
    log::set_max_level(LevelFilter::Error);
    let fmt_before = fmt_calls();
    let probe_before = probe_calls();
    log::info!("{}", probe());
    log::info!("{}", Counted);
    assert_eq!(
        probe_calls(),
        probe_before,
        "argument expression evaluated although runtime filter rejected the record"
    );
    assert_eq!(
        fmt_calls(),
        fmt_before,
        "Display rendered although runtime filter rejected the record"
    );
    assert_eq!(
        drain(),
        [],
        "logger called although runtime filter rejected the record"
    );

    // Runtime filter now admits Info: each dispatched macro call evaluates
    // its argument expression once and the recorder renders it once.
    log::set_max_level(LevelFilter::Info);
    let dispatched = static_allows(Level::Info) as usize;
    let probe_line = line!() + 1;
    log::info!("{}", probe());
    let counted_line = line!() + 1;
    log::info!("{}", Counted);
    assert_eq!(probe_calls() - probe_before, dispatched);
    assert_eq!(fmt_calls() - fmt_before, dispatched);
    let events = drain();
    if static_allows(Level::Info) {
        match &events[..] {
            [probe_event, counted_event] => {
                assert_eq!(probe_event.level(), Level::Info);
                assert_eq!(probe_event.target(), module_path!());
                assert_eq!(probe_event.module_path(), Some(module_path!()));
                assert_eq!(probe_event.file(), Some(file!()));
                assert_eq!(probe_event.line(), Some(probe_line));
                assert_eq!(probe_event.message(), "probe");

                assert_eq!(counted_event.line(), Some(counted_line));
                assert_eq!(counted_event.message(), "counted");
            }
            other => panic!("expected exactly two Log events, got {:?}", other),
        }
    } else {
        assert_eq!(events, [], "statically pruned build must not dispatch");
    }

    // Trace is the first level dropped by partial static limits
    // (e.g. `max_level_debug`): observable only through the probe.
    log::set_max_level(LevelFilter::Trace);
    let probe_before = probe_calls();
    log::trace!("{}", probe());
    assert_eq!(probe_calls() - probe_before, static_allows(Level::Trace) as usize);
    let events = drain();
    if !static_allows(Level::Trace) {
        assert_eq!(events, [], "trace must be pruned when STATIC_MAX_LEVEL < Trace");
    }
}

#[test]
fn record_carries_callsite_metadata() {
    let _gate = gate();
    init();
    drain();
    log::set_max_level(LevelFilter::Trace);

    let want_line = line!() + 1;
    log!(target: "rec_target", Level::Warn, "meta {}", Counted);
    // Default target is the caller's module path.
    log::warn!("default target");
    let events = drain();

    if static_allows(Level::Warn) {
        match &events[..] {
            [explicit, default] => {
                assert_eq!(
                    (explicit.level(), explicit.target()),
                    (Level::Warn, "rec_target")
                );
                assert_eq!(explicit.module_path(), Some(module_path!()));
                assert_eq!(explicit.file(), Some(file!()));
                assert_eq!(explicit.line(), Some(want_line));
                assert_eq!(explicit.message(), "meta counted");

                assert_eq!(default.target(), module_path!());
                assert_eq!(default.module_path(), Some(module_path!()));
            }
            other => panic!("expected exactly two Log events, got {:?}", other),
        }
    } else {
        assert_eq!(events, [], "statically pruned build must not dispatch");
    }
}

impl Event {
    fn level(&self) -> Level {
        match self {
            Event::Log { level, .. } => *level,
            _ => panic!("expected a Log event, got {:?}", self),
        }
    }
    fn target(&self) -> &str {
        match self {
            Event::Log { target, .. } => target,
            _ => panic!("expected a Log event, got {:?}", self),
        }
    }
    fn module_path(&self) -> Option<&str> {
        match self {
            Event::Log { module_path, .. } => module_path.as_deref(),
            _ => panic!("expected a Log event, got {:?}", self),
        }
    }
    fn file(&self) -> Option<&str> {
        match self {
            Event::Log { file, .. } => file.as_deref(),
            _ => panic!("expected a Log event, got {:?}", self),
        }
    }
    fn line(&self) -> Option<u32> {
        match self {
            Event::Log { line, .. } => *line,
            _ => panic!("expected a Log event, got {:?}", self),
        }
    }
    fn message(&self) -> &str {
        match self {
            Event::Log { message, .. } => message,
            _ => panic!("expected a Log event, got {:?}", self),
        }
    }
}

#[test]
fn log_enabled_short_circuits_before_consulting_logger() {
    let _gate = gate();
    init();
    drain();

    // Rejected by the runtime filter: the logger's enabled() is not consulted.
    log::set_max_level(LevelFilter::Info);
    assert!(!log_enabled!(Level::Debug));
    assert_eq!(
        drain(),
        [],
        "enabled() must not be called when the runtime filter rejects the level"
    );

    // Admitted by both gates: the logger's enabled() is consulted exactly once.
    let expected = static_allows(Level::Info);
    assert_eq!(log_enabled!(Level::Info), expected);
    let events = drain();
    if expected {
        assert_eq!(
            events,
            [Event::Enabled {
                level: Level::Info,
                target: module_path!().to_string()
            }]
        );
    } else {
        assert_eq!(events, [], "statically pruned build must not call enabled()");
    }
}

#[cfg(feature = "kv")]
#[test]
fn kv_pairs_are_attached_to_the_record() {
    let _gate = gate();
    init();
    drain();
    log::set_max_level(LevelFilter::Trace);

    log!(target: "kv_target", Level::Error, answer = 42u64, tag = "kv"; "kv message");
    let events = drain();

    if static_allows(Level::Error) {
        match &events[..] {
            [event] => {
                assert_eq!(event.target(), "kv_target");
                assert_eq!(event.message(), "kv message");
                assert_eq!(
                    event.keys(),
                    ["answer=42".to_string(), "tag=kv".to_string()]
                );
            }
            other => panic!("expected exactly one Log event, got {:?}", other),
        }
    } else {
        assert_eq!(events, [], "statically pruned build must not dispatch");
    }
}

#[cfg(feature = "kv")]
impl Event {
    fn keys(&self) -> Vec<String> {
        match self {
            Event::Log { keys, .. } => keys.clone(),
            _ => panic!("expected a Log event, got {:?}", self),
        }
    }
}

/// Negative: without the `kv` feature the macros reject kv syntax at compile
/// time, and the private dispatch API rejects kv pairs at runtime.
#[cfg(not(feature = "kv"))]
#[test]
#[should_panic(expected = "key-value support is experimental")]
fn kv_pairs_without_kv_feature_are_rejected() {
    let _gate = gate();
    init();
    log::set_max_level(LevelFilter::Trace);
    log::__private_api::log(
        log::__private_api::GlobalLogger,
        format_args!("kv rejected"),
        Level::Error,
        &("kv_target", module_path!(), log::__private_api::loc()),
        &[("answer", "42")] as &[(&str, &str)],
    );
}

#[test]
fn flush_reaches_the_global_logger() {
    let _gate = gate();
    init();
    drain();
    log::logger().flush();
    assert_eq!(drain(), [Event::Flush]);
}
