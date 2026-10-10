//! `tv-remote-tui`: interactive terminal UI (no subcommand) and CLI for the
//! Samsung TVs, built on `tvremote_core`.

mod app;
mod browser;
mod castflow;
mod cli;
mod tui;
mod ui;
mod util;
mod worker;

use clap::Parser;

fn main() {
    let mut args = cli::Cli::parse();
    let code = match args.command.take() {
        Some(cmd) => cli::run(args, cmd),
        None => match cli::open_tv(args.config.as_deref(), args.tv.as_deref()) {
            Ok((fleet, _)) if fleet.entries().is_empty() => {
                eprintln!(
                    "error: no TV configured: run `tv-remote-tui tvs scan` then `tvs add <n>` \
                     (config: {})",
                    tvremote_core::default_config_path().display()
                );
                1
            }
            Ok((fleet, tv)) => match tui::run(fleet, tv) {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("error: {e}");
                    1
                }
            },
            Err(f) => {
                eprintln!("error: {}", f.msg);
                f.code
            }
        },
    };
    std::process::exit(code);
}
