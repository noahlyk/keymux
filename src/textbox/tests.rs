//! The listener against a stand-in compositor: it offers a seat and an input method manager,
//! and plays what a compositor sends when a text box is focused and edited.

use super::*;
use std::os::unix::net::UnixStream;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;
use wayland_protocols::wp::text_input::zv3::server::zwp_text_input_v3 as text_input;
use wayland_protocols_misc::zwp_input_method_v2::server::{
    zwp_input_method_manager_v2::{self as manager, ZwpInputMethodManagerV2 as ServerManager},
    zwp_input_method_v2::ZwpInputMethodV2 as ServerMethod,
};
use wayland_server::protocol::wl_seat::WlSeat as ServerSeat;
use wayland_server::{Client, DataInit, Dispatch, Display, DisplayHandle, GlobalDispatch, New, Resource};

#[derive(Default)]
struct Compositor {
    method: Option<ServerMethod>,
}

struct Clients;
impl wayland_server::backend::ClientData for Clients {}

impl GlobalDispatch<ServerSeat, ()> for Compositor {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        seat: New<ServerSeat>,
        (): &(),
        init: &mut DataInit<'_, Self>,
    ) {
        init.init(seat, ());
    }
}

impl Dispatch<ServerSeat, ()> for Compositor {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &ServerSeat,
        _: <ServerSeat as Resource>::Request,
        (): &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
}

impl GlobalDispatch<ServerManager, ()> for Compositor {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        manager: New<ServerManager>,
        (): &(),
        init: &mut DataInit<'_, Self>,
    ) {
        init.init(manager, ());
    }
}

impl Dispatch<ServerManager, ()> for Compositor {
    fn request(
        compositor: &mut Self,
        _: &Client,
        _: &ServerManager,
        request: manager::Request,
        (): &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        if let manager::Request::GetInputMethod { input_method, .. } = request {
            compositor.method = Some(init.init(input_method, ()));
        }
    }
}

impl Dispatch<ServerMethod, ()> for Compositor {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &ServerMethod,
        _: <ServerMethod as Resource>::Request,
        (): &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
}

/// What the stand-in compositor sends the input method.
enum Send {
    Activate,
    Deactivate,
    Text(&'static str, u32, u32),
    Content(text_input::ContentHint, text_input::ContentPurpose),
    Done,
}

/// Run the listener against a stand-in compositor that plays `script`, and return the reports
/// it made.
fn play(script: Vec<Send>) -> Vec<TextBoxReport> {
    let (client_end, server_end) = UnixStream::pair().unwrap();
    let (reports, received) = mpsc::channel();

    let mut display = Display::<Compositor>::new().unwrap();
    let mut handle = display.handle();
    handle.create_global::<Compositor, ServerSeat, ()>(7, ());
    handle.create_global::<Compositor, ServerManager, ()>(1, ());
    handle.insert_client(server_end, Arc::new(Clients)).unwrap();

    let listener = std::thread::spawn(move || {
        let conn = Connection::from_socket(client_end).unwrap();
        listen(&conn, move |report| {
            let _ = reports.send(report.clone());
        })
    });

    let mut compositor = Compositor::default();
    let mut script = script.into_iter();
    let started = std::time::Instant::now();
    // Serve the client until it has asked for its input method and the script has been sent
    while started.elapsed() < Duration::from_secs(5) {
        display.dispatch_clients(&mut compositor).unwrap();
        if let Some(method) = &compositor.method {
            for step in script.by_ref() {
                match step {
                    Send::Activate => method.activate(),
                    Send::Deactivate => method.deactivate(),
                    Send::Text(text, cursor, anchor) => {
                        method.surrounding_text(text.to_string(), cursor, anchor);
                    }
                    Send::Content(hint, purpose) => method.content_type(hint, purpose),
                    Send::Done => method.done(),
                }
            }
            display.flush_clients().unwrap();
            break;
        }
        display.flush_clients().unwrap();
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(compositor.method.is_some(), "the listener never asked for an input method");

    let mut made = Vec::new();
    while let Ok(report) = received.recv_timeout(Duration::from_millis(300)) {
        made.push(report);
    }
    // Hanging up must end the listener, not leave it waiting
    drop(compositor);
    drop(handle);
    drop(display);
    let ended = listener.join().unwrap();
    assert!(ended.is_err(), "the listener should stop when the connection ends");
    made
}

#[test]
fn a_focused_text_box_is_reported_with_the_text_before_the_cursor() {
    let reports = play(vec![
        Send::Activate,
        Send::Text("hello world", 5, 5),
        Send::Done,
        // The user types "!" after "hello"
        Send::Text("hello! world", 6, 6),
        Send::Done,
    ]);
    assert_eq!(reports.len(), 2, "{reports:?}");
    assert_eq!(reports[0].state.before, "hello");
    assert!(reports[0].state.active);
    assert_eq!(reports[1].state.before, "hello!");
    // The same text box throughout
    assert_eq!(reports[0].focus, reports[1].focus);
}

#[test]
fn a_second_text_box_is_a_new_focus() {
    let reports = play(vec![
        Send::Activate,
        Send::Text("one", 3, 3),
        Send::Done,
        Send::Activate,
        Send::Text("two", 3, 3),
        Send::Done,
    ]);
    assert_eq!(reports.len(), 2, "{reports:?}");
    assert_ne!(reports[0].focus, reports[1].focus);
}

#[test]
fn leaving_the_text_box_reports_that_it_is_gone() {
    let reports = play(vec![
        Send::Activate,
        Send::Text("one", 3, 3),
        Send::Done,
        Send::Deactivate,
        Send::Done,
    ]);
    assert_eq!(reports.len(), 2, "{reports:?}");
    assert!(reports[0].state.active);
    assert!(!reports[1].state.active);
    assert!(reports[1].state.before.is_empty());
}

#[test]
fn a_password_field_is_never_read() {
    let reports = play(vec![
        Send::Activate,
        Send::Content(
            text_input::ContentHint::None,
            text_input::ContentPurpose::Password,
        ),
        Send::Text("hunter2", 7, 7),
        Send::Done,
    ]);
    assert_eq!(reports.len(), 1, "{reports:?}");
    assert!(!reports[0].state.active);
    assert!(reports[0].state.before.is_empty());
}

#[test]
fn a_field_marked_sensitive_is_never_read() {
    let reports = play(vec![
        Send::Activate,
        Send::Content(
            text_input::ContentHint::SensitiveData,
            text_input::ContentPurpose::Normal,
        ),
        Send::Text("4111 1111", 9, 9),
        Send::Done,
    ]);
    assert_eq!(reports.len(), 1, "{reports:?}");
    assert!(!reports[0].state.active);
}

#[test]
fn selected_text_is_reported_as_a_selection() {
    let reports = play(vec![
        Send::Activate,
        Send::Text("hello world", 11, 6),
        Send::Done,
    ]);
    assert!(reports[0].state.selection);
    assert_eq!(reports[0].state.before, "hello world");
}
