//! Real-time scheduling helpers.
//!
//! The input thread is the whole latency budget: a HID interrupt arrives every
//! millisecond and has to be parsed and forwarded before the next one. Running
//! it under `SCHED_FIFO` keeps a busy desktop from adding scheduler jitter on
//! top of the 1 ms USB poll.

use anyhow::{Context, Result};

/// Raise the calling thread to `SCHED_FIFO` at `priority`.
///
/// Requires `rtprio` headroom in `/etc/security/limits.d` (the `audio` group
/// gets it on most audio-oriented distributions). Failure is not fatal: the
/// caller logs it and keeps running under `SCHED_OTHER`.
pub fn set_realtime(priority: i32) -> Result<()> {
    let param = libc::sched_param {
        sched_priority: priority,
    };
    // SAFETY: `param` is a valid, fully initialised sched_param and 0 means
    // "the calling thread".
    let rc = unsafe { libc::sched_setscheduler(0, libc::SCHED_FIFO, &param) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error()).context("sched_setscheduler(SCHED_FIFO)");
    }
    Ok(())
}

/// Best-effort `SCHED_FIFO`; logs and continues when the rlimit forbids it.
pub fn try_realtime(priority: i32, what: &str) {
    match set_realtime(priority) {
        Ok(()) => eprintln!("[rt] {what} running SCHED_FIFO prio {priority}"),
        Err(e) => eprintln!(
            "[rt] {what} staying on SCHED_OTHER ({e}); \
             add your user to the 'audio' group for lower jitter"
        ),
    }
}

/// Lock all current and future pages to prevent paging in the input path.
pub fn lock_memory() {
    // SAFETY: mlockall with valid flags; failure is reported via errno only.
    let rc = unsafe { libc::mlockall(libc::MCL_CURRENT | libc::MCL_FUTURE) };
    if rc != 0 {
        eprintln!(
            "[rt] mlockall failed ({}); page faults may add jitter",
            std::io::Error::last_os_error()
        );
    }
}
