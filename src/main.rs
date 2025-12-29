mod signals;

use std::cell::Cell;
use std::env::args;
use std::fmt::Display;
use std::fs::remove_file;
use std::io::{self, PipeReader, Read, Write, pipe, stdout};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::process::{ChildStderr, ChildStdin, ChildStdout, Command, Stdio, exit};
use std::thread::spawn;

use epoll::{ControlOptions, Event, Events};
use signals::kill_child_on_signal;

/// A buffer that contains 2N bytes, which allows a writer to write N bytes at a
/// time, and a reader to read N bytes at a time.
///
/// Unlike a ring buffer, all readable and writable sections of the buffer are a
/// continuous segment of memory. This allows slices of the buffer to be used
/// directly instead of maintaining a separate buffer and copying sections of
/// the ring buffer to and from it.
struct FlipBuffer<T: Default + Copy, const N: usize> {
    read_page: Cell<Box<[T]>>,
    write_page: Cell<Box<[T]>>,
    read_cursor: usize,
    read_max: usize,
    write_cursor: usize,
}

impl<T: Default + Copy, const N: usize> FlipBuffer<T, N> {
    /// Creates a new FlipBuffer with the provided initial value.
    pub fn new() -> Self {
        let mut empty_read = Vec::with_capacity(N);
        empty_read.resize(N, Default::default());

        let mut empty_write = Vec::with_capacity(N);
        empty_write.resize(N, Default::default());

        FlipBuffer {
            read_page: Cell::new(empty_read.into_boxed_slice()),
            write_page: Cell::new(empty_write.into_boxed_slice()),
            read_cursor: 0,
            read_max: 0,
            write_cursor: 0
        }
    }

    /// Swaps the role of the read buffer and the write buffer. The write buffer
    /// becomes readable, ending at the position where the writer stopped
    /// writing. The read buffer becomes writable.
    ///
    /// After this point any data in the write buffer (what used to be the read
    /// buffer) is be unreadable.
    fn flip_buffers(&mut self) {
        let written = self.write_cursor;
        self.read_cursor = 0;
        self.read_max = written;
        self.write_cursor = 0;
        self.read_page.swap(&self.write_page);
    }

    /// Invokes `func` with a slice of readable data. If no readable data is
    /// available, the slice is empty.
    ///
    /// `func` should return the number of bytes that were consumed from what
    /// was available. If `func` returns a value larger than the size of the
    /// slice, this function panics.
    pub fn with_read<E, F: FnOnce(&[T]) -> Result<usize, E>>(&mut self, func: F) -> Result<usize, E> {
        if self.read_cursor == self.read_max && self.write_cursor > 0 {
            // Steal the write buffer if it has some data we can read.
            self.flip_buffers();
        }

        let slice = &self.read_page.get_mut()[self.read_cursor..self.read_max];
        let readable = self.read_max - self.read_cursor;
        match func(slice) {
            Ok(was_read) => {
                if was_read > readable {
                    panic!("Readable range is {}..{} ({} bytes) but {} was consumed", self.read_cursor, self.read_max, readable, was_read);
                }
                self.read_cursor += was_read;
                Ok(was_read)
            },
            Err(err) => Err(err)
        }
    }

    /// Invokes `func` with a slice of writable data. If no writable data is
    /// available, the slice is empty.
    ///
    /// `func` should return the number of bytes that were written into the
    /// provided slice. If `func` returns a value larger than the size of that
    /// slice, this function panics.
    pub fn with_write<E, F: FnOnce(&mut [T]) -> Result<usize, E>>(&mut self, func: F) -> Result<usize, E> {
        if self.write_cursor == N  && self.read_cursor == self.read_max {
            // Steal the read buffer if there is no data that we would clobber.
            self.flip_buffers();
        }

        let slice = &mut self.write_page.get_mut()[self.write_cursor..N];
        let writable = N - self.write_cursor;
        match func(slice) {
            Ok(was_written) => {
                if was_written > writable {
                    panic!("Writable range is {}..{} ({} bytes) but {} was written", self.write_cursor, N, writable, was_written);
                }
                self.write_cursor += was_written;
                Ok(was_written)
            },
            Err(err) => Err(err)
        }
    }
}

/// Prints `msg` to stderr and exits the process.
fn die<D: Display>(msg: D) -> ! {
    eprintln!("{}", msg);
    exit(255)
}

/// Displays the command-line help and exits the process.
fn usage() -> ! {
    die("Usage: iosock SOCKET_PATH PROGRAM ARGS...")
}

/// Registers `target` with the epoll socket `pollster`, configured to trigger
/// when the provided `events` occur. The data for the even is `target` as a
/// file descriptor.
fn epoll_add<P: AsRawFd, T: AsRawFd>(pollster: &P, target: &T, events: Events) -> io::Result<()> {
    let target_fd = target.as_raw_fd();
    epoll::ctl(
        pollster.as_raw_fd(),
        ControlOptions::EPOLL_CTL_ADD,
        target_fd,
        Event::new(events, target_fd as u64),
    )
}

/// Accepts connections on the Unix socket `listener`, and passes data from it
/// to `child_stdin`, and data from `child_stdout` and `child_stderr` to
/// `listener`. Stops when data is received on the `child_notify` pipe.
///
/// When there is no client on `listener` the data from the child process is
/// written to the stdout and stderr of this process.
fn socket_stream_bridge(
    listener: UnixListener,
    mut child_stdin: ChildStdin,
    mut child_stdout: ChildStdout,
    mut child_stderr: ChildStderr,
    child_notify: PipeReader,
) -> io::Result<()> {
    let pollster = epoll::create(true)?;
    let mut socket_client: Option<UnixStream> = None;

    epoll_add(&pollster, &child_stdout, Events::EPOLLIN)?;
    epoll_add(&pollster, &child_stderr, Events::EPOLLIN)?;
    epoll_add(&pollster, &listener, Events::EPOLLIN)?;
    epoll_add(&pollster, &child_notify, Events::EPOLLIN)?;

    let mut stdout = stdout();

    let child_stdout_fd = child_stdout.as_raw_fd();
    let child_stderr_fd = child_stderr.as_raw_fd();
    let listener_fd = listener.as_raw_fd();
    let notify_fd = child_notify.as_raw_fd();

    let mut events_buf = [Event::new(Events::empty(), 0); 8];

    const BUFFER_BYTES: usize = 32 * 1024;
    let mut child_stdin_buf: FlipBuffer<u8, BUFFER_BYTES> = FlipBuffer::new();
    let mut child_stdouterr_buf: FlipBuffer<u8, BUFFER_BYTES> = FlipBuffer::new();

    'events: loop {
        let events = epoll::wait(pollster, -1, &mut events_buf)?;
        for i in 0..events {
            let target = events_buf[i].data as i32;
            if target == notify_fd {
                break 'events
            } else if target == child_stdout_fd {
                child_stdouterr_buf.with_write(|buf| child_stdout.read(buf))?;
            } else if target == child_stderr_fd {
                child_stdouterr_buf.with_write(|buf| child_stderr.read(buf))?;
            } else if target == listener_fd {
                let (new_client, _) = listener.accept()?;
                if socket_client.is_some() {
                    drop(new_client);
                } else {
                    epoll_add(&pollster, &new_client, Events::EPOLLIN)?;
                    socket_client = Some(new_client);
                }
            } else if let Some(client) = socket_client.as_mut() {
                let copied = child_stdin_buf.with_write(|buf| client.read(buf))?;
                if copied == 0 {
                    socket_client = None;
                }
            }
        }

        child_stdin_buf.with_read(|buf| child_stdin.write(buf))?;
        match socket_client.as_mut() {
            Some(client) => child_stdouterr_buf.with_read(|buf| client.write(buf))?,
            None => child_stdouterr_buf.with_read(|buf| stdout.write(buf))?
        };
    }

    Ok(())
}

fn main() {
    let mut args = args();
    args.next();
    let sock_path = args.next().unwrap_or_else(|| usage());
    let command_line: Vec<String> = args.collect();
    if command_line.len() == 0 {
        usage();
    }

    let listener = UnixListener::bind(&sock_path)
        .unwrap_or_else(|err| die(format!("Could not create socket: {}", err)));

    let (closer_read, mut closer_write) = pipe()
        .unwrap_or_else(|err| die(format!("Could not create notification pipe: {}", err)));

    let mut child = Command::new(&command_line[0])
        .args(&command_line[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|err| die(format!("Could not start process: {}", err)));

    kill_child_on_signal(child.id() as i32)
        .unwrap_or_else(|err| die(format!("Could not bind signal handler: {}", err)));

    let child_stdin = child.stdin.take().unwrap_or_else(|| die("No stdin pipe for child"));
    let child_stdout = child.stdout.take().unwrap_or_else(|| die("No stdout pipe for child"));
    let child_stderr = child.stderr.take().unwrap_or_else(|| die("No stderr pipe for child"));

    let worker = spawn(move || {
        socket_stream_bridge(listener, child_stdin, child_stdout, child_stderr, closer_read)
    });

    let wait_result = child.wait();
    let to_close = [1u8; 1];
    closer_write.write_all(&to_close)
                .unwrap_or_else(|err| die(format!("Could not notify writer about child death: {}", err)));

    worker.join() // Two levels of failure: Result<Result<(), IOError>, ThreadPanic>
          .unwrap_or_else(|err| die(format!("Worker thread panicked: {:?}", err)))
          .unwrap_or_else(|err| die(format!("Worker thread died: {:?}", err)));

    remove_file(&sock_path)
        .unwrap_or_else(|err| die(format!("Could not remove socket: {}", err)));

    match wait_result {
        Ok(status) => exit(status.code().unwrap_or(254)),
        Err(err) => {
            eprintln!("Failed to wait for child: {}", err);
            exit(255)
        }
    }
}
