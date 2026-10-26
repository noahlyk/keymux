//! `keymux textbox`: tells the daemon what the focused text box says is before the cursor.
//!
//! Runs as the session user, because the daemon runs as root and can't reach the Wayland
//! socket. It registers as the seat's input method (`zwp_input_method_v2`), which is how the
//! compositor passes on a text box's `surrounding text` when the application supports
//! `zwp_text_input_v3`: GTK, Qt, Firefox, and most toolkits' own text fields.
//!
//! It only listens. It never grabs the keyboard, never commits text and never shows a popup,
//! so typing goes to the application exactly as it does without it. Text boxes marked as a
//! password or sensitive are not read at all. Only one input method can run on a seat, so this
//! cannot run next to fcitx or ibus.

#[cfg(test)]
mod tests;

use crate::ipc::TextBoxReporter;
use crate::steno::textbox::{ReportBuilder, TextBoxReport};
use anyhow::{bail, Context, Result};
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum};
use wayland_protocols::wp::text_input::zv3::client::zwp_text_input_v3::{
    ContentHint, ContentPurpose,
};
use wayland_protocols_misc::zwp_input_method_v2::client::{
    zwp_input_method_manager_v2::ZwpInputMethodManagerV2,
    zwp_input_method_v2::{self, ZwpInputMethodV2},
};

struct State {
    builder: ReportBuilder,
    /// Where each report goes
    sink: Box<dyn FnMut(&TextBoxReport)>,
    /// Another input method has the seat
    unavailable: bool,
}

fn print_report(report: &TextBoxReport) {
    let state = &report.state;
    println!(
        "box {:>4}  {:<8} {}{:?}",
        report.focus,
        if state.active { "reports" } else { "silent" },
        if state.selection { "[selection] " } else { "" },
        state.before,
    );
}

pub fn run(dump: bool) -> Result<()> {
    tracing_subscriber::fmt().with_target(false).init();
    let conn = Connection::connect_to_env().context("no Wayland session to listen on")?;
    let mut reporter = TextBoxReporter::default();
    tracing::info!("listening for the focused text box");
    listen(&conn, move |report| {
        if dump {
            print_report(report);
        } else {
            // A daemon that isn't running is not an error: the next report connects again
            reporter.send(report);
        }
    })
}

/// Register as the input method and pass each report to `sink`, until the connection ends.
fn listen(conn: &Connection, sink: impl FnMut(&TextBoxReport) + 'static) -> Result<()> {
    let (globals, mut queue) = registry_queue_init::<State>(conn)?;
    let qh = queue.handle();

    let seat: wl_seat::WlSeat = globals
        .bind(&qh, 1..=1, ())
        .context("this compositor has no seat")?;
    let manager: ZwpInputMethodManagerV2 = globals.bind(&qh, 1..=1, ()).context(
        "this compositor has no input method protocol (zwp_input_method_manager_v2), \
         which reading the text box needs",
    )?;
    // Kept alive for as long as the loop runs
    let _method = manager.get_input_method(&seat, &qh, ());

    let mut state = State {
        builder: ReportBuilder::default(),
        sink: Box::new(sink),
        unavailable: false,
    };
    loop {
        queue.blocking_dispatch(&mut state)?;
        if state.unavailable {
            bail!("another input method (fcitx, ibus...) already has this seat");
        }
    }
}

impl Dispatch<ZwpInputMethodV2, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwpInputMethodV2,
        event: zwp_input_method_v2::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwp_input_method_v2::Event;
        match event {
            Event::Activate => state.builder.activate(),
            Event::Deactivate => state.builder.deactivate(),
            Event::SurroundingText {
                text,
                cursor,
                anchor,
            } => state.builder.surrounding_text(text, cursor, anchor),
            Event::ContentType { hint, purpose } => {
                let sensitive_hint = matches!(
                    hint,
                    WEnum::Value(hint) if hint.contains(ContentHint::SensitiveData)
                );
                let sensitive_purpose = matches!(
                    purpose,
                    WEnum::Value(ContentPurpose::Password | ContentPurpose::Pin)
                );
                state.builder.content_type(sensitive_hint || sensitive_purpose);
            }
            Event::Done => (state.sink)(&state.builder.done()),
            Event::Unavailable => state.unavailable = true,
            // Not needed to read the text
            _ => {}
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        _: &mut Self,
        _: &wl_seat::WlSeat,
        _: wl_seat::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpInputMethodManagerV2, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwpInputMethodManagerV2,
        _: <ZwpInputMethodManagerV2 as wayland_client::Proxy>::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
