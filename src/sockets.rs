use std::cell::Cell;
use std::fmt::Display;
use std::io::{self, stdout, PipeReader, Read, Write};
use std::mem;
use std::net::Shutdown;
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::process::{ChildStderr, ChildStdin, ChildStdout};

use epoll::{ControlOptions, Event, Events};

/// A buffer that contains 2N bytes, which allows a writer to write N bytes at a
/// time, and a reader to read N bytes at a time.
///
/// Unlike a ring buffer, all readable and writable sections of the buffer are a
/// continuous segment of memory. This allows slices of the buffer to be used
/// directly instead of maintaining a separate buffer and copying sections of
/// the ring buffer to and from it.
struct FlipBuffer<const N: usize> {
    read_page: Cell<Box<[u8]>>,
    write_page: Cell<Box<[u8]>>,
    read_cursor: usize,
    read_max: usize,
    write_cursor: usize,
}

/// The result of an operation that copies data from a buffer to an IO stream,
/// or an IO stream to a buffer.
///
/// Compared to an [`io::Result`], this conveys more information for
/// [`FlipBuffer::write_to`] and similar methods that bundle together two
/// operations. These operations could fail because the buffer is full/empty, or
/// because the underlying IO stream cannot accept/produce bytes at the moment
/// we're reading it.
enum CopyResult {
    BufferNotReady,
    IOCompleted(usize),
    IOError(io::Error)
}

/// Converts an [`io::Result`] from an IO operation (returning a byte count)
/// into a [`CopyResult`]. This never returns a [`CopyResult::BufferNotReady`]
/// because the buffer must have space available for the IO operation to begin.
impl From<io::Result<usize>> for CopyResult {
    fn from(value: io::Result<usize>) -> Self {
        match value {
            Ok(size) => CopyResult::IOCompleted(size),
            Err(err) => CopyResult::IOError(err)
        }
    }
}

impl<const N: usize> FlipBuffer<N> {
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
            write_cursor: 0,
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

    /// Provides the readable portion of this buffer to a [`std::io::Read`] to
    /// read as much as possible.
    pub fn read_from<R: Read>(&mut self, reader: &mut R) -> CopyResult {
        if self.write_cursor == N && self.read_cursor == self.read_max {
            // Steal the read buffer if there is no data that we would clobber.
            self.flip_buffers();
        }

        let writable = N - self.write_cursor;
        if writable == 0 {
            return CopyResult::BufferNotReady
        }

        let slice = &mut self.write_page.get_mut()[self.write_cursor..N];
        reader.read(slice).map(|size| {
            self.write_cursor += size;
            size
        }).into()
    }

    /// Provides the writable portion of this buffer to a [`std::io::Write`] to
    /// write as much as possible. Any bytes written will not be written again
    /// in future calls.
    pub fn write_to<W: Write>(&mut self, writer: &mut W) -> CopyResult {
        if self.read_cursor == self.read_max && self.write_cursor > 0 {
            // Steal the write buffer if it has some data we can read.
            self.flip_buffers();
        }

        let readable = self.read_max - self.read_cursor;
        if readable == 0 {
            return CopyResult::BufferNotReady
        }

        let slice = &self.read_page.get_mut()[self.read_cursor..self.read_max];
        writer.write(slice).map(|size| {
            self.read_cursor += size;
            size
        }).into()
    }

    /// Ignores any data written by [`with_write`] since the last [`with_read`]
    /// call.
    pub fn discard(&mut self) {
        self.write_cursor = 0;
    }
}

const BUFFER_BYTES: usize = 32 * 1024;

/// What ends of a socket are currently open. Iniitially the socket allows both
/// send and recv, but as we shutdown ends of it (due to the associated child
/// stdio pipes going away) or the peer does, some directions may be lost. There
/// is no "neither" option here since we close the socket when we detect a
/// second shutdown.
#[derive(Clone, Copy)]
enum SocketStatus {
    SendRecv,
    SendOnly,
    RecvOnly,
}

impl Display for SocketStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SocketStatus::SendRecv => write!(f, "SendRecv"),
            SocketStatus::RecvOnly => write!(f, "RecvOnly"),
            SocketStatus::SendOnly => write!(f, "SendOnly"),
        }
    }
}

/// Wrapper around [`std::net::Shutdown`] for implementing [`std::fmt::Display`].
struct ShutdownDisplay(Shutdown);

impl Display for ShutdownDisplay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            Shutdown::Read => write!(f, "Read"),
            Shutdown::Write => write!(f, "Write"),
            Shutdown::Both => write!(f, "Both"),
        }
    }
}

/// Manages the IO operations that copy data to and from the child process,
/// using either a Unix domain socket or our stdout.
struct IOBridge {
    // IO targets
    unix_listener: Option<UnixListener>,
    unix_client: Option<UnixStream>,
    unix_client_status: SocketStatus,
    child_stdin: ChildStdin,
    child_stdout: ChildStdout,
    child_stderr: ChildStderr,
    child_notify: PipeReader,

    // epoll events
    pollster: i32,
    // There are at most 5 targets: child in/out/err, server sock, client sock
    // epoll groups results by file descriptor so we won't get multiple events
    // on the same target in one invocation
    event_buffer: [Event; 5],

    // Buffers for child stdio pipes
    child_stdin_buf: FlipBuffer<BUFFER_BYTES>,
    child_stdouterr_buf: FlipBuffer<BUFFER_BYTES>,

    // States that represent how the bridge is 'wired'. Ignoring the notification
    // pipe (it doesn't carry real data), we have the following potential IO flows.
    // Each stream can be either open or closed.
    //
    // [unix socket] -> [in buffer] -> [child stdin]
    //       ^
    //       |        +-[child stdout]
    //       |        v
    //       +---[outerr buffer]--->[our stdout]
    //                ^
    //                +-[child stderr]
    //
    // The child's stdout and stderr is implicit in how we configure epoll. If the
    // child pipes are closed, then we won't get notifications for them. The state
    // of the socket is tracked by the Option - on the first error we set it to
    // None. The only pipes we have to track the outputs since we don't use epoll to
    // track whether pipes are writable or not.
    parent_stdout_open: bool,
    child_stdin_open: bool,
}

impl IOBridge {
    /// Creates a new uninitialized IO bridge.
    pub fn new(
        listener: UnixListener,
        child_stdin: ChildStdin,
        child_stdout: ChildStdout,
        child_stderr: ChildStderr,
        child_notify: PipeReader,
    ) -> Self {
        IOBridge {
            unix_listener: Some(listener),
            unix_client: None,
            unix_client_status: SocketStatus::SendRecv,
            child_stdin: child_stdin,
            child_stdout: child_stdout,
            child_stderr: child_stderr,
            child_notify: child_notify,
            pollster: -1,
            event_buffer: [Event::new(Events::empty(), 0); _],
            child_stdin_buf: FlipBuffer::new(),
            child_stdouterr_buf: FlipBuffer::new(),
            child_stdin_open: true,
            parent_stdout_open: true,
        }
    }

    /// Registers `target` with our epoll instance, configured to trigger when
    /// data is readable from it.
    fn epoll_add_readable<T: AsRawFd>(&self, target: &T) -> io::Result<()> {
        let target_fd = target.as_raw_fd();
        epoll::ctl(
            self.pollster,
            ControlOptions::EPOLL_CTL_ADD,
            target_fd,
            Event::new(Events::EPOLLIN, target_fd as u64),
        )
    }

    /// Unregisters `target` with our epoll instance.
    fn epoll_del<T: AsRawFd>(&self, target: &T) -> io::Result<()> {
        epoll::ctl(
            self.pollster,
            ControlOptions::EPOLL_CTL_DEL,
            target.as_raw_fd(),
            // Required but not used, we just have to put something here
            Event::new(Events::EPOLLERR, 0),
        )
    }

    /// Registers all file descriptors with epoll.
    fn init_pollster(&mut self) -> io::Result<()> {
        self.pollster = epoll::create(true)?;
        self.epoll_add_readable(&self.child_stdout)?;
        self.epoll_add_readable(&self.child_stderr)?;
        if let Some(unix_listener) = self.unix_listener.as_ref() {
            self.epoll_add_readable(unix_listener)?;
        }
        self.epoll_add_readable(&self.child_notify)
    }

    /// Updates the client socket based on a send or receive operation returning
    /// an error. If the socket was already partially shutdown, it is closed
    /// instead.
    fn shutdown_unix_client(&mut self, direction: Shutdown) -> io::Result<()> {
        if let Some(unix_client) = mem::take(&mut self.unix_client) {
            self.unix_client = match (direction, &self.unix_client_status) {
                (Shutdown::Read, SocketStatus::SendRecv) => {
                    self.unix_client_status = SocketStatus::SendOnly;
                    // NOTE: '?' here discards the client since we moved out of
                    // self.unix_client and didn't replace it. This is
                    // intentional - the only interesting error here is ENOTCONN
                    // which we would discard the socket for anyway.
                    unix_client.shutdown(direction)?;
                    Some(unix_client)
                }
                (Shutdown::Write, SocketStatus::SendRecv) => {
                    self.unix_client_status = SocketStatus::RecvOnly;
                    unix_client.shutdown(direction)?;
                    Some(unix_client)
                }

                // Should not happen: the socket is already shutdown on the end
                // that we're shutting down here. Not an error but report it for
                // debugging.
                (Shutdown::Write, SocketStatus::RecvOnly) => {
                    eprintln!(
                        "Warning: redundant shutdown on socket (dir={} status={})",
                        ShutdownDisplay(direction),
                        self.unix_client_status
                    );
                    Some(unix_client)
                }

                // Note that this Shutdown::Read check includes both RecvOnly and SendOnly mode.
                // Including SendOnly avoids an issue where the child process doesn't do any IO and
                // the only way we know the socket is closed is by repeated recv requests returning
                // 0 bytes.
                (Shutdown::Read, _)
                | (Shutdown::Write, SocketStatus::SendOnly)
                | (Shutdown::Both, _) => None,
            };
        }
        Ok(())
    }

    /// Drains events from epoll and copies data from readable sockets into the
    /// corresponding buffers. Returns true if an event was received on the
    /// notification channel, and false otherwise.
    fn fill_buffers(&mut self) -> io::Result<bool> {
        let events = epoll::wait(self.pollster, -1, &mut self.event_buffer)?;
        let child_notify_fd = self.child_notify.as_raw_fd();
        for i in 0..events {
            if self.event_buffer[i].data as i32 == child_notify_fd {
                return Ok(true);
            }
        }

        let listener_fd = self
            .unix_listener
            .as_ref()
            .map(|s| s.as_raw_fd())
            .unwrap_or(-1);
        let child_stdout_fd = self.child_stdout.as_raw_fd();
        let child_stderr_fd = self.child_stderr.as_raw_fd();
        for i in 0..events {
            let target = self.event_buffer[i].data as i32;

            // Note that none of these operations depend on whether there is a
            // destination on the other side of the buffer. We *must* read
            // things to prevent the writers from blocking due to lack of write
            // buffer space.
            if target == child_stdout_fd {
                match self.child_stdouterr_buf.read_from(&mut self.child_stdout) {
                    CopyResult::IOCompleted(0) => {
                        return self.epoll_del(&child_stderr_fd).map(|_| false)
                    }
                    CopyResult::IOError(_) => {
                        return self.epoll_del(&child_stdout_fd).map(|_| false)
                    }
                    _ => ()
                }
            } else if target == child_stderr_fd {
                match self.child_stdouterr_buf.read_from(&mut self.child_stderr) {
                    CopyResult::IOCompleted(0) => {
                        return self.epoll_del(&child_stderr_fd).map(|_| false)
                    }
                    CopyResult::IOError(_) => {
                        return self.epoll_del(&child_stderr_fd).map(|_| false)
                    }
                    _ => ()
                }
            } else if target == listener_fd {
                let unix_listener = self.unix_listener.as_ref().unwrap();
                match unix_listener.accept() {
                    Ok((new_client, _)) => {
                        if self.unix_client.is_some() {
                            drop(new_client)
                        } else {
                            self.epoll_add_readable(&new_client)?;
                            self.unix_client = Some(new_client);
                            self.unix_client_status = SocketStatus::SendRecv;
                        }
                    }
                    Err(err) => {
                        // In theory there are non-fatal errors here, but don't
                        // consider them:
                        //
                        // - Connection init errors: per the Linux accept(2)
                        //   manpage, errors from protocol handshake are
                        //   surfaced in accept if they happen early enough.
                        //   This should be impossible for Unix domain sockets
                        //   since there is neither TCP nor IP here.
                        //
                        // - ECONNABORTED: Only set by accept if the peer address
                        //   cannot be determined which should be impossible for
                        //   Unix domain sockets. (FIXME: Unless the peer dies
                        //   during connect()? Is that possible?)
                        //
                        //   https://github.com/torvalds/linux/blob/b69053dd3ffbc0d2dedbbc86182cdef6f641fe1b/net/socket.c#L1995
                        eprintln!("Error accepting listener socket: {}", err);
                        self.epoll_del(&listener_fd)?
                    }
                }
            } else if let Some(client) = self.unix_client.as_mut() {
                match self.child_stdin_buf.read_from(client) {
                    CopyResult::IOCompleted(0) => {
                        self.shutdown_unix_client(Shutdown::Read)?
                    },
                    CopyResult::IOError(err) => {
                        eprintln!("Error reading from client socket: {}", err);
                        self.unix_client = None;
                    }
                    _ => (),
                }
            }
        }

        Ok(false)
    }

    /// Copies the child input buffer into the child stdin, and the child output
    /// buffer to either the socket or our stdout. If no destination is available
    /// for either operation then discard the original buffer instead.
    fn send_buffers(&mut self) -> io::Result<()> {
        if self.child_stdin_open {
            match self.child_stdin_buf.write_to(&mut self.child_stdin) {
                CopyResult::IOCompleted(0) => {
                    self.child_stdin_buf.discard();
                    self.child_stdin_open = false;
                },
                CopyResult::IOError(err) => {
                    eprintln!("Error writing to child stdin: {}", err);
                    self.child_stdin_buf.discard();
                    self.child_stdin_open = false;
                },
                _ => ()
            }
        } else {
            self.child_stdin_buf.discard();
        }

        match (
            self.unix_client.as_mut(),
            self.unix_client_status,
            self.parent_stdout_open,
        ) {
            (None, _, false) | (Some(_), SocketStatus::RecvOnly, _) => {
                self.child_stdouterr_buf.discard()
            }
            (None, _, true) => {
                let parent_stdout_closed =
                    match self.child_stdouterr_buf.write_to(&mut stdout()) {
                        CopyResult::IOCompleted(0)  => true,
                        CopyResult::IOError(err) if err.kind() == io::ErrorKind::BrokenPipe => true,
                        CopyResult::IOError(err) => return Err(err),
                        _ => false
                    };
                if parent_stdout_closed {
                    // Don't log this, if stdout is dead then stderr probably is too
                    self.parent_stdout_open = false;
                    self.child_stdouterr_buf.discard();
                }
            }
            (Some(client), _, _) => {
                let unix_client_closed =
                    match self.child_stdouterr_buf.write_to(client) {
                        CopyResult::IOCompleted(0) => true,
                        CopyResult::IOError(err) if err.kind() == io::ErrorKind::BrokenPipe => true,
                        CopyResult::IOError(err) => return Err(err),
                        _ => false
                    };
                if unix_client_closed {
                    self.shutdown_unix_client(Shutdown::Write)?;
                    self.child_stdouterr_buf.discard();
                }
            }
        }

        Ok(())
    }

    /// Initializes the epoll listener and copies data until a termination
    /// request is received from the main thread.
    pub fn run(&mut self) -> io::Result<()> {
        self.init_pollster()?;
        while !self.fill_buffers()? {
            self.send_buffers()?;
        }
        Ok(())
    }
}

/// Accepts connections on the Unix socket `listener`, and passes data from it
/// to `child_stdin`, and data from `child_stdout` and `child_stderr` to
/// `listener`. Stops when data is received on the `child_notify` pipe.
///
/// When there is no client on `listener` the data from the child process is
/// written to the stdout and stderr of this process.
pub fn socket_stream_bridge(
    listener: UnixListener,
    child_stdin: ChildStdin,
    child_stdout: ChildStdout,
    child_stderr: ChildStderr,
    child_notify: PipeReader,
) -> io::Result<()> {
    let mut bridge = IOBridge::new(listener, child_stdin, child_stdout, child_stderr, child_notify);
    bridge.run()
}
