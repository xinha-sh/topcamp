//! Shared stderr subscriber for the two binaries (`serve`, `worker`).
//!
//! One line per event (`level target: message fields`). Deliberately
//! dependency-free — `tracing-subscriber` would drag `regex-automata` in
//! for env-filter.

/// Minimal stderr subscriber; see module docs.
struct Stderr;

impl tracing::Subscriber for Stderr {
    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        // `RUST_LOG=debug` (or `trace`) opens the verbose levels; anything
        // else (or unset) keeps `info` and above.
        let verbose = matches!(
            std::env::var("RUST_LOG").as_deref(),
            Ok("debug") | Ok("trace")
        );
        if verbose {
            true
        } else {
            !metadata.is_event() || *metadata.level() <= tracing::Level::INFO
        }
    }

    fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        use std::io::Write as _;
        use tracing::field::{Field, Visit};
        struct Writer {
            message: String,
            fields: Vec<String>,
        }
        impl Visit for Writer {
            fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
                if field.name() == "message" {
                    self.message = format!("{value:?}");
                } else {
                    self.fields.push(format!("{}={value:?}", field.name()));
                }
            }
        }
        let mut out = Writer {
            message: String::new(),
            fields: Vec::new(),
        };
        event.record(&mut out);
        let _ = writeln!(
            std::io::stderr(),
            "{} {}: {} {}",
            event.metadata().level(),
            event.metadata().target(),
            out.message,
            out.fields.join(" ")
        );
    }

    fn enter(&self, _span: &tracing::span::Id) {}
    fn exit(&self, _span: &tracing::span::Id) {}
}

/// Install the process-global subscriber. Fails only if one is already
/// set (e.g. a test harness installed its own).
pub fn init_stderr() -> Result<(), &'static str> {
    tracing::subscriber::set_global_default(Stderr)
        .map_err(|_| "a global subscriber is already set")
}
