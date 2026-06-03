//! Activity dump on `SIGQUIT` (Ctrl-\).
//!
//! When bougie appears to hang — a slow metadata fetch, a long pubgrub
//! solve, a stalled dist extract — pressing **Ctrl-\** prints what each
//! thread is currently doing to stderr, then keeps running. It's the
//! JVM thread-dump model: `SIGQUIT` is repurposed so it reports instead
//! of dropping a core.
//!
//! ## Why semantic breadcrumbs, not native backtraces
//!
//! The release binary is built with `strip = "symbols"` + `lto = "fat"`,
//! so a symbolized native backtrace would be unreadable, and capturing
//! all-thread native stacks needs `unsafe` (signal handler + ptrace),
//! which the workspace's `unsafe_code = "deny"` forbids. Instead we
//! piggyback on `tracing`: a [`Layer`] records the stack of currently
//! *entered* spans per thread, and the dump walks those. A
//! synchronous blocking call keeps its span entered for its whole
//! duration, so the dump shows exactly where a thread is wedged — with
//! the span's fields (e.g. the package being fetched) and how long it's
//! been stuck there.
//!
//! The major blocking phases are instrumented (command dispatch, the
//! resolver's prefetch + pubgrub solve, dist download/extract, fetch +
//! archive extraction, PHP/extension install, the autoloader scan, and
//! service provisioning / recipe steps). Add `#[instrument]` or
//! `tracing::*_span!` at any new blocking site and it shows up here for
//! free (and in `BOUGIE_LOG` output too). The same spans that power
//! `BOUGIE_LOG=...=debug` timing logs feed this dump.
//!
//! ## Caveat: Ctrl-\ signals the whole foreground process group
//!
//! `SIGQUIT` from the terminal is delivered by the kernel to *every*
//! process in the foreground process group, not just the one you're
//! watching. bougie's own processes (the CLI, `bougied`, `bougie-babysit`)
//! all install this handler and survive — they dump and keep running.
//! But bougie also shells out: the recipe runner spawns `/bin/sh -c`
//! per task (`bougie-recipe::run::execute`), and `bougie run` execs the
//! user's program. Those are **not** bougie and have no handler, so a
//! Ctrl-\ fired while one is alive hits it with `SIGQUIT`'s default
//! disposition — terminate. A killed `/bin/sh` makes the recipe step it
//! was running report `exit -1` (signal death → `code()` is `None`),
//! which looks like the step *failed* even though the real work (e.g.
//! `bougied` provisioning services in its own session, which the signal
//! never reached) completed fine.
//!
//! So: prefer Ctrl-\ during long *single-process* phases (resolve,
//! metadata fetch, dist extract, autoloader scan). During `make` /
//! `up` / `run` — anything that shells out — a Ctrl-\ can spuriously
//! fail the shelled-out step. (Insulating those children in their own
//! process group would fix it but also stop Ctrl-C from reaching them;
//! ignoring `SIGQUIT` in the child needs `pre_exec`, which the unsafe
//! policy forbids. Hence: documented, not worked around.)

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::Write as _;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::ThreadId;
use std::time::Instant;

use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id};
use tracing::Subscriber;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::Layer;

/// One entered (not yet exited) span on some thread's current stack.
#[derive(Debug)]
struct Frame {
    name: &'static str,
    target: &'static str,
    /// Pre-formatted `key=value` field list captured at span creation.
    fields: String,
    /// When this thread entered the span — drives the "held" duration.
    entered: Instant,
}

/// A live thread's name plus a handle to its span stack. The stack is
/// behind its own `Mutex` so the hot path (enter/exit) only ever touches
/// an uncontended per-thread lock; the global [`registry`] lock is taken
/// just once per thread (at registration) and again during a dump.
#[derive(Clone, Debug)]
struct ThreadStack {
    name: String,
    frames: Arc<Mutex<Vec<Frame>>>,
}

fn registry() -> &'static Mutex<HashMap<ThreadId, ThreadStack>> {
    static REGISTRY: OnceLock<Mutex<HashMap<ThreadId, ThreadStack>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

thread_local! {
    /// This thread's span stack, registered globally on first use. The
    /// `registry` keeps a clone of the `Arc`, so a dump from another
    /// thread can read it. When the thread dies the registry entry is
    /// left behind but its `frames` are empty (all spans exited), and
    /// the dump skips empty stacks — so stale entries never print.
    static LOCAL: Arc<Mutex<Vec<Frame>>> = {
        let frames = Arc::new(Mutex::new(Vec::new()));
        let current = std::thread::current();
        let name = current
            .name()
            .map_or_else(|| format!("{:?}", current.id()), str::to_owned);
        registry().lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(
            current.id(),
            ThreadStack { name, frames: frames.clone() },
        );
        frames
    };
}

/// Formats a span's fields into a compact `k=v k=v` string, rendering
/// the conventional `message` field bare (no `message=` prefix).
struct FieldVisitor(String);

impl Visit for FieldVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if !self.0.is_empty() {
            self.0.push(' ');
        }
        if field.name() == "message" {
            let _ = write!(self.0, "{value:?}");
        } else {
            let _ = write!(self.0, "{}={value:?}", field.name());
        }
    }
}

/// Field snapshot stashed in a span's extensions at creation, so
/// `on_enter` can read it without re-visiting the attributes.
struct SpanFields(String);

/// The [`Layer`] that maintains per-thread span stacks. It carries no
/// per-layer filter, so it observes every span regardless of
/// `BOUGIE_LOG` — the dump must work even when logging is off.
#[derive(Debug)]
pub struct ActivityLayer;

impl<S> Layer<S> for ActivityLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let mut visitor = FieldVisitor(String::new());
        attrs.record(&mut visitor);
        if let Some(span) = ctx.span(id) {
            span.extensions_mut().insert(SpanFields(visitor.0));
        }
    }

    fn on_enter(&self, id: &Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };
        let meta = span.metadata();
        let fields = span
            .extensions()
            .get::<SpanFields>()
            .map_or_else(String::new, |f| f.0.clone());
        let frame = Frame {
            name: meta.name(),
            target: meta.target(),
            fields,
            entered: Instant::now(),
        };
        LOCAL.with(|frames| {
            frames
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(frame);
        });
    }

    fn on_exit(&self, _id: &Id, _ctx: Context<'_, S>) {
        // `tracing` guarantees enter/exit are balanced and stack-nested
        // per thread, so the span being exited is the top of our stack.
        LOCAL.with(|frames| {
            frames
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pop();
        });
    }
}

/// The activity-tracking layer to compose into the subscriber. Always
/// active (no filter) so a dump is useful even without `BOUGIE_LOG`.
#[must_use]
pub fn layer() -> ActivityLayer {
    ActivityLayer
}

/// Spawn the background thread that turns `SIGQUIT` (Ctrl-\) into an
/// activity dump. Registering a handler also overrides the default
/// terminate-and-core-dump action, so Ctrl-\ becomes a no-quit
/// "what are you doing?" probe. Call once, after the subscriber is set.
pub fn install_signal_handler() {
    use signal_hook::consts::SIGQUIT;
    use signal_hook::iterator::Signals;

    // Eagerly create the registry so the dump thread never races a
    // not-yet-initialized `OnceLock`.
    let _ = registry();

    let Ok(mut signals) = Signals::new([SIGQUIT]) else {
        return;
    };
    let _ = std::thread::Builder::new()
        .name("bougie-sigquit-dump".to_owned())
        .spawn(move || {
            // `forever()` yields once per delivered SIGQUIT on this
            // ordinary thread — never in async-signal context — so the
            // dump may freely lock mutexes and write to stderr.
            for _ in signals.forever() {
                dump_activity();
            }
        });
}

/// Render every thread's active span stack to stderr.
fn dump_activity() {
    let text = render(Instant::now());
    let stderr = std::io::stderr();
    let mut lock = stderr.lock();
    let _ = lock.write_all(text.as_bytes());
    let _ = lock.flush();
}

/// Build the dump text for a given "now". Split out from [`dump_activity`]
/// so it can be unit-tested without a real signal or stderr.
fn render(now: Instant) -> String {
    let reg = registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    let mut out = String::new();
    out.push_str("\n=== bougie activity dump (SIGQUIT / Ctrl-\\) ===\n");
    let _ = writeln!(out, "pid {}", std::process::id());

    let mut any = false;
    for stack in reg.values() {
        let frames = stack
            .frames
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if frames.is_empty() {
            continue;
        }
        any = true;
        let _ = writeln!(out, "\nthread \"{}\":", stack.name);
        for (depth, frame) in frames.iter().enumerate() {
            let indent = "  ".repeat(depth + 1);
            let held = now.saturating_duration_since(frame.entered);
            let _ = write!(out, "{indent}{} [{}] ({:.2?} held)", frame.name, frame.target, held);
            if frame.fields.is_empty() {
                out.push('\n');
            } else {
                let _ = writeln!(out, "  {}", frame.fields);
            }
        }
    }

    if !any {
        out.push_str(
            "\n(no instrumented activity in progress — bougie may be blocked in an\n\
             un-instrumented call. Re-run with BOUGIE_LOG=debug for more detail.)\n",
        );
    }
    out.push_str("===============================================\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::layer::SubscriberExt as _;

    // `render` reads the process-global thread registry, so the two
    // tests below must not observe each other's spans. Serialize them.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn dump_shows_entered_span_with_fields() {
        let _serial = TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let subscriber = tracing_subscriber::registry().with(layer());
        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("prefetch_closure", packages = 42);
            let _entered = span.enter();
            let text = render(Instant::now());
            assert!(text.contains("prefetch_closure"), "{text}");
            assert!(text.contains("packages=42"), "{text}");
            assert!(text.contains("held"), "{text}");
        });
        // Once the span is exited (guard dropped) it leaves the stack.
        let after = render(Instant::now());
        assert!(!after.contains("prefetch_closure"), "{after}");
    }

    #[test]
    fn dump_reports_idle_when_no_spans_entered() {
        let _serial = TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        // No active spans on this fresh thread → the idle hint.
        let text = std::thread::spawn(|| render(Instant::now()))
            .join()
            .unwrap();
        assert!(text.contains("no instrumented activity"), "{text}");
    }
}
