//! Ctrl-C during an A/B run. The first Ctrl-C (or SIGTERM, SIGHUP) only sets a
//! flag: the session and check running in the foreground receive the same
//! signal from the terminal and end, the runner sees the flag, removes its
//! copies and restores the daemon's settings. A second one ends the process
//! at once.

use std::sync::atomic::{AtomicBool, Ordering};

static STOP: AtomicBool = AtomicBool::new(false);

/// True once a stop was asked for.
pub fn requested() -> bool {
    STOP.load(Ordering::SeqCst)
}

/// Catch Ctrl-C, SIGTERM and SIGHUP for the rest of the process. Does nothing
/// on platforms without POSIX signals.
pub fn install() {
    #[cfg(unix)]
    {
        let handler = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
        for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            // SAFETY: the handler only stores to an atomic and calls `signal`,
            // both async-signal-safe.
            unsafe {
                libc::signal(sig, handler);
            }
        }
    }
}

#[cfg(unix)]
extern "C" fn on_signal(sig: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
    // The next one of the same signal gets the default action: it ends the process.
    // SAFETY: `signal` is async-signal-safe.
    unsafe {
        libc::signal(sig, libc::SIG_DFL);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn a_signal_sets_the_flag_and_the_next_one_gets_the_default_action() {
        install();
        assert!(!requested());
        // SAFETY: raising a signal whose handler only stores to an atomic.
        unsafe {
            libc::raise(libc::SIGHUP);
        }
        assert!(requested());
        // The handler gave that signal its default action back: a second one ends the process.
        // SAFETY: reading and restoring signal dispositions of this test process.
        unsafe {
            let now = libc::signal(libc::SIGHUP, libc::SIG_DFL);
            assert_eq!(now, libc::SIG_DFL);
            // Ctrl-C ends the test process again as usual.
            libc::signal(libc::SIGINT, libc::SIG_DFL);
            libc::signal(libc::SIGTERM, libc::SIG_DFL);
        }
    }
}
