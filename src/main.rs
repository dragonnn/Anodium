#[macro_use]
extern crate slog_scope;

use slog::Drain;

#[cfg(feature = "udev")]
mod cursor;
mod input_handler;
mod state;

mod framework;

mod grabs;

mod render;
mod utils;

mod animations;
mod positioner;

mod config;

mod iterators;
mod output_map;

mod popup;
mod window;

use state::Anodium;

fn main() {
    // A logger facility, here we use the terminal here
    let log = slog::Logger::root(
        slog_async::Async::default(slog_term::term_full().fuse()).fuse(),
        //std::sync::Mutex::new(slog_term::term_full().fuse()).fuse(),
        slog::o!(),
    );
    let _guard = slog_scope::set_global_logger(log.clone());
    slog_stdlog::init().expect("Could not setup log backend");

    framework::backend::auto();
}
