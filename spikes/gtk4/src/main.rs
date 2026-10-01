mod process;

use gtk::prelude::*;
use gtk::{
    AccessibleRole, Application, ApplicationWindow, Box as GtkBox, Button, Label, Orientation,
};
use process::{start_status_probe, Probe, ProbeKind};
use std::cell::{Cell, RefCell};
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

const APP_ID: &str = "org.xwindowlog.Gtk4Spike";

fn main() {
    let cli_path = command_line_cli_path(std::env::args_os());
    let app = Application::builder().application_id(APP_ID).build();
    app.connect_activate(move |app| build_ui(app, cli_path.clone()));
    app.run_with_args(&["xwindowlog-gtk4-spike"]);
}

fn resolve_cli_path(candidate: &OsStr) -> Option<PathBuf> {
    let path = PathBuf::from(candidate);
    path.is_absolute().then_some(path)
}

fn command_line_cli_path(args: impl IntoIterator<Item = OsString>) -> Option<PathBuf> {
    let mut args = args.into_iter();
    args.next()?;
    let flag = args.next()?;
    let path = args.next()?;
    if flag != OsStr::new("--cli") || args.next().is_some() {
        return None;
    }
    resolve_cli_path(&path)
}

fn build_ui(app: &Application, cli_path: Option<PathBuf>) {
    let status = Label::new(Some(if cli_path.is_some() {
        "Status: Ready to check persisted status."
    } else {
        "Status: Provide an absolute CLI path with --cli PATH."
    }));
    status.set_xalign(0.0);
    status.set_accessible_role(AccessibleRole::Status);

    let check = Button::with_label("Check status");
    let cancel = Button::with_label("Cancel status check");
    cancel.set_sensitive(false);

    let actions = GtkBox::new(Orientation::Horizontal, 8);
    actions.append(&check);
    actions.append(&cancel);
    let content = GtkBox::new(Orientation::Vertical, 12);
    content.set_margin_top(16);
    content.set_margin_bottom(16);
    content.set_margin_start(16);
    content.set_margin_end(16);
    content.append(&status);
    content.append(&actions);

    let window = ApplicationWindow::builder()
        .application(app)
        .title("xwindowlog GTK4 feasibility")
        .default_width(420)
        .default_height(150)
        .child(&content)
        .build();

    let running = Rc::new(RefCell::new(None::<Probe>));
    let closing = Rc::new(Cell::new(false));

    let start_running = Rc::clone(&running);
    let start_status = status.downgrade();
    let start_cancel = cancel.downgrade();
    check.connect_clicked(move |button| {
        if start_running.borrow().is_some() {
            return;
        }
        let Some(path) = cli_path.as_deref() else {
            if let Some(status) = start_status.upgrade() {
                status.set_text("Status: Provide an absolute CLI path with --cli PATH.");
            }
            return;
        };
        match start_status_probe(path) {
            Ok(probe) => {
                *start_running.borrow_mut() = Some(probe);
                button.set_sensitive(false);
                if let Some(cancel) = start_cancel.upgrade() {
                    cancel.set_sensitive(true);
                }
                if let Some(status) = start_status.upgrade() {
                    status.set_text("Status: Checking persisted status…");
                }
            }
            Err(_) => {
                if let Some(status) = start_status.upgrade() {
                    status.set_text("Status: Check unavailable.");
                }
            }
        }
    });

    let cancel_running = Rc::clone(&running);
    let cancel_status = status.downgrade();
    cancel.connect_clicked(move |button| {
        if let Some(probe) = cancel_running.borrow().as_ref() {
            probe.cancel();
            button.set_sensitive(false);
            if let Some(status) = cancel_status.upgrade() {
                status.set_text("Status: Cancelling check…");
            }
        }
    });

    let close_running = Rc::clone(&running);
    let close_status = status.downgrade();
    let close_pending = Rc::clone(&closing);
    window.connect_close_request(move |_| {
        if let Some(probe) = close_running.borrow().as_ref() {
            close_pending.set(true);
            probe.cancel();
            if let Some(status) = close_status.upgrade() {
                status.set_text("Status: Cancelling check before closing…");
            }
            gtk::glib::Propagation::Stop
        } else {
            gtk::glib::Propagation::Proceed
        }
    });

    let poll_running = Rc::clone(&running);
    let poll_closing = Rc::clone(&closing);
    let poll_window = window.downgrade();
    let poll_status = status.downgrade();
    let poll_check = check.downgrade();
    let poll_cancel = cancel.downgrade();
    gtk::glib::timeout_add_local(Duration::from_millis(25), move || {
        let result = poll_running.borrow().as_ref().and_then(Probe::try_result);
        if let Some(result) = result {
            if result.window_close_is_safe() {
                poll_running.borrow_mut().take();
            }
            if let Some(button) = poll_check.upgrade() {
                button.set_sensitive(result.window_close_is_safe());
            }
            if let Some(button) = poll_cancel.upgrade() {
                button.set_sensitive(false);
            }
            if let Some(status) = poll_status.upgrade() {
                let text = if !result.window_close_is_safe() {
                    "Status: Cleanup incomplete; window remains open."
                } else {
                    match result.kind {
                        ProbeKind::Completed => "Status: Check completed.",
                        ProbeKind::Cancelled => "Status: Check cancelled.",
                        ProbeKind::TimedOut => "Status: Check timed out.",
                        ProbeKind::OutputLimit => "Status: Check stopped at the output limit.",
                        _ => "Status: Check unavailable.",
                    }
                };
                status.set_text(text);
            }
        }
        if poll_closing.get() && poll_running.borrow().is_none() {
            if let Some(window) = poll_window.upgrade() {
                window.close();
            }
            return gtk::glib::ControlFlow::Break;
        }
        if poll_window.upgrade().is_some() {
            gtk::glib::ControlFlow::Continue
        } else {
            gtk::glib::ControlFlow::Break
        }
    });

    window.present();
}

#[cfg(test)]
mod tests {
    use super::{command_line_cli_path, resolve_cli_path};
    use std::ffi::{OsStr, OsString};

    #[test]
    fn cli_path_requires_absolute_executable_and_no_extra_arguments() {
        let executable = std::env::current_exe().expect("test executable path");
        assert_eq!(
            resolve_cli_path(executable.as_os_str()),
            Some(executable.clone())
        );
        assert_eq!(resolve_cli_path(OsStr::new("xwindowlog")), None);
        let args = ["spike", "--cli"]
            .into_iter()
            .map(OsString::from)
            .chain([executable.as_os_str().to_owned()]);
        assert_eq!(command_line_cli_path(args), Some(executable));
        assert!(command_line_cli_path(["spike", "xwindowlog"].map(OsString::from)).is_none());
        assert!(
            command_line_cli_path(["spike", "--cli", "xwindowlog"].map(OsString::from)).is_none()
        );
    }
}
