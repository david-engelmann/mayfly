//! The shared monotonic clock.
//!
//! The one clock every stamp in a run is taken from, with
//! the rounding the report timings carry.

/// The one monotonic clock every stamp in a run is taken from, in
/// seconds. The runner, the init event stamps, and the interrupt
/// receipt stamps all read this clock, so stamps taken in
/// different processes of one run stay comparable.
pub fn now() -> f64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // The destination is a fully initialised timespec owned by
    // this call, and clock_gettime writes only through it.
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
    }
    ts.tv_sec as f64 + ts.tv_nsec as f64 / 1_000_000_000.0
}

/// Round to three decimals, the rounding the report timings carry.
pub fn round3(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}

/// Milliseconds between a past stamp on the shared clock and now,
/// rounded the way the Python harness rounded them.
pub fn ms_since(since: f64) -> f64 {
    round3((now() - since) * 1000.0)
}
