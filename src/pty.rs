// This module implements crude support for running subprocesses under ptys.
// This is used for programs like srcds which only accept interactive input if
// isatty(stdin) is true.
//
// "Crude" here means that we don't attempt to act like a terminal, we just
// shuffle bytes from the socket to the subprocess and back. There's a *lot*
// of terminal state documented in termios(3). We only care about part of
// that functionality:
//
// - The basics: opening the pty pair, binding the slave end to the stdio
//   streams of the child process
//
// - Window size control: this only sets an initial size. The idea is
//   that this should be large enough that the subprocess doesn't try to
//   do manual line wrapping for typical outputs.
//
// TODO: Add more things here as they come up from testing.

use std::ffi::{c_int, CStr, CString};
use std::fs::File;
use std::io::{self, Read, Write};
use std::mem::zeroed;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::ptr::null_mut;
use std::slice;

/// A safe wrapper around [`libc::open`].
fn safe_open(path: &CStr, flags: c_int) -> io::Result<OwnedFd> {
    let fd = unsafe { libc::open(path.as_ptr(), flags) };
    if fd == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}

/// A safe wrapper around [`libc::posix_openpt`]. It always opens the pty in
/// read/write mode and does *not* make it the controlling terminal of this
/// process.
fn safe_openpt() -> io::Result<OwnedFd> {
    let flags = libc::O_RDWR | libc::O_NOCTTY;
    let fd = unsafe { libc::posix_openpt(flags) };
    if fd == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}

/// A safe wrapper around [`libc::grantpt`].
fn safe_grantpt<F: AsRawFd>(fd: &F) -> io::Result<()> {
    let fd = unsafe { libc::grantpt(fd.as_raw_fd()) };
    if fd == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// A safe wrapper around [`libc::unlockpt`].
fn safe_unlockpt<F: AsRawFd>(fd: &F) -> io::Result<()> {
    let fd = unsafe { libc::unlockpt(fd.as_raw_fd()) };
    if fd == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// A safe wrapper around [`libc::ptsname`].
fn safe_ptsname<F: AsRawFd>(fd: &F) -> io::Result<CString> {
    let shared_name = unsafe { libc::ptsname(fd.as_raw_fd()) };
    if shared_name == null_mut() {
        Err(io::Error::last_os_error())
    } else {
        // Per ptsname(3), the result lives in static storage and cannot be
        // freed. We have to copy this into a separate buffer to make an owned
        // type out of it.
        let name_slice = unsafe {
            let length = libc::strlen(shared_name);
            slice::from_raw_parts(shared_name as *mut u8, length)
        };
        CString::new(name_slice)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
    }
}

/// A safe wrapper around [`libc::dup2`].
fn safe_dup2<F: AsRawFd>(src: &F, dest: RawFd) -> io::Result<()> {
    let result = unsafe { libc::dup2(src.as_raw_fd(), dest) };
    // Technically this is a file descriptor, but don't return it since the
    // caller knows it's equal to dest. And if we returned it in an OwnedFd it
    // would be closed when the point is to keep this open for the subprocess.
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// A safe wrapper around [`libc::ioctl`] with `TIOCSWINSZ`.
fn safe_set_tty_size<F: AsRawFd>(tty: &F, winsize: &libc::winsize) -> io::Result<()> {
    let result = unsafe { libc::ioctl(tty.as_raw_fd(), libc::TIOCSWINSZ, winsize) };
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// A safe wrapper around [`libc::tcgetattr`].
fn safe_tcgetattr<F: AsRawFd>(tty: &F) -> io::Result<libc::termios> {
    let mut output: libc::termios = unsafe { zeroed() };
    let result = unsafe { libc::tcgetattr(tty.as_raw_fd(), &mut output) };
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(output)
    }
}

/// A safe wrapper around [`libc::tcsetattr`].
fn safe_tcsetattr<F: AsRawFd>(tty: &F, termios: &libc::termios) -> io::Result<()> {
    let result = unsafe { libc::tcsetattr(tty.as_raw_fd(), libc::TCSANOW, termios) };
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Wraps a closure and keeps rerunning it as long as it fails with EINTR.
fn retry_interrupted<T, F: FnMut() -> io::Result<T>>(mut func: F) -> io::Result<T> {
    loop {
        match func() {
            Ok(val) => return Ok(val),
            Err(err) if err.kind() != io::ErrorKind::Interrupted => return Err(err),
            _ => (),
        }
    }
}

const DEFAULT_PTY_ROWS: u16 = 64;
const DEFAULT_PTY_COLS: u16 = 256;

/// A pseudo-teriminal. Once created, the pty may be configured and then used to
/// spawn a child process.
pub struct Pty {
    master: OwnedFd,
    winsize: libc::winsize,
}

impl Pty {
    /// Opens a pseudo-terminal device configured for.
    pub fn open() -> io::Result<Self> {
        let winsize = libc::winsize {
            ws_row: DEFAULT_PTY_ROWS,
            ws_col: DEFAULT_PTY_COLS,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        safe_openpt()
            .and_then(|fd| {
                safe_grantpt(&fd)?;
                Ok(fd)
            })
            .and_then(|fd| {
                safe_unlockpt(&fd)?;
                Ok(fd)
            })
            .and_then(|fd| {
                Ok(Pty {
                    master: fd,
                    winsize,
                })
            })
    }

    /// Sets the number of columns that the pty exposes. This only takes effect
    /// when invoking [`spawn`].
    pub fn set_columns(&mut self, columns: u16) {
        self.winsize.ws_col = columns;
    }

    /// Sets the number of rows that the pty exposes. This only takes effect
    /// when invoking [`spawn`].
    pub fn set_rows(&mut self, rows: u16) {
        self.winsize.ws_row = rows;
    }

    /// Runs the child process under a pty. Returns both the child process
    /// handle as well as a wrapper around the pty's master file descriptor
    /// that supports read and write operations.
    pub fn spawn(self, mut command: Command) -> io::Result<(Child, PtyFile)> {
        safe_set_tty_size(&self.master, &self.winsize)?;

        // Default pty configuration on Linux 6.19:
        //
        //   iflag: 02400 (ICRNL | IXON)
        //   oflag: 05 (OPOST | ONLCR)
        //   cflag: 3600277 (...)
        //   lflag: 105073 (IEXTEN | ECHOKE | ECHOCTL | ECHO | ECHOE | ECHOK | ICANON | ISIG)
        //
        // ONCLR translates LF to CRLF. Having this doesn't break anything, but
        // it generates useless data that we have to copy.
        safe_tcgetattr(&self.master).and_then(|mut termios| {
            termios.c_oflag = termios.c_oflag & !libc::ONLCR;
            safe_tcsetattr(&self.master, &termios)
        })?;

        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        let slave_fname = safe_ptsname(&self.master)?;
        unsafe {
            command.pre_exec(move || {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }

                // Deliberately *don't* pass O_NOCTTY here, we want this to be
                // the controlling tty for the subprocess.
                let slave_fd = retry_interrupted(|| safe_open(&slave_fname, libc::O_RDWR))?;
                retry_interrupted(|| safe_dup2(&slave_fd, 0))?;
                retry_interrupted(|| safe_dup2(&slave_fd, 1))?;
                retry_interrupted(|| safe_dup2(&slave_fd, 2))
            });
        }
        let master = PtyFile(self.master.into());
        command.spawn().map(|child| (child, master))
    }
}

/// A wrapper around a [`File`] for the master of a [`Pty`]. This delegates to
/// [`File`] for all operations, the only difference is that pty-specific errors
/// are mapped to errors that would occur from pipes.
pub struct PtyFile(File);

impl PtyFile {
    /// Runs the IO operation and treats an [`libc::EIO`] error the same as an
    /// EOF. See https://stackoverflow.com/a/10306782 for more information.
    fn ignore_eio<F: FnMut() -> io::Result<usize>>(mut op: F) -> io::Result<usize> {
        op().or_else(|err| {
            if err.raw_os_error() == Some(libc::EIO) {
                Ok(0)
            } else {
                Err(err)
            }
        })
    }
}

impl Read for PtyFile {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        PtyFile::ignore_eio(|| self.0.read(buf))
    }
}

impl Write for PtyFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        PtyFile::ignore_eio(|| self.0.write(buf))
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl AsRawFd for PtyFile {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

#[cfg(test)]
mod test {
    use std::io::Read;
    use std::process::Command;

    use super::*;

    #[test]
    fn spawn_default_winsize() {
        let mut stty = Command::new("stty");
        stty.args(vec!["size"]);
        let pty = Pty::open().unwrap();
        let (mut subproc, mut pty_master) = pty.spawn(stty).unwrap();

        let mut output = String::new();
        pty_master.read_to_string(&mut output).unwrap();
        subproc.wait().unwrap();

        let expected = format!("{DEFAULT_PTY_ROWS} {DEFAULT_PTY_COLS}\n");
        assert_eq!(expected, output);
    }

    #[test]
    fn spawn_custom_winsize() {
        let mut stty = Command::new("stty");
        stty.args(vec!["size"]);

        let mut pty = Pty::open().unwrap();
        pty.set_rows(32);
        pty.set_columns(80);
        let (mut subproc, mut pty_master) = pty.spawn(stty).unwrap();

        let mut output = String::new();
        pty_master.read_to_string(&mut output).unwrap();
        subproc.wait().unwrap();

        assert_eq!("32 80\n", output);
    }

    #[test]
    fn read_and_write() {
        let mut head = Command::new("cat");

        let pty = Pty::open().unwrap();
        let (mut subproc, mut pty_master) = pty.spawn(head).unwrap();

        pty_master
            .write_all("hello\nworld\n\x04".as_bytes())
            .unwrap();
        // x04 is ^D, cat treats this as EOF

        let mut output = String::new();
        pty_master.read_to_string(&mut output).unwrap();
        subproc.wait().unwrap();

        assert_eq!("hello\nworld\nhello\nworld\n", output);
    }
}
