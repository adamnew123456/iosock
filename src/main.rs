mod signals;
mod sockets;
mod tracing;

use std::env::{args, var};
use std::fmt::Display;
use std::fs::remove_file;
use std::io::{pipe, Write};
use std::os::unix::net::UnixListener;
use std::process::{exit, Command, Stdio};
use std::thread::spawn;

use signals::kill_child_on_signal;
use sockets::socket_stream_bridge;

/// Prints `msg` to stderr and exits the process.
fn die<D: Display>(msg: D) -> ! {
    eprintln!("{}", msg);
    exit(255)
}

/// Displays the command-line help and exits the process.
fn usage() -> ! {
    die("Usage: iosock SOCKET_PATH PROGRAM ARGS...")
}

fn main() {
    let trace_console = match var("IOSOCK_TRACE") {
        Ok(value) => value.len() > 0,
        Err(_) => false
    };

    let mut args = args();
    args.next();
    let sock_path = args.next().unwrap_or_else(|| usage());
    let command_line: Vec<String> = args.collect();
    if command_line.len() == 0 {
        usage();
    }

    let listener = UnixListener::bind(&sock_path)
        .unwrap_or_else(|err| die(format!("Could not create socket: {}", err)));

    let (closer_read, mut closer_write) =
        pipe().unwrap_or_else(|err| die(format!("Could not create notification pipe: {}", err)));

    let mut child = Command::new(&command_line[0])
        .args(&command_line[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|err| die(format!("Could not start process: {}", err)));

    kill_child_on_signal(child.id() as i32)
        .unwrap_or_else(|err| die(format!("Could not bind signal handler: {}", err)));

    let child_stdin = child
        .stdin
        .take()
        .unwrap_or_else(|| die("No stdin pipe for child"));
    let child_stdout = child
        .stdout
        .take()
        .unwrap_or_else(|| die("No stdout pipe for child"));
    let child_stderr = child
        .stderr
        .take()
        .unwrap_or_else(|| die("No stderr pipe for child"));

    let worker = spawn(move || {
        if trace_console {
            socket_stream_bridge(
                tracing::StderrTracer::new(),
                listener,
                child_stdin,
                child_stdout,
                child_stderr,
                closer_read,
            )
        } else {
            socket_stream_bridge(
                tracing::NoopTracer,
                listener,
                child_stdin,
                child_stdout,
                child_stderr,
                closer_read,
            )
        }
    });

    let wait_result = child.wait();
    let to_close = [1u8; 1];
    closer_write.write_all(&to_close).unwrap_or_else(|err| {
        die(format!(
            "Could not notify writer about child death: {}",
            err
        ))
    });

    worker
        .join() // Two levels of failure: Result<Result<(), IOError>, ThreadPanic>
        .unwrap_or_else(|err| die(format!("Worker thread panicked: {:?}", err)))
        .unwrap_or_else(|err| die(format!("Worker thread died: {:?}", err)));

    remove_file(&sock_path).unwrap_or_else(|err| die(format!("Could not remove socket: {}", err)));

    match wait_result {
        Ok(status) => exit(status.code().unwrap_or(254)),
        Err(err) => {
            eprintln!("Failed to wait for child: {}", err);
            exit(255)
        }
    }
}
