use std::{fmt::Display, process::exit};

pub mod buffer;
pub mod channel_manager;
pub mod io;
pub mod polled_fd;
pub mod pollster;
pub mod pty;
pub mod signals;

/// Prints `msg` to stderr and exits the process.
pub fn die<D: Display>(msg: D) -> ! {
    eprintln!("{}", msg);
    exit(255)
}
