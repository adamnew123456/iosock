use std::{io, mem::zeroed, ptr::null_mut};

static mut CHILD_PID: i32 = 0;

unsafe extern "C" fn on_terminate_signal() {
    // Can't meaningfully return anything here *or* do stdio to log the
    // error.
    libc::kill(CHILD_PID, libc::SIGTERM);
}

fn safe_sigaddset(sigset: *mut libc::sigset_t, signum: libc::c_int) -> io::Result<()> {
    if unsafe { libc::sigaddset(sigset, signum) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn safe_sigaction(signum: libc::c_int, action: *const libc::sigaction) -> io::Result<()> {
    if unsafe { libc::sigaction(signum, action, null_mut()) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

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
