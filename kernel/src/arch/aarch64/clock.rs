//! The clocks this board offers.
//!
//! There is one: the architected counter, which every core reads through
//! `cntvct_el0` and which runs at a rate `cntfrq_el0` reports. There is no
//! battery-backed clock of any kind, so the date has to come from somewhere
//! else; until it does, the machine believes it booted at the epoch.

use core::arch::asm;

/// A reading of the architected counter. Its unit is not a second;
/// `counter_frequency` below says how many of them make one.
#[inline]
pub fn cycle_counter() -> u64 {
    let value: u64;
    unsafe { asm!("mrs {}, cntvct_el0", out(reg) value, options(nomem, nostack)) };
    value
}

/// Counter ticks in a second, as the hardware reports it.
#[inline]
pub fn counter_frequency() -> u64 {
    let value: u64;
    unsafe { asm!("mrs {}, cntfrq_el0", out(reg) value, options(nomem, nostack)) };
    value
}

/// A civil date and time.
#[derive(Debug, Clone, Copy, Default)]
pub struct WallClock {
    pub year: i64,
    pub month: i64,
    pub day: i64,
    pub hour: i64,
    pub minute: i64,
    pub second: i64,
}

/// The date, as far as this board knows it. Nothing on a Pi remembers the time
/// across a power cycle, so this is the epoch rather than a guess: a program
/// that reads it sees a clock that has not been set, which is the truth.
pub fn read_wall_clock() -> WallClock {
    WallClock { year: 1970, month: 1, day: 1, hour: 0, minute: 0, second: 0 }
}
