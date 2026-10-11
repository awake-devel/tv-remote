//! TV Remote: GTK4 / libadwaita remote control for Samsung TVs.
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
    // GApplication never sees our arguments, so answer these here instead of opening the window.
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!(
            "TV Remote (GTK)\n\nUsage: tv-remote [--page <remote|touchpad|apps|cast|picture|device>]\n\n  \
             --page <PAGE>   open on that page (first window only; a running app is just raised)\n  \
             -h, --help      this help\n  -V, --version   version\n\nThe command-line tool is tv-remote-tui."
        );
        return gtk::glib::ExitCode::SUCCESS;
    }
    if args.iter().any(|a| a == "-V" || a == "--version") {
        println!("tv-remote {}", env!("CARGO_PKG_VERSION"));
        return gtk::glib::ExitCode::SUCCESS;
    }
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
