#![allow(clippy::too_many_arguments)]
#![allow(irrefutable_let_patterns)]

#[macro_use]
extern crate slog_scope;

mod event_handler;
mod input_handler;

mod framework;

mod grabs;

mod render;
mod utils;

mod config;

mod output_manager;

mod popup;
mod window;

mod state;

mod backend_handler;
mod shell_handler;

mod cli;

mod workspace;

use config::outputs::shell::logger::ShellDrain;
use state::Anodium;

use slog::Drain;

use smithay::reexports::calloop::EventLoop;

use std::sync::atomic::Ordering;

fn main() {
    let options = cli::get_anodium_options();
    let prefered_backend = options.backend.clone();

    std::env::set_var("RUST_LOG", "trace,smithay=error");
    let terminal_drain = slog_async::Async::default(slog_envlogger::new(
        slog_term::CompactFormat::new(slog_term::TermDecorator::new().stderr().build())
            .build()
            .fuse(),
    ))
    .fuse();

    let shell_drain = slog_async::Async::default(ShellDrain::new().fuse());

    let log = slog::Logger::root(
        slog::Duplicate::new(terminal_drain, shell_drain).fuse(),
        slog::o!(),
    );
    let _guard = slog_scope::set_global_logger(log.clone());
    slog_stdlog::init().expect("Could not setup log backend");

    //
    // Run The Compositor
    //

    let mut event_loop = EventLoop::try_new().unwrap();

    let (mut anodium, rx) = Anodium::new(event_loop.handle(), "seat0".into(), options);

    framework::backend::auto(&mut event_loop, &mut anodium, rx, prefered_backend);

    run_loop(anodium, event_loop);
}

fn run_loop(mut state: Anodium, mut event_loop: EventLoop<'static, Anodium>) {
    let signal = event_loop.get_signal();
    event_loop
        .run(None, &mut state, |state| {
            if state.workspace.outputs().count() == 0 || !state.running.load(Ordering::SeqCst) {
                signal.stop();
            }

            state.display.borrow_mut().flush_clients(&mut ());
            state.update();
        })
        .unwrap();
}
