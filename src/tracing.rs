use std::{fmt::Display, io::{self, Write, stderr}};

/// The IO entity that a trace event is about.
pub enum TraceStream {
    ChildStdin,
    ChildStdout,
    ChildStderr,
    SocketRead,
    SocketWrite,
    Console
}

impl Display for TraceStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TraceStream::ChildStdin => write!(f, "stdin"),
            TraceStream::ChildStdout => write!(f, "stdout"),
            TraceStream::ChildStderr => write!(f, "stderr"),
            TraceStream::SocketRead => write!(f, "socket:recv"),
            TraceStream::SocketWrite => write!(f, "socket:send"),
            TraceStream::Console => write!(f, "console")
        }
    }
}

pub enum TraceBuffer {
    ChildInput,
    ChildOutput
}

impl Display for TraceBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TraceBuffer::ChildInput => write!(f, "(stdin)"),
            TraceBuffer::ChildOutput => write!(f, "(stdout/stderr)"),
        }
    }
}

pub enum TraceEvent {
    // We don't trace all possible events here:
    //
    // - Socket shutdown failures. shutdown(2) gives only four error conditions.
    //   ENOTSOCK and EINVAL should be impossible because of how the socket FD
    //   is encapsulated. EBADF might be possible after closing the socket but
    //   we don't care because the socket is closed no matter what. I'm least
    //   sure about ENOTCONN but assuming it means the socket was *never*
    //   connected (*currently* not connected would mean zero-byte receives)
    //   this should be impossible since we got the FD from an accept call.
    //
    // - Redundant shutdowns. These are traced the same as another shutdown.
    //   I don't antipcate needing this, but it can be added if need be.

    /// A client socket connected and was bound to the child stdio
    ClientAccepted,
    /// A client socket tried to connect but we rejected it because another
    /// socket connected before
    ClientRejected,
    /// A client socket tried to connect but we received an IO error when
    /// accepting it
    ClientAcceptFailed(io::ErrorKind),
    /// The socket was shutdown on both ends at once.
    ClientClosed,
    /// Data was transferred between a buffer and an IO stream
    Transferred(TraceBuffer, TraceStream, usize),
    /// The buffer was too full/empty to accept/provide data from the IO stream
    TransferFailedBuffer(TraceBuffer, TraceStream),
    /// The buffer was able to accept/provide data but the IO stream could not
    /// process it
    TransferFailedIO(TraceBuffer, TraceStream, io::ErrorKind),
    /// Data from a buffer was discarded
    Discarded(TraceBuffer),
    /// An IO stream was closed
    Closed(TraceStream)
}

impl Display for TraceEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TraceEvent::ClientAccepted => write!(f, "client-accepted"),
            TraceEvent::ClientRejected => write!(f, "client-rejected"),
            TraceEvent::ClientAcceptFailed(err) => write!(f, "client-accept-error,{}", err),
            TraceEvent::ClientClosed => write!(f, "client-closed"),
            TraceEvent::Transferred(buf, stream, bytes) => write!(f, "transferred,{},{},{}", buf, stream, bytes),
            TraceEvent::TransferFailedBuffer(buf, stream) => write!(f, "transfer-failed-buffer,{},{}", buf, stream),
            TraceEvent::TransferFailedIO(buf, stream, err) => write!(f, "transfer-failed-io,{},{},{}", buf, stream, err),
            TraceEvent::Discarded(buf) => write!(f, "discarded,{}", buf),
            TraceEvent::Closed(stream) => write!(f, "closed,{}", stream),
        }
    }
}

/// Interface for receiving information about the IO operations that
/// [`crate::sockets::IOBridge`] performs.
pub trait BridgeTracer {
    fn on_trace(&mut self, event: TraceEvent);
}

pub struct NoopTracer;

impl BridgeTracer for NoopTracer {
    fn on_trace(&mut self, _: TraceEvent) {}
}

pub struct StderrTracer {
    open: bool
}

impl StderrTracer {
    pub fn new() -> Self {
        StderrTracer { open: true }
    }
}

impl BridgeTracer for StderrTracer {
    fn on_trace(&mut self, event: TraceEvent) {
        if self.open {
            let mut console = stderr();
            let message = format!("{}\n", event);
            if console.write_all(message.as_bytes()).is_err() {
                self.open = false;
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    pub struct LogTracer {
        events: Vec<TraceEvent>
    }

    impl LogTracer {
        pub fn new() -> Self {
            LogTracer { events: Vec::new() }
        }
    }

    impl BridgeTracer for LogTracer {
        fn on_trace(&mut self, event: TraceEvent) {
            self.events.push(event);
        }
    }
}
