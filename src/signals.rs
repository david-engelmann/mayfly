//! The signal guard.
//!
//! Records SIGTERM and SIGINT instead of dying on them mid
//! run, stamping the first receipt on the shared clock.

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};

use crate::clock::now;

/// Record SIGTERM and SIGINT instead of dying on them mid run.
///
/// The handler only records the first signal number and stamps
/// the moment it was received on the shared monotonic clock, so
/// the outcome logic can compare the receipt of an interrupt
/// against the task done stamp rather than against when the
/// runner happened to drain the event. Handlers exist only on
/// the main thread, so a run driven from a worker thread gets
/// none and keeps the default behaviour.
pub struct SignalGuard {
    installed: bool,
    prev_int: Option<libc::sigaction>,
    prev_term: Option<libc::sigaction>,
}

static SIGNAL_PUBLISHED: AtomicBool = AtomicBool::new(false);
static SIGNAL_FIRST: AtomicI32 = AtomicI32::new(0);
static SIGNAL_FIRST_TIME: AtomicU64 = AtomicU64::new(0);
static SIGNAL_CLAIMED: AtomicBool = AtomicBool::new(false);

extern "C" fn record_signal(signum: i32) {
    // The snapshot is claimed by a swap on a dedicated flag, so
    // exactly one handler writes the signum and its stamp, and
    // only that handler publishes them, setting the published
    // flag after both stores land. A reader that acquires the
    // flag therefore always sees the first signal and its stamp
    // whole, written by the one handler that claimed them, and
    // never a publication standing over an unwritten pair. A
    // handler that loses the claim publishes nothing, because
    // a publication a losing handler could move would let a
    // reader observe it before the claimant's stores land.
    // Two signals handled on different threads at the same
    // moment cannot interleave their stores either, because
    // only the handler whose swap takes the claim flag from
    // false to true writes the pair at all.
    if !SIGNAL_CLAIMED.swap(true, Ordering::SeqCst) {
        SIGNAL_FIRST.store(signum, Ordering::Release);
        SIGNAL_FIRST_TIME.store(now().to_bits(), Ordering::Release);
        SIGNAL_PUBLISHED.store(true, Ordering::Release);
    }
}

fn is_main_thread() -> bool {
    #[cfg(target_os = "linux")]
    {
        // gettid and getpid are plain queries with no pointer or
        // lifetime contract, and their results are compared as
        // integers only.
        unsafe { libc::syscall(libc::SYS_gettid) == libc::getpid() as i64 }
    }
    #[cfg(target_os = "macos")]
    {
        unsafe { libc::pthread_main_np() == 1 }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        true
    }
}

impl SignalGuard {
    pub fn install() -> SignalGuard {
        let mut guard = SignalGuard {
            installed: false,
            prev_int: None,
            prev_term: None,
        };
        if !is_main_thread() {
            return guard;
        }
        SIGNAL_CLAIMED.store(false, Ordering::SeqCst);
        SIGNAL_PUBLISHED.store(false, Ordering::SeqCst);
        SIGNAL_FIRST.store(0, Ordering::SeqCst);
        SIGNAL_FIRST_TIME.store(0, Ordering::SeqCst);
        // The action struct is fully initialised before the
        // call, the handler is a plain extern function that
        // touches only the atomics above, and the previous
        // actions are written into owned storage the guard
        // keeps for its restore.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = record_signal as *const () as usize;
            libc::sigemptyset(&mut action.sa_mask);
            action.sa_flags = 0;
            let mut prev_int: libc::sigaction = std::mem::zeroed();
            if libc::sigaction(libc::SIGINT, &action, &mut prev_int) == 0 {
                guard.prev_int = Some(prev_int);
            }
            let mut prev_term: libc::sigaction = std::mem::zeroed();
            if libc::sigaction(libc::SIGTERM, &action, &mut prev_term) == 0 {
                guard.prev_term = Some(prev_term);
            }
        }
        guard.installed = true;
        guard
    }

    pub fn interrupted(&self) -> bool {
        self.installed && SIGNAL_PUBLISHED.load(Ordering::Acquire)
    }

    pub fn first(&self) -> Option<i32> {
        if self.interrupted() {
            Some(SIGNAL_FIRST.load(Ordering::Acquire))
        } else {
            None
        }
    }

    pub fn first_time(&self) -> Option<f64> {
        if self.interrupted() {
            Some(f64::from_bits(SIGNAL_FIRST_TIME.load(Ordering::Acquire)))
        } else {
            None
        }
    }
}

impl Drop for SignalGuard {
    fn drop(&mut self) {
        if !self.installed {
            return;
        }
        // The restored actions are the exact structs sigaction
        // handed back at install time, kept unmodified in the
        // guard, so restoring them returns the thread to the
        // handlers it had before the guard.
        unsafe {
            if let Some(prev) = self.prev_int.take() {
                libc::sigaction(libc::SIGINT, &prev, std::ptr::null_mut());
            }
            if let Some(prev) = self.prev_term.take() {
                libc::sigaction(libc::SIGTERM, &prev, std::ptr::null_mut());
            }
        }
    }
}
