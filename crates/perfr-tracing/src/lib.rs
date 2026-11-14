// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A [`tracing_subscriber`] layer that records `tracing` spans as perfr
//! scopes, `tracing` events as perfr log lines, and the per-frame
//! `tracy.frame_mark` event as a perfr frame mark.
//!
//! Written for Bevy, which puts a `tracing` span around every system,
//! schedule and render pass behind its `trace` cargo feature, logs through
//! `tracing` events, and marks each frame with that event when its Tracy
//! backend is compiled in. The layer knows nothing about Bevy, though: any
//! subscriber built on the `Registry` can add it.
//!
//! ```no_run
//! use tracing_subscriber::prelude::*;
//! tracing_subscriber::registry()
//!     .with(perfr_tracing::PerfrLayer::new())
//!     .init();
//! ```
//!
//! In a Bevy app it goes through `LogPlugin::custom_layer`; see
//! `integrations/bevy/README.md` in the perfr repository.
//!
//! What it costs: a span's name is formatted once, when the span is created,
//! and kept on the span. Every enter is one [`perfr_api::start`] and every
//! exit one [`perfr_api::stop`] -- a few stores while a service is capturing,
//! one branch when none is. Events are formatted only while a service is
//! capturing. A span entered before a capture started has no handle, so its
//! exit stops nothing; a span still open when the capture stops is ended by
//! a no-op.

use std::cell::RefCell;
use std::fmt::{self, Write as _};
use std::sync::atomic::{AtomicBool, Ordering};

use tracing_core::field::{Field, Visit};
use tracing_core::span::{Attributes, Id};
use tracing_core::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::registry::LookupSpan;

/// The layer. Add it to a `Registry`-based subscriber.
#[derive(Debug, Default)]
pub struct PerfrLayer {
    frame_scope: Option<Box<str>>,
    /// Whether the app has marked a frame itself, with a `tracy.frame_mark`
    /// event. From then on the scope named by
    /// [`PerfrLayer::frame_mark_on_exit`] marks none: two marks a frame
    /// would cut every frame in two.
    app_marks_frames: AtomicBool,
}

impl PerfrLayer {
    /// Creates the layer and this process's segment ([`perfr_api::init`]).
    ///
    /// If the segment cannot be made the layer still installs and every
    /// call stays a no-op; [`PerfrLayer::try_new`] reports that instead.
    pub fn new() -> Self {
        let _ = perfr_api::init();
        Self::default()
    }

    /// Like [`PerfrLayer::new`], but `Err(errno)` when the segment could not
    /// be created.
    pub fn try_new() -> Result<Self, i32> {
        perfr_api::init()?;
        Ok(Self::default())
    }

    /// Also marks a frame ([`perfr_api::frame_mark`]) each time a scope with
    /// exactly this name exits: the scope that brackets one turn of the
    /// app's loop.
    ///
    /// For an app that has no frame-mark event. Bevy emits `tracy.frame_mark`
    /// only when its Tracy backend is compiled in; the span it puts around
    /// each `App::update`, once a frame on the main thread, is named
    /// `update`, so that is the name to give here. If the event does turn
    /// up, it is the app's own word on where its frames end and this scope
    /// stops marking them.
    pub fn frame_mark_on_exit(mut self, scope_name: impl Into<Box<str>>) -> Self {
        self.frame_scope = Some(scope_name.into());
        self
    }
}

/// What a span is recorded as, kept in the span's extensions: its scope name
/// and whether its exit marks a frame.
struct ScopeName {
    name: Box<str>,
    frame: bool,
}

thread_local! {
    /// The spans this thread has entered and not yet exited: the span id,
    /// the handle `start` returned and whether the exit marks a frame. Enter
    /// and exit pair on one thread (`tracing`'s guards are not `Send`) and
    /// nest, so an exit is almost always the top entry; `rposition` covers a
    /// subscriber that exits out of order.
    static ENTERED: RefCell<Vec<(u64, perfr_api::Handle, bool)>> = const { RefCell::new(Vec::new()) };
}

impl<S> Layer<S> for PerfrLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };
        let name = scope_name(attrs);
        let frame = self.frame_scope.as_deref() == Some(&*name);
        span.extensions_mut().insert(ScopeName { name, frame });
    }

    fn on_enter(&self, id: &Id, ctx: Context<'_, S>) {
        if !perfr_api::capturing() {
            return;
        }
        let Some(span) = ctx.span(id) else { return };
        let extensions = span.extensions();
        let Some(scope) = extensions.get::<ScopeName>() else {
            return;
        };
        let handle = perfr_api::start(scope.name.as_bytes());
        if handle == 0 {
            return;
        }
        // `try_with`: a span entered while this thread's locals are being
        // torn down must not panic inside the subscriber.
        let _ = ENTERED.try_with(|entered| {
            entered
                .borrow_mut()
                .push((id.into_u64(), handle, scope.frame))
        });
    }

    fn on_exit(&self, id: &Id, _ctx: Context<'_, S>) {
        let raw = id.into_u64();
        let entered = ENTERED
            .try_with(|entered| {
                let mut entered = entered.borrow_mut();
                let at = entered.iter().rposition(|(span, _, _)| *span == raw)?;
                Some(entered.remove(at))
            })
            .ok()
            .flatten();
        if let Some((_, handle, frame)) = entered {
            perfr_api::stop(handle);
            if frame && !self.app_marks_frames.load(Ordering::Relaxed) {
                perfr_api::frame_mark();
            }
        }
    }

    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let metadata = event.metadata();
        if metadata.fields().field("tracy.frame_mark").is_some() {
            // The app's own frame boundary, and not a line of its log.
            self.app_marks_frames.store(true, Ordering::Relaxed);
            perfr_api::frame_mark();
            return;
        }
        if !perfr_api::capturing() {
            return;
        }
        let mut message = MessageVisitor::default();
        event.record(&mut message);
        perfr_api::log(
            level_of(metadata.level()),
            message.finish(metadata.target()),
        );
    }
}

/// The scope name for a span: the span's name, then its fields.
///
/// A field called `name` is the subject -- Bevy's `system`, `system_commands`
/// and `check_conditions` spans carry the system's path there -- so it
/// follows the span name after a colon, with no key: `system_commands:
/// bevy_render::view::prepare_windows`. For a span named `system` it stands
/// alone, since that is the name everyone looks for. Every other field
/// follows as ` key=value`.
fn scope_name(attrs: &Attributes<'_>) -> Box<str> {
    let span = attrs.metadata().name();
    let mut fields = NameVisitor::default();
    attrs.record(&mut fields);
    let NameVisitor { subject, rest } = fields;
    let mut name =
        String::with_capacity(span.len() + 2 + subject.as_deref().map_or(0, str::len) + rest.len());
    match subject {
        Some(subject) if span == "system" => name.push_str(&subject),
        Some(subject) => {
            name.push_str(span);
            name.push_str(": ");
            name.push_str(&subject);
        }
        None => name.push_str(span),
    }
    name.push_str(&rest);
    name.into_boxed_str()
}

#[derive(Default)]
struct NameVisitor {
    subject: Option<String>,
    rest: String,
}

impl Visit for NameVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "name" && self.subject.is_none() {
            self.subject = Some(value.to_owned());
        } else {
            let _ = write!(self.rest, " {}={}", field.name(), value);
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() == "name" && self.subject.is_none() {
            self.subject = Some(unquoted(format!("{value:?}")));
        } else {
            let _ = write!(self.rest, " {}={:?}", field.name(), value);
        }
    }
}

/// `?name` records a `Debug` rendering, which quotes strings; the scope name
/// should not.
fn unquoted(mut text: String) -> String {
    if text.len() >= 2 && text.starts_with('"') && text.ends_with('"') {
        text.pop();
        text.remove(0);
    }
    text
}

/// A log line: `target: message key=value ...`.
///
/// A record that came through the `log` crate's bridge has the target `log`
/// and its real origin in `log.target`, with `log.module_path`, `log.file`
/// and `log.line` beside it; the line takes that target and drops the rest.
#[derive(Default)]
struct MessageVisitor {
    message: Option<String>,
    target: Option<String>,
    rest: String,
}

impl MessageVisitor {
    fn finish(self, target: &str) -> String {
        let target = self.target.as_deref().unwrap_or(target);
        let mut line = String::with_capacity(
            target.len() + 2 + self.message.as_deref().map_or(0, str::len) + self.rest.len(),
        );
        line.push_str(target);
        line.push_str(": ");
        if let Some(message) = &self.message {
            line.push_str(message);
        }
        line.push_str(&self.rest);
        line
    }
}

impl Visit for MessageVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "message" => self.message = Some(value.to_owned()),
            "log.target" => self.target = Some(value.to_owned()),
            name if name.starts_with("log.") => {}
            name => {
                let _ = write!(self.rest, " {name}={value}");
            }
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        match field.name() {
            // `Debug` for `fmt::Arguments` is the formatted text, unquoted.
            "message" => self.message = Some(format!("{value:?}")),
            "log.target" => self.target = Some(unquoted(format!("{value:?}"))),
            name if name.starts_with("log.") => {}
            name => {
                let _ = write!(self.rest, " {name}={value:?}");
            }
        }
    }
}

fn level_of(level: &Level) -> u8 {
    use perfr_api::level;
    if *level == Level::TRACE {
        level::TRACE
    } else if *level == Level::DEBUG {
        level::DEBUG
    } else if *level == Level::INFO {
        level::INFO
    } else if *level == Level::WARN {
        level::WARN
    } else {
        level::ERROR
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use perfr_scope_ring::event::kind;
    use perfr_scope_ring::merge::{drain, Cursors};
    use perfr_scope_ring::shm::now_monotonic_ns;
    use perfr_scope_ring::text::TextAssembler;
    use perfr_scope_ring::{ScopeEvent, ScopeRingReader};
    use tracing_subscriber::prelude::*;

    fn all_events() -> Vec<ScopeEvent> {
        let reader = ScopeRingReader::open(std::process::id()).expect("segment exists");
        let rings = reader.rings();
        let mut cursors = Cursors::for_rings(rings.ring_count());
        let pass = drain(rings, &mut cursors, now_monotonic_ns());
        pass.slices.into_iter().flat_map(|s| s.events).collect()
    }

    /// Named records -- scope starts and instants -- as (name, kind, id),
    /// with spilled names reassembled.
    fn named(events: &[ScopeEvent]) -> Vec<(String, u8, u64)> {
        let mut assembler = TextAssembler::new();
        let mut out = Vec::new();
        for event in events {
            if !matches!(event.kind, kind::SCOPE_START | kind::INSTANT | kind::TEXT) {
                continue;
            }
            if let Some((name, _)) = assembler.accept(event) {
                out.push((name, event.kind, event.scope_id));
            }
        }
        out
    }

    #[test]
    fn spans_events_and_frame_marks_reach_the_ring() {
        perfr_api::init().expect("init");
        // No service here, so stand in for one: set the capturing flag and
        // hold the reader, since dropping it would clear the flag.
        let service = ScopeRingReader::open(std::process::id()).expect("segment");
        service.set_capturing(true);

        #[derive(Debug)]
        struct Main;
        let layer = PerfrLayer::new().frame_mark_on_exit("schedule: Main");
        let subscriber = tracing_subscriber::registry().with(layer);
        tracing::subscriber::with_default(subscriber, || {
            // Bevy's per-frame schedule span, named the way the layer names
            // it. No frame-mark event has been seen, so its exit is the
            // frame boundary.
            tracing::info_span!("schedule", name = ?Main).in_scope(|| {});
            let system = tracing::info_span!("system", name = "my_game::update");
            system.in_scope(|| {
                tracing::info_span!("queue_submit", count = 3u32).in_scope(|| {});
                tracing::info_span!("sub app", name = ?"Render").in_scope(|| {});
                tracing::info!(answer = 42, "hello");
                tracing::event!(Level::INFO, tracy.frame_mark = true);
                // What the `log` crate's bridge emits for `log::warn!`.
                tracing::event!(
                    target: "log",
                    Level::WARN,
                    message = "button?",
                    log.target = "sctk_adwaita::buttons",
                    log.module_path = "sctk_adwaita::buttons",
                    log.file = "buttons.rs",
                    log.line = 172u32
                );
            });
            // The app has marked a frame itself by now: the schedule's exit
            // no longer does, or every frame would be cut in two.
            tracing::info_span!("schedule", name = ?Main).in_scope(|| {});
        });
        service.set_capturing(false);

        let events = all_events();
        let names = named(&events);
        let start_of = |name: &str| {
            names
                .iter()
                .find(|(n, k, _)| n == name && *k == kind::SCOPE_START)
                .unwrap_or_else(|| panic!("no scope named {name:?} in {names:?}"))
                .2
        };
        // A `system` span is named by its `name` field alone; other spans
        // keep their own name, a `name` field after a colon, other fields as
        // key=value; `?name` loses its Debug quotes.
        let system = start_of("my_game::update");
        let submit = start_of("queue_submit count=3");
        let sub_app = start_of("sub app: Render");
        for id in [system, submit, sub_app] {
            assert!(
                events
                    .iter()
                    .any(|e| e.kind == kind::SCOPE_STOP && e.scope_id == id),
                "scope {id:#x} was stopped"
            );
        }
        // Children start after the parent and stop before it.
        let at = |k: u8, id: u64| {
            events
                .iter()
                .find(|e| e.kind == k && e.scope_id == id)
                .unwrap()
                .timestamp_ns
        };
        assert!(at(kind::SCOPE_START, system) <= at(kind::SCOPE_START, submit));
        assert!(at(kind::SCOPE_STOP, sub_app) <= at(kind::SCOPE_STOP, system));

        // Two frame marks of the main series (no name), on this thread: one
        // from the first exit of the `schedule: Main` scope, after that
        // scope stopped, and one from the `tracy.frame_mark` event. The
        // schedule's second exit, after the event, marked nothing.
        let tid = perfr_scope_ring::platform::thread_id() as u32;
        let marks: Vec<u64> = events
            .iter()
            .filter(|e| e.kind == kind::FRAME && e.tid == tid)
            .inspect(|e| assert_eq!(e.text_len, 0, "the main series has no name"))
            .map(|e| e.timestamp_ns)
            .collect();
        assert_eq!(marks.len(), 2, "frame marks: {marks:?}");
        let main = start_of("schedule: Main");
        assert!(marks[0] >= at(kind::SCOPE_STOP, main), "the first mark follows the schedule's stop");
        assert!(marks[1] >= at(kind::SCOPE_START, system), "the second is the event, inside the system");
        assert!(
            names.iter().all(|(n, k, _)| !(n == "frame" && *k == kind::INSTANT)),
            "a frame boundary is a frame mark, not an instant called frame"
        );

        // A log line is written the way a scope's name is: a `LOG` record
        // with the level in its flags, and what does not fit in it as text
        // continuations with the same key. The assembler puts it back.
        let mut assembler = TextAssembler::new();
        let mut levels = std::collections::HashMap::new();
        let mut lines: Vec<(u8, String)> = Vec::new();
        for event in events.iter().filter(|e| matches!(e.kind, kind::LOG | kind::TEXT)) {
            if event.kind == kind::LOG {
                levels.insert((event.tid, event.scope_id), perfr_api::level::from_flags(event.flags));
            }
            if let Some((message, _)) = assembler.accept(event) {
                // A continuation of something that is not a log line has no
                // level here, and is not one.
                if let Some(level) = levels.get(&(event.tid, event.scope_id)) {
                    lines.push((*level, message));
                }
            }
        }
        assert!(
            lines.contains(&(perfr_api::level::INFO, "perfr_tracing::tests: hello answer=42".to_string())),
            "the event became a log line: {lines:?}"
        );
        assert!(
            lines.contains(&(perfr_api::level::WARN, "sctk_adwaita::buttons: button?".to_string())),
            "a bridged `log` record takes its real target and sheds the log.* fields: {lines:?}"
        );
        assert!(
            ENTERED.with(|entered| entered.borrow().is_empty()),
            "every entered span was exited and forgotten"
        );
    }

    #[test]
    fn levels_map_one_to_one_and_fatal_is_never_produced() {
        use perfr_api::level;
        assert_eq!(level_of(&Level::TRACE), level::TRACE);
        assert_eq!(level_of(&Level::DEBUG), level::DEBUG);
        assert_eq!(level_of(&Level::INFO), level::INFO);
        assert_eq!(level_of(&Level::WARN), level::WARN);
        assert_eq!(level_of(&Level::ERROR), level::ERROR);
    }

    #[test]
    fn unquoting_touches_only_a_quoted_string() {
        assert_eq!(unquoted("\"Render\"".into()), "Render");
        assert_eq!(unquoted("Render".into()), "Render");
        assert_eq!(unquoted("\"".into()), "\"");
        assert_eq!(unquoted(String::new()), "");
    }
}
