use std::io;
use std::mem::zeroed;
use std::ptr::null_mut;

#[cfg(test)]
use std::time::{Duration, Instant};

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

/// The C-level signal handler for handling timers. This is only used by djinn,
/// to ensure that it does not run too long if the test triggers an infinite
/// loop or deadlock within the main iosock program.
unsafe extern "C" fn on_exit_alarm() {
    libc::_exit(255);
}

/// Safe wrapper around [`libc::sigaddset`].
pub(crate) fn safe_sigaddset(sigset: *mut libc::sigset_t, signum: libc::c_int) -> io::Result<()> {
    if unsafe { libc::sigaddset(sigset, signum) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Safe wrapper around [`libc::sigprocmask`]. Blocks the signals in the
/// provided set until they are retrieved via [`safe_sigtimedwait`] or similar.
#[cfg(test)]
pub(crate) fn block_signals(sigset: *const libc::sigset_t) -> io::Result<()> {
    if unsafe { libc::sigprocmask(libc::SIG_BLOCK, sigset, null_mut()) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Safe wrapper around [`libc::sigtimedwait`]. Returns true if one of the
/// requested signals occurred within the timeout period and false otherwise.
#[cfg(test)]
pub(crate) fn safe_sigtimedwait(
    sigset: *const libc::sigset_t,
    timeout: Duration,
) -> io::Result<bool> {
    let mut timeout_spec: libc::timespec = unsafe { zeroed() };
    let start_time = Instant::now();
    let end_time = start_time + timeout;
    let mut now = start_time;

    loop {
        let to_wait = end_time - now;
        timeout_spec.tv_sec = to_wait.as_secs() as i64;
        timeout_spec.tv_nsec = (to_wait.as_nanos() % 1000000000) as i64;

        let caught_signal = unsafe { libc::sigtimedwait(sigset, null_mut(), &timeout_spec) };
        if caught_signal != -1 {
            return Ok(true);
        }

        let errno = io::Error::last_os_error();
        match errno.raw_os_error() {
            Some(libc::EAGAIN) => return Ok(false),
            // Interrupted by some other kind of signal, keep waiting
            Some(libc::EINTR) => (),
            _ => return Err(errno),
        }

        // Check the elapsed time after the first call, no need to do this on
        // the very first iteration since start_time == end_time
        now = Instant::now();
        if now >= end_time {
            return Ok(false);
        }
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
    action.sa_sigaction = on_terminate_signal as *const () as usize;
    safe_sigaddset(&mut action.sa_mask, libc::SIGHUP)?;
    safe_sigaddset(&mut action.sa_mask, libc::SIGTERM)?;
    safe_sigaddset(&mut action.sa_mask, libc::SIGINT)?;
    safe_sigaction(libc::SIGHUP, &action)?;
    safe_sigaction(libc::SIGTERM, &action)?;
    safe_sigaction(libc::SIGINT, &action)
}

/// Registers a signal handler that terminates this process after the provided
/// number of seconds has passed.
pub fn exit_after_timeout(seconds: u32) -> io::Result<()> {
    let mut action: libc::sigaction = unsafe { zeroed() };
    action.sa_sigaction = on_exit_alarm as *const () as usize;
    safe_sigaddset(&mut action.sa_mask, libc::SIGALRM)?;
    safe_sigaction(libc::SIGALRM, &action)?;
    unsafe { libc::alarm(seconds) };
    Ok(())
}
