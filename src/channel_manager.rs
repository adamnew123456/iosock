use std::io::{self, ErrorKind, Read, Write};
use std::os::fd::RawFd;

use crate::buffer::*;
use crate::polled_fd::*;

/// Indicates an "expected" IO error that should only close the associated IO
/// channel. If this returns false, the IO error should kill the program.
pub fn is_nonfatal_io_error(error: io::ErrorKind) -> bool {
    match error {
        ErrorKind::ConnectionReset | ErrorKind::BrokenPipe => true,
        _ => false,
    }
}

/// Reads data from the channel into the buffer. The return value's [`Ok`] case
/// indicates whether the channel should be closed (`true` meaning "close it"),
/// and what the status of the buffer is.
pub fn channel_to_buffer<R: Read>(
    read: &mut R,
    buffer: &mut FlipBuffer,
) -> io::Result<(bool, BufferStateChange)> {
    let result = match buffer.read_or_drain(read) {
        (CopyResult::IOCompleted(0), state) => (true, state),
        (CopyResult::IOError(err), state) if is_nonfatal_io_error(err) => (true, state),
        (CopyResult::IOError(err), _) => return Err(err.into()),
        (_, state) => (false, state),
    };
    Ok(result)
}

/// Writes data from the buffer into the channel. The return value's [`Ok`] case
/// indicates whether the channel should be closed (`true` meaning "close it"),
/// and what the status of the buffer is.
pub fn buffer_to_channel<W: Write>(
    buffer: &mut FlipBuffer,
    write: &mut W,
) -> io::Result<(bool, BufferStateChange)> {
    let result = match buffer.write_to(write) {
        (CopyResult::IOCompleted(0), state) => (true, state),
        (CopyResult::IOError(err), state) if is_nonfatal_io_error(err) => (true, state),
        (CopyResult::IOError(err), _) => return Err(err.into()),
        (_, state) => (false, state),
    };
    Ok(result)
}

pub trait ChannelManager {
    /// Returns an initial list of file descriptors to monitor. Called before any other events.
    fn initialize(&mut self, fd_set: &mut PolledFdSet);

    /// Called when a file descriptor registered with [`PollReadable`] becomes readable.
    fn on_readable(&mut self, fd: RawFd, fd_set: &mut PolledFdSet) -> io::Result<()>;

    /// Called when a file descriptor registered with [`PollWritable`] becomes readable.
    fn on_writable(&mut self, fd: RawFd, fd_set: &mut PolledFdSet) -> io::Result<()>;

    /// Called when the remote end of a file descriptor is closed. This can be either a readable
    /// or writable file descriptor.
    fn on_hangup(&mut self, fd: RawFd, fd_set: &mut PolledFdSet) -> io::Result<()>;
}
