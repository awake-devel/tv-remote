//! TV Remote: GTK4 / libadwaita remote control for the living-room Samsung TV.
//!
//! All TV work goes through `tvremote_core` on worker threads (see [`ctx`]); the GTK
//! main loop never blocks.

mod ctx;
mod format;
mod pages;
mod pointer_lock;
mod window;

use adw::prelude::*;

const APP_ID: &str = "fr.ryoku.TvRemote";

fn main() -> gtk::glib::ExitCode {
    // `--page <remote|touchpad|apps|cast|picture|device>` opens on that page.
    let args: Vec<String> = std::env::args().collect();
    let start_page = args
        .iter()
        .position(|a| a == "--page")
        .and_then(|i| args.get(i + 1).cloned());
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_activate(move |app| {
        // Single instance: re-activation focuses the existing window.
        if let Some(w) = app.active_window() {
            w.present();
            return;
        }
        window::build(app, start_page.as_deref());
    });
    // Our own flags are handled above; GApplication only sees the program name.
    app.run_with_args(&args[..1.min(args.len())])
}
