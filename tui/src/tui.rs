//! The interactive terminal loop: terminal setup/teardown, input polling, redraws.

use std::io::IsTerminal;
use std::time::Duration;

use ratatui::crossterm::event::{self, Event as TermEvent, KeyEventKind};

use tvremote_core::{Fleet, Tv};

use crate::app::App;

/// Restores the terminal when dropped (normal exit and early returns).
struct RestoreGuard;

impl Drop for RestoreGuard {
    fn drop(&mut self) {
        ratatui::restore();
    }
}

/// Restore the terminal before the default panic message is printed, so a panic
/// never leaves the shell in raw mode / the alternate screen.
fn install_panic_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        ratatui::restore();
        prev(info);
    }));
}

pub fn run(fleet: Fleet, tv: Tv) -> Result<(), String> {
    if !std::io::stdout().is_terminal() {
        return Err("the interactive UI needs a terminal (see --help for CLI commands)".into());
    }
    install_panic_hook();
    let emoji = crate::util::terminal_supports_emoji();
    let mut app = App::new(fleet, tv, emoji);
    let mut terminal =
        ratatui::try_init().map_err(|e| format!("cannot set up the terminal: {e}"))?;
    let _guard = RestoreGuard;
    app.start();

    while !app.quit {
        app.pump();
        terminal
            .draw(|f| crate::ui::draw(f, &mut app))
            .map_err(|e| e.to_string())?;
        // Poll with a timeout so background results and the now-playing line refresh
        // even without input.
        if event::poll(Duration::from_millis(150)).map_err(|e| e.to_string())? {
            match event::read().map_err(|e| e.to_string())? {
                TermEvent::Key(k) if k.kind == KeyEventKind::Press => app.on_key(k),
                _ => {}
            }
            // Drain queued input quickly (key repeat) before the next redraw.
            while !app.quit && event::poll(Duration::ZERO).unwrap_or(false) {
                if let Ok(TermEvent::Key(k)) = event::read() {
                    if k.kind == KeyEventKind::Press {
                        app.on_key(k);
                    }
                }
            }
        }
    }
    app.fleet.stop_status_polling();
    Ok(())
}
