use std::env::args;
use std::fs::remove_file;
use std::io::{self, PipeReader, Write, pipe};
use std::os::unix::net::UnixListener;
use std::process::{exit, Child, Command, Stdio};
use std::thread::{spawn, JoinHandle};

use iosock::die;
use iosock::io::{ChildPipes, ChildPty, IOSockChannelManager};
use iosock::pollster::Pollster;
use iosock::pty::*;
use iosock::signals::kill_child_on_signal;

/// Displays the command-line help and exits the process.
fn usage() -> ! {
    die("Usage: iosock [-p|--pty] SOCKET_PATH PROGRAM ARGS...")
}

fn run_stdio_pipes(mut command: Command, listener: UnixListener, closer_read: PipeReader) -> (Child, JoinHandle<io::Result<()>>) {
    command.stdin(Stdio::piped());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .unwrap_or_else(|err| die(format!("Could not start process: {}", err)));

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

    let worker = spawn(move || -> io::Result<()> {
        let pipe_stdio = ChildPipes::new(child_stdin, child_stdout, child_stderr);
        let channel_manager = IOSockChannelManager::new(pipe_stdio, listener);
        let pollster = Pollster::new(channel_manager, &closer_read).or_else(|err| {
            eprintln!("IO thread could not create pollster: {err}");
            Err(err)
        })?;

        pollster.run().or_else(|err| {
            eprintln!("IO thread died: {err}");
            Err(err)
        })
    });

    (child, worker)
}

fn run_stdio_pty(command: Command, listener: UnixListener, closer_read: PipeReader) -> (Child, JoinHandle<io::Result<()>>) {
    let pty = Pty::open()
        .unwrap_or_else(|err| die(format!("Could not start pty: {}", err)));

    let (child, pty_master) = pty.spawn(command)
        .unwrap_or_else(|err| die(format!("Could not start process: {}", err)));

    let worker = spawn(move || -> io::Result<()> {
        let pty_stdio = ChildPty::new(pty_master);
        let channel_manager = IOSockChannelManager::new(pty_stdio, listener);
        let pollster = Pollster::new(channel_manager, &closer_read).or_else(|err| {
            eprintln!("IO thread could not create pollster: {err}");
            Err(err)
        })?;

        pollster.run().or_else(|err| {
            eprintln!("IO thread died: {err}");
            Err(err)
        })
    });

    (child, worker)
}

fn main() {
    let mut args = args();
    args.next();

    let mut use_pty = false;
    let mut sock_path = args.next().unwrap_or_else(|| usage());
    if sock_path == "-p" || sock_path == "--pty" {
        use_pty = true;
        sock_path = args.next().unwrap_or_else(|| usage());
    }

    let command_line: Vec<String> = args.collect();
    if command_line.len() == 0 {
        usage();
    }

    let listener = UnixListener::bind(&sock_path)
        .unwrap_or_else(|err| die(format!("Could not create socket: {}", err)));

    let (closer_read, mut closer_write) =
        pipe().unwrap_or_else(|err| die(format!("Could not create notification pipe: {}", err)));

    let mut proc = Command::new(&command_line[0]);
    proc.args(&command_line[1..]);

    let (mut child, worker) = if use_pty {
        run_stdio_pty(proc, listener, closer_read)
    } else {
        run_stdio_pipes(proc, listener, closer_read)
    };

    kill_child_on_signal(child.id() as i32)
        .unwrap_or_else(|err| die(format!("Could not bind signal handler: {}", err)));

    let wait_result = child.wait();
    let to_close = [1u8; 1];
    closer_write.write_all(&to_close).unwrap_or_else(|err| {
        eprintln!("Could not notify writer about child death: {}", err);
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
