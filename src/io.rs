use std::io::{self, stdout, Stdout};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::prelude::RawFd;
use std::process::{ChildStderr, ChildStdin, ChildStdout};

use crate::buffer::*;
use crate::channel_manager::*;
use crate::polled_fd::*;
use crate::pty::*;

/// The opposite of [`Shutdown`], this indicates what operations a socket may
/// accept. A socket can accept both reads and writes, or just one or the other,
/// depending on whether the remote end has performed a shutdown or not.
#[derive(PartialEq)]
enum SocketAvailability {
    ReadOnly,
    WriteOnly,
    ReadWrite,
}

/// Combines a [`UnixStream`] with information about what directions still allow
/// IO.
struct UnixClient {
    stream: UnixStream,
    availability: SocketAvailability,
}

/// Abstracts over pipe-based stdio and pty-based stdio when interacting with
/// child processes.
pub trait ChildStdio {
    /// Registers any events for the child process stdio file descriptors.
    fn initialize(&mut self, fd_set: &mut PolledFdSet);

    /// Returns the file descriptor that writes to the child's stdin, or None if
    /// the stdin has been closed.
    fn stdin_fd(&self) -> Option<RawFd>;

    /// Returns the file descriptor that reads from the child's stdout, or None
    /// if the stdout has been closed.
    fn stdout_fd(&self) -> Option<RawFd>;

    /// Returns the file descriptor that reads from the child's stderr, or None
    /// if the stderr has been closed.
    fn stderr_fd(&self) -> Option<RawFd>;

    /// Reads from the provided buffer into stdin.
    fn write_stdin(&mut self, buffer: &mut FlipBuffer) -> io::Result<(bool, BufferStateChange)>;

    /// Writes from stdout into the provided buffer.
    fn read_stdout(&mut self, buffer: &mut FlipBuffer) -> io::Result<(bool, BufferStateChange)>;

    /// Writes from stderr into the provided buffer.
    fn read_stderr(&mut self, buffer: &mut FlipBuffer) -> io::Result<(bool, BufferStateChange)>;

    /// Closes the provided file descriptor, if it's one of the child's stdio
    /// file descriptors.
    fn hangup(&mut self, fd: RawFd, fd_set: &mut PolledFdSet);
}

/// Used to implement [`ChildStdio`] for pipe-based stdio. stdin, stdout, and
/// stderr are all distinct pipes that can be read/written and closed
/// indepdendently.
pub struct ChildPipes {
    stdin: Option<ChildStdin>,
    stdout: Option<ChildStdout>,
    stderr: Option<ChildStderr>,
}

impl ChildPipes {
    pub fn new(stdin: ChildStdin, stdout: ChildStdout, stderr: ChildStderr) -> Self {
        ChildPipes {
            stdin: Some(stdin),
            stdout: Some(stdout),
            stderr: Some(stderr),
        }
    }
}

impl ChildStdio for ChildPipes {
    fn initialize(&mut self, fd_set: &mut PolledFdSet) {
        fd_set.register(self.stdin.as_ref().unwrap(), epoll::Events::empty());
        fd_set.register(self.stdout.as_ref().unwrap(), epoll::Events::EPOLLIN);
        fd_set.register(self.stderr.as_ref().unwrap(), epoll::Events::EPOLLIN);
    }

    fn stdin_fd(&self) -> Option<RawFd> {
        self.stdin.as_ref().map(|f| f.as_raw_fd())
    }

    fn stdout_fd(&self) -> Option<RawFd> {
        self.stdout.as_ref().map(|f| f.as_raw_fd())
    }

    fn stderr_fd(&self) -> Option<RawFd> {
        self.stderr.as_ref().map(|f| f.as_raw_fd())
    }

    fn hangup(&mut self, fd: RawFd, fd_set: &mut PolledFdSet) {
        if self.stdin_fd() == Some(fd) {
            fd_set.unregister_closed(&fd);
            self.stdin = None;
        } else if self.stdout_fd() == Some(fd) {
            fd_set.unregister_closed(&fd);
            self.stdout = None;
        } else if self.stderr_fd() == Some(fd) {
            fd_set.unregister_closed(&fd);
            self.stderr = None;
        }
    }

    fn write_stdin(&mut self, buffer: &mut FlipBuffer) -> io::Result<(bool, BufferStateChange)> {
        if let Some(stdin) = self.stdin.as_mut() {
            buffer_to_channel(buffer, stdin)
        } else {
            return Ok((false, BufferStateChange::None));
        }
    }

    fn read_stdout(&mut self, buffer: &mut FlipBuffer) -> io::Result<(bool, BufferStateChange)> {
        if let Some(stdout) = self.stdout.as_mut() {
            channel_to_buffer(stdout, buffer)
        } else {
            return Ok((false, BufferStateChange::None));
        }
    }

    fn read_stderr(&mut self, buffer: &mut FlipBuffer) -> io::Result<(bool, BufferStateChange)> {
        if let Some(stderr) = self.stdout.as_mut() {
            channel_to_buffer(stderr, buffer)
        } else {
            return Ok((false, BufferStateChange::None));
        }
    }
}

/// Used to implement [`ChildStdio`] for pty-based stdio. There is only one
/// pseudoterminal that is used for reads and writes to the child's stdin,
/// stdout, and stderr.
pub struct ChildPty {
    pty: Option<PtyFile>,
}

impl ChildPty {
    pub fn new(pty: PtyFile) -> Self {
        ChildPty { pty: Some(pty) }
    }
}

impl ChildStdio for ChildPty {
    fn initialize(&mut self, fd_set: &mut PolledFdSet) {
        fd_set.register(self.pty.as_ref().unwrap(), epoll::Events::EPOLLIN);
    }

    fn stdin_fd(&self) -> Option<RawFd> {
        self.pty.as_ref().map(|p| p.as_raw_fd())
    }

    fn stdout_fd(&self) -> Option<RawFd> {
        self.stdin_fd()
    }

    fn stderr_fd(&self) -> Option<RawFd> {
        // Technically the pty contains both stdout and stderr, but avoid
        // returning that same fd for both roles since it could lead to
        // double-reads when two readability checks are done on the fd.
        None
    }

    fn hangup(&mut self, fd: RawFd, fd_set: &mut PolledFdSet) {
        if self.stdin_fd() == Some(fd) {
            fd_set.unregister_closed(self.pty.as_ref().unwrap());
            self.pty = None;
        }
    }

    fn write_stdin(&mut self, buffer: &mut FlipBuffer) -> io::Result<(bool, BufferStateChange)> {
        if let Some(pty) = self.pty.as_mut() {
            buffer_to_channel(buffer, pty)
        } else {
            return Ok((false, BufferStateChange::None));
        }
    }

    fn read_stdout(&mut self, buffer: &mut FlipBuffer) -> io::Result<(bool, BufferStateChange)> {
        if let Some(pty) = self.pty.as_mut() {
            channel_to_buffer(pty, buffer)
        } else {
            return Ok((false, BufferStateChange::None));
        }
    }

    fn read_stderr(&mut self, _: &mut FlipBuffer) -> io::Result<(bool, BufferStateChange)> {
        panic!("ChildPty does not support read_stderr: no dedicated stderr file descriptor");
    }
}

/// Contains all the IO channels managed by the program along with their
/// buffers.
pub struct IOSockChannelManager<Stdio: ChildStdio> {
    child: Stdio,
    console: Option<Stdout>,
    server_socket: UnixListener,
    client_socket: Option<UnixClient>,
    in_buffer: FlipBuffer,
    out_buffer: FlipBuffer,
}

impl<Stdio: ChildStdio> IOSockChannelManager<Stdio> {
    pub fn new(stdio: Stdio, server_socket: UnixListener) -> Self {
        IOSockChannelManager {
            child: stdio,
            console: Some(stdout()),
            server_socket: server_socket,
            client_socket: None,
            in_buffer: FlipBuffer::new(),
            out_buffer: FlipBuffer::new(),
        }
    }

    /// Determines if the current client socket is readable or not. It must
    /// exist and the remote end must not have shut down its write stream.
    fn client_socket_readable(&self) -> bool {
        match self.client_socket.as_ref() {
            Some(c) => c.availability != SocketAvailability::WriteOnly,
            None => false,
        }
    }

    /// Determines if the current client socket is writable or not. It must
    /// exist and the remote end must not have shut down its read stream.
    fn client_socket_writable(&self) -> bool {
        match self.client_socket.as_ref() {
            Some(c) => c.availability != SocketAvailability::ReadOnly,
            None => false,
        }
    }

    /// Determines whether there is an output channel that can accept data from
    /// the child's stdout and stderr streams. This is either the console stdout
    /// or the client Unix socket. If neither of these is available, this
    /// returns false and any data written to the output buffer should be
    /// discarded.
    fn has_output_channel(&self) -> bool {
        self.console.is_some() || self.client_socket_writable()
    }

    /// Updates the file descriptor set to poll for writable notifications on
    /// the primary write channel. Returns true if there is a primary write
    /// channel, false if there is not.
    ///
    /// **NOTE** Be careful when the primary write channel changes! If we
    /// fallback to the console writer when the client socket closes, this
    /// request has to be propagated to the secondary write channel. Otherwise
    /// we may never receive the writability notification!
    fn request_primary_output_writable(&self, fd_set: &mut PolledFdSet) -> bool {
        let fd = match (self.client_socket.as_ref(), self.console.as_ref()) {
            (Some(client), _) => client.stream.as_raw_fd(),
            (_, Some(stdout)) => stdout.as_raw_fd(),
            _ => return false,
        };

        fd_set.modify(&fd, |pfd| pfd.poll_for_write(true));
        true
    }
}

impl<Stdio: ChildStdio> ChannelManager for IOSockChannelManager<Stdio> {
    fn initialize(&mut self, fd_set: &mut PolledFdSet) {
        // OK to use unwrap here. The pollster should be calling this before
        // using any of the event dispatch functions, so these *must* be Some at
        // this point
        self.child.initialize(fd_set);
        fd_set.register(&self.server_socket, epoll::Events::EPOLLIN);
        fd_set.register(self.console.as_ref().unwrap(), epoll::Events::empty());
    }

    fn on_readable(&mut self, fd: RawFd, fd_set: &mut PolledFdSet) -> io::Result<()> {
        if fd == self.server_socket.as_raw_fd() {
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
            let (new_stream, _) = self.server_socket.accept()?;
            if self.client_socket.is_none() {
                // Promote the primary output channel if the secondary was still
                // writing.
                let mut events = epoll::Events::EPOLLIN;
                events.set(epoll::Events::EPOLLOUT, self.out_buffer.writable_size() > 0);
                fd_set.register(&new_stream, events);

                let new_client = UnixClient {
                    stream: new_stream,
                    availability: SocketAvailability::ReadWrite,
                };
                self.client_socket = Some(new_client);

                // The console may be writable, but we no longer care because
                // it's the secondary output channel. Avoid wakup loops for
                // now-useless events.
                if let Some(f) = self.console.as_ref() {
                    fd_set.modify(f, |pfd| pfd.poll_for_write(false));
                }
            }
            return Ok(());
        }

        let client_readable = self.client_socket_readable();
        match self.client_socket.as_mut() {
            Some(c) if c.stream.as_raw_fd() == fd => {
                // See the note in the on_writable implementation. This just
                // sanity checks our state against epoll's, if they disagree
                // then we have a bug.
                assert!(client_readable);

                let (should_close, buffer_state) =
                    channel_to_buffer(&mut c.stream, &mut self.in_buffer)?;

                let should_discard = if should_close {
                    if c.availability == SocketAvailability::ReadOnly {
                        self.on_hangup(fd, fd_set)?;
                    } else {
                        // Have to modify epoll flags here, otherwise we'll keep
                        // getting read notifications that will only read 0
                        // bytes
                        fd_set.modify(&fd, |pfd| pfd.poll_for_read(false));
                        c.availability = SocketAvailability::WriteOnly;
                    }

                    // Nothing was written just now, and discarding now may
                    // destroy data in the buffer that the output can copy
                    false
                } else if buffer_state == BufferStateChange::BecameNonEmpty {
                    match self.child.stdin_fd() {
                        Some(c) => {
                            fd_set.modify(&c, |pfd| pfd.poll_for_write(true));
                            false
                        }
                        None => true,
                    }
                } else {
                    self.child.stdin_fd().is_none()
                };

                if should_discard {
                    self.out_buffer.discard();
                }
                return Ok(());
            }
            _ => (),
        }

        if self.child.stdout_fd() == Some(fd) {
            let (should_close, buffer_state) = self.child.read_stdout(&mut self.out_buffer)?;

            let should_discard = if should_close {
                self.on_hangup(fd, fd_set)?;
                // Nothing was written just now, and discarding now may
                // destroy data in the buffer that the output can copy
                false
            } else if buffer_state == BufferStateChange::BecameNonEmpty {
                !self.request_primary_output_writable(fd_set)
            } else {
                !self.has_output_channel()
            };

            if should_discard {
                self.out_buffer.discard();
            }
        } else if self.child.stderr_fd() == Some(fd) {
            let (should_close, buffer_state) = self.child.read_stderr(&mut self.out_buffer)?;

            let should_discard = if should_close {
                self.on_hangup(fd, fd_set)?;
                // Nothing was written just now, and discarding now may
                // destroy data in the buffer that the output can copy
                false
            } else if buffer_state == BufferStateChange::BecameNonEmpty {
                !self.request_primary_output_writable(fd_set)
            } else {
                !self.has_output_channel()
            };

            if should_discard {
                self.out_buffer.discard();
            }
        }

        Ok(())
    }

    fn on_writable(&mut self, fd: RawFd, fd_set: &mut PolledFdSet) -> io::Result<()> {
        let client_writable = self.client_socket_writable();
        match self.client_socket.as_mut() {
            Some(c) if c.stream.as_raw_fd() == fd => {
                // To be clear, the reason for this assertion is *not* to
                // guarantee that we can write to the socket without any error
                // conditions. There's no way to do that: the client could
                // shutdown after epoll returns the event and we'd get the
                // EPIPE.
                //
                // This just ensures that our idea of the socket's state matches
                // up with how epoll is configured. If we get a write event on
                // the socket but it was just closed, or we marked its availability
                // as ReadOnly, that's an iosock bug.
                assert!(client_writable);

                let (should_close, buffer_state) =
                    buffer_to_channel(&mut self.out_buffer, &mut c.stream)?;
                if should_close {
                    if c.availability == SocketAvailability::WriteOnly {
                        self.on_hangup(fd, fd_set)?;
                    } else {
                        // Have to modify epoll flags here, otherwise we'll keep
                        // getting write notifications that will generate SIGPIPE
                        fd_set.modify(&fd, |pfd| pfd.poll_for_read(false));
                        c.availability = SocketAvailability::ReadOnly;
                    }
                } else if buffer_state == BufferStateChange::BecameEmpty {
                    fd_set.modify(&c.stream, |pfd| pfd.poll_for_write(false));
                }
                return Ok(());
            }
            _ => (),
        };

        match self.console.as_mut() {
            Some(f) if f.as_raw_fd() == fd => {
                let (should_close, buffer_state) = buffer_to_channel(&mut self.out_buffer, f)?;
                if should_close {
                    self.on_hangup(fd, fd_set)?;
                } else if buffer_state == BufferStateChange::BecameEmpty {
                    fd_set.modify(f, |pfd| pfd.poll_for_write(false));
                }
                return Ok(());
            }
            _ => (),
        };

        let client_readable = self.client_socket_readable();
        if self.child.stdin_fd() == Some(fd) {
            let (should_close, buffer_state) = self.child.write_stdin(&mut self.in_buffer)?;
            if should_close {
                self.on_hangup(fd, fd_set)?;
            } else if buffer_state == BufferStateChange::BecameEmpty && client_readable {
                fd_set.modify(&fd, |pfd| pfd.poll_for_write(false));
            }
            return Ok(());
        }

        Ok(())
    }

    fn on_hangup(&mut self, fd: RawFd, fd_set: &mut PolledFdSet) -> io::Result<()> {
        match self.client_socket.as_mut() {
            Some(c) if c.stream.as_raw_fd() == fd => {
                fd_set.unregister_closed(&c.stream);
                self.client_socket = None;

                // Fallback to the secondary output channel if it's still
                // valid and there's data to write
                if self.out_buffer.writable_size() > 0 && self.console.is_some() {
                    fd_set.modify(self.console.as_ref().unwrap(), |pfd| {
                        pfd.poll_for_write(true)
                    });
                }

                return Ok(());
            }
            _ => (),
        };

        if self.child.stdin_fd() == Some(fd)
            || self.child.stdout_fd() == Some(fd)
            || self.child.stderr_fd() == Some(fd)
        {
            self.child.hangup(fd, fd_set);
            return Ok(());
        }

        match self.console.as_mut() {
            Some(c) if c.as_raw_fd() == fd => {
                // We have to unregister stdout directly here, we can't just
                // drop self.console because dropping an instance of Stdout
                // doesn't actually close the stdout file descriptor
                fd_set.unregister(c);
                self.console = None;
                return Ok(());
            }
            _ => (),
        }

        Ok(())
    }
}
