use std::io;
use std::mem::zeroed;
use std::ptr::null_mut;

/// Stores the process ID of the child we spawned. This is in static state so
/// the signal handler can use it to kill the process later.
static mut CHILD_PID: i32 = 0;

/// The C-level signal handler, actually kills the child process in repsonse to
/// the signal.
unsafe extern "C" fn on_terminate_signal() {
    // Can't meaningfully return anything here *or* do stdio to log the
    // error.
    libc::kill(CHILD_PID, libc::SIGTERM);
}

/// Safe wrapper around [`libc::sigaddset`].
fn safe_sigaddset(sigset: *mut libc::sigset_t, signum: libc::c_int) -> io::Result<()> {
    if unsafe { libc::sigaddset(sigset, signum) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Safe wrapper around [`libc::sigaction`].
fn safe_sigaction(signum: libc::c_int, action: *const libc::sigaction) -> io::Result<()> {
    if unsafe { libc::sigaction(signum, action, null_mut()) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Registers a signal handler that terminates the given process when *this*
/// process receives a non-KILL termination signal.
pub fn kill_child_on_signal(child_pid: i32) -> io::Result<()> {
    unsafe { CHILD_PID = child_pid };

    let mut action: libc::sigaction = unsafe { zeroed() };
    action.sa_sigaction = on_terminate_signal as usize;
    safe_sigaddset(&mut action.sa_mask, libc::SIGHUP)?;
    safe_sigaddset(&mut action.sa_mask, libc::SIGTERM)?;
    safe_sigaddset(&mut action.sa_mask, libc::SIGINT)?;
    safe_sigaction(libc::SIGHUP, &action)?;
    safe_sigaction(libc::SIGTERM, &action)?;
    safe_sigaction(libc::SIGINT, &action)
}
