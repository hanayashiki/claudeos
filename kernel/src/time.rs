//! Wall clock and monotonic time.
//!
//! The machine's real-time clock is read once at boot. Elapsed time comes from
//! the CPU's free-running cycle counter, because a clock that only advances
//! 100 times a second cannot measure anything a program is likely to be
//! timing. What makes that counter a clock is the rate it runs at, and the
//! machine is asked for that: `counter_frequency` reads it out of a register
//! on one machine and measures it against the interval timer's own countdown
//! on the other.
//!
//! The wall clock is the monotonic clock plus a base, the time since the epoch
//! at which the monotonic clock read zero. Setting the wall clock replaces the
//! base and nothing else. Every wait in the kernel is counted in ticks or
//! against the monotonic clock, so a sleep, a timeout, an interval timer or a
//! retransmission runs the same length whatever the wall clock is set to while
//! it runs.

use crate::arch::{counter_frequency, cycle_counter, TICK_HZ};
use crate::sched::WaitQueue;
use core::fmt;
use core::sync::atomic::{AtomicI64, AtomicU64, Ordering};

const NS_PER_SECOND: i64 = 1_000_000_000;

/// Nanoseconds since the Unix epoch at the moment the monotonic clock read
/// zero.
///
/// It is one word and every change to it is one store, so a reader gets the
/// base from before a change or the base from after it, never the seconds of
/// one with the nanoseconds of the other.
static REALTIME_BASE_NS: AtomicI64 = AtomicI64::new(0);

/// How many times a program has set the wall clock. A sleep to a wall-clock
/// time reads this before it works out how many ticks away that time is, and a
/// different count afterwards says the answer was worked out against a base
/// that has since been replaced.
static REALTIME_CHANGES: AtomicU64 = AtomicU64::new(0);

/// Tasks sleeping until the wall clock reads a given time. Setting the clock
/// wakes all of them, and each works out its wait again against the new time.
pub static REALTIME_SET: WaitQueue = WaitQueue::new();

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

/// The civil date of a day counted from the Unix epoch, the inverse of
/// `days_from_civil`.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + if month <= 2 { 1 } else { 0 };
    (year, month, day)
}

pub fn init() {
    let now = crate::arch::read_wall_clock();
    let days = days_from_civil(now.year, now.month, now.day);
    let seconds = days * 86400 + now.hour * 3600 + now.minute * 60 + now.second;
    REALTIME_BASE_NS.store(seconds * NS_PER_SECOND, Ordering::Release);
}

/// Say that the machine cannot be earlier than `unix`, and move the clock
/// forward if it currently believes otherwise.
///
/// A board with no battery-backed clock starts at the epoch, which makes every
/// file it writes older than every file it was given. The dates on the initial
/// ram disk are the only evidence of the real time that arrives with it, so
/// they are used as a floor. This is a stand-in: it makes times within a boot
/// monotonic and plausible, and it is not the same thing as knowing the date.
///
/// Called once, while the ram disk is unpacked and before any program runs.
/// A program that sets the clock afterwards goes through `set_realtime`, which
/// takes any time, as Linux does.
pub fn set_floor(unix: i64) {
    let floor = unix.saturating_mul(NS_PER_SECOND);
    let _ = REALTIME_BASE_NS.fetch_update(Ordering::AcqRel, Ordering::Acquire, |base| {
        (base < floor).then_some(floor)
    });
}

/// Make the wall clock read `target_ns` nanoseconds since the epoch now, and
/// return what it read just before.
///
/// Only the base is replaced, in one store. The count of changes goes up after
/// the store and the sleepers are woken after that, so a sleeper that read the
/// count before the store either sees the new count before it parks or is on
/// the queue by the time it is woken.
pub fn set_realtime(target_ns: i64) -> i64 {
    let monotonic = monotonic_ns() as i64;
    let previous = REALTIME_BASE_NS.swap(target_ns - monotonic, Ordering::AcqRel);
    REALTIME_CHANGES.fetch_add(1, Ordering::AcqRel);
    REALTIME_SET.wake_all();
    previous.saturating_add(monotonic)
}

/// How many times the wall clock has been set since boot.
pub fn realtime_changes() -> u64 {
    REALTIME_CHANGES.load(Ordering::Acquire)
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

/// Nanoseconds since the Unix epoch.
pub fn realtime_ns() -> i64 {
    REALTIME_BASE_NS.load(Ordering::Acquire).saturating_add(monotonic_ns() as i64)
}

/// Seconds and nanoseconds since the Unix epoch.
pub fn realtime_parts() -> (i64, i64) {
    let ns = realtime_ns();
    (ns.div_euclid(NS_PER_SECOND), ns.rem_euclid(NS_PER_SECOND))
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

/// A wall-clock time in nanoseconds since the epoch, printed as UTC to the
/// second: `2026-09-15T01:23:45Z`.
pub struct Utc(pub i64);

impl fmt::Display for Utc {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let seconds = self.0.div_euclid(NS_PER_SECOND);
        let (year, month, day) = civil_from_days(seconds.div_euclid(86400));
        let of_day = seconds.rem_euclid(86400);
        write!(
            f,
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
            year,
            month,
            day,
            of_day / 3600,
            of_day / 60 % 60,
            of_day % 60
        )
    }
}

/// How far the wall clock was moved, in nanoseconds, printed as how far the
/// clock had been from the time it was set to: `1h43m behind` for a clock set
/// forward by that much, `ahead` for one set back.
pub struct Step(pub i64);

impl fmt::Display for Step {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let ns = self.0.unsigned_abs();
        let seconds = ns / NS_PER_SECOND as u64;
        let (days, hours, minutes) = (seconds / 86400, seconds / 3600 % 24, seconds / 60 % 60);
        if days > 0 {
            write!(f, "{}d{}h{}m", days, hours, minutes)?;
        } else if hours > 0 {
            write!(f, "{}h{}m", hours, minutes)?;
        } else if minutes > 0 {
            write!(f, "{}m{}s", minutes, seconds % 60)?;
        } else {
            write!(f, "{}.{:03}s", seconds, ns % NS_PER_SECOND as u64 / 1_000_000)?;
        }
        f.write_str(if self.0 >= 0 { " behind" } else { " ahead" })
    }
}
