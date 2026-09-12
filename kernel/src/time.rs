//! Wall clock and monotonic time.
//!
//! The machine's real-time clock is read once at boot. Elapsed time comes from
//! the CPU's free-running cycle counter, calibrated against the timer tick,
//! because a clock that only advances 100 times a second cannot measure
//! anything a program is likely to be timing.

use crate::arch::{cycle_counter, TICK_HZ};
use crate::sync::Spinlock;
use core::sync::atomic::{AtomicU64, Ordering};

static BOOT_UNIX_TIME: Spinlock<i64> = Spinlock::new(0);

/// Cycle counter reading at the end of calibration, and how many of its units
/// make a second. Zero means calibration has not run, and time falls back to
/// the tick count.
static CYCLES_BASE: AtomicU64 = AtomicU64::new(0);
static CYCLES_PER_SECOND: AtomicU64 = AtomicU64::new(0);

/// Measure the cycle counter against the timer tick.
///
/// Called once interrupts are on and the tick is running. Ticks are the only
/// other clock, so the calibration is no better than a tick: it waits for a
/// tick edge, counts over several ticks, and divides.
pub fn calibrate() {
    const TICKS: u64 = 5;
    let start_tick = crate::trap::ticks();
    while crate::trap::ticks() == start_tick {
        core::hint::spin_loop();
    }
    let begin_tick = crate::trap::ticks();
    let begin = cycle_counter();
    while crate::trap::ticks() < begin_tick + TICKS {
        core::hint::spin_loop();
    }
    let elapsed_ticks = crate::trap::ticks() - begin_tick;
    let elapsed = cycle_counter() - begin;
    if elapsed_ticks == 0 || elapsed == 0 {
        return;
    }
    let per_second = elapsed * TICK_HZ as u64 / elapsed_ticks;
    // The counter's zero is whenever the machine started; take the reading at
    // the end of calibration as the base and add the time already elapsed.
    CYCLES_PER_SECOND.store(per_second, Ordering::Release);
    let elapsed_ns = crate::trap::ticks() * (1_000_000_000 / TICK_HZ as u64);
    let base = cycle_counter().saturating_sub(elapsed_ns * per_second / 1_000_000_000);
    CYCLES_BASE.store(base, Ordering::Release);
}

/// Days since the Unix epoch for a civil date (Howard Hinnant's algorithm).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

pub fn init() {
    let now = crate::arch::read_wall_clock();
    let days = days_from_civil(now.year, now.month, now.day);
    *BOOT_UNIX_TIME.lock() =
        days * 86400 + now.hour * 3600 + now.minute * 60 + now.second;
}

/// Nanoseconds since boot.
pub fn monotonic_ns() -> u64 {
    let per_second = CYCLES_PER_SECOND.load(Ordering::Acquire);
    if per_second == 0 {
        return crate::trap::ticks() * (1_000_000_000 / TICK_HZ as u64);
    }
    let elapsed = cycle_counter().saturating_sub(CYCLES_BASE.load(Ordering::Acquire));
    (elapsed as u128 * 1_000_000_000u128 / per_second as u128) as u64
}

pub fn monotonic_parts() -> (i64, i64) {
    let ns = monotonic_ns();
    ((ns / 1_000_000_000) as i64, (ns % 1_000_000_000) as i64)
}

/// Seconds and nanoseconds since the Unix epoch.
pub fn realtime_parts() -> (i64, i64) {
    let boot = *BOOT_UNIX_TIME.lock();
    let ns = monotonic_ns();
    (boot + (ns / 1_000_000_000) as i64, (ns % 1_000_000_000) as i64)
}

pub fn unix_time() -> i64 {
    realtime_parts().0
}

pub fn ticks_to_ns(ticks: u64) -> u64 {
    ticks * (1_000_000_000 / TICK_HZ as u64)
}

/// Convert nanoseconds to a whole number of timer ticks, rounding up.
pub fn ns_to_ticks(ns: u64) -> u64 {
    let per_tick = 1_000_000_000u64 / TICK_HZ as u64;
    (ns + per_tick - 1) / per_tick
}
