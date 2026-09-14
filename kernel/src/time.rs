//! Wall clock and monotonic time.
//!
//! The machine's real-time clock is read once at boot. Elapsed time comes from
//! the CPU's free-running cycle counter, because a clock that only advances
//! 100 times a second cannot measure anything a program is likely to be
//! timing. What makes that counter a clock is the rate it runs at, and the
//! machine is asked for that: `counter_frequency` reads it out of a register
//! on one machine and measures it against the interval timer's own countdown
//! on the other.

use crate::arch::{counter_frequency, cycle_counter, TICK_HZ};
use crate::sync::Spinlock;
use core::sync::atomic::{AtomicU64, Ordering};

static BOOT_UNIX_TIME: Spinlock<i64> = Spinlock::new(0);

/// Cycle counter reading at the end of calibration, and how many of its units
/// make a second. Zero means calibration has not run, and time falls back to
/// the tick count.
static CYCLES_BASE: AtomicU64 = AtomicU64::new(0);
static CYCLES_PER_SECOND: AtomicU64 = AtomicU64::new(0);

/// Find out what a unit of the cycle counter is worth, and start the clock.
///
/// Called once interrupts are on and the tick is running, because the fallback
/// below needs the tick.
pub fn calibrate() {
    let per_second = counter_frequency();
    if per_second != 0 {
        adopt(per_second);
        return;
    }
    // Nothing on this machine would say, so the tick is what is left to
    // measure against. It is a poor reference: a tick is delivered as late as
    // interrupts happen to have been off for, and that lateness lands whole
    // inside a window this short. The timer that raises it free-runs, so a
    // late tick shortens the next one and only the first and last tick's
    // lateness survives the run -- the error is their difference spread over
    // the window, which is why the window is fifty ticks and not five. An
    // emulated machine measured 2.1 per cent off over five ticks and 0.06 over
    // fifty, at the cost of half a second of boot.
    const TICKS: u64 = 50;
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
    adopt(elapsed * TICK_HZ as u64 / elapsed_ticks);
}

/// Take `per_second` as the counter's rate and place the clock's zero.
///
/// The counter's own zero is whenever the machine started, which is earlier
/// than anything the kernel saw, so the reading now is taken as the time the
/// tick count says has passed.
fn adopt(per_second: u64) {
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

/// Say that the machine cannot be earlier than `unix`, and move the clock
/// forward if it currently believes otherwise.
///
/// A board with no battery-backed clock starts at the epoch, which makes every
/// file it writes older than every file it was given. The dates on the initial
/// ram disk are the only evidence of the real time that arrives with it, so
/// they are used as a floor. This is a stand-in: it makes times within a boot
/// monotonic and plausible, and it is not the same thing as knowing the date.
pub fn set_floor(unix: i64) {
    let mut boot = BOOT_UNIX_TIME.lock();
    if *boot < unix {
        *boot = unix;
    }
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

/// Cycle counter units in a second: the rate calibration settled on once it
/// has run, and before that whatever the machine says when asked, which is
/// zero if it cannot say.
///
/// For a wait that cannot use the tick, such as the one after a panic, which
/// has interrupts masked and can come before calibration has run.
pub fn counter_rate() -> u64 {
    let calibrated = CYCLES_PER_SECOND.load(Ordering::Acquire);
    if calibrated != 0 {
        calibrated
    } else {
        counter_frequency()
    }
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
