//! Waiting, for the drivers of the two SD host controllers.
//!
//! Every wait in bring-up is one a reference driver asks for by name, or a
//! bound on how long a register may take to change. Short ones spin on the
//! architected counter, which reports its own rate; long ones give the
//! processor back to the scheduler.
//!
//! All of this runs in a kernel task or a system call with interrupts on: the
//! WiFi driver's own task, the storage driver's bring-up task, and whichever
//! task is reading or writing /data. A spin is preempted by the timer like any
//! other work and a sleep is an ordinary sleep. Neither is called from the
//! boot context or with a spinlock held.

use crate::arch;

/// Microseconds on the architected counter. Only differences mean anything.
pub fn now_us() -> u64 {
    let frequency = arch::counter_frequency();
    if frequency == 0 {
        return 0;
    }
    (arch::cycle_counter() as u128 * 1_000_000 / frequency as u128) as u64
}

/// Spin for `micros` microseconds.
pub fn spin_us(micros: u64) {
    let start = now_us();
    while now_us().wrapping_sub(start) < micros {
        core::hint::spin_loop();
    }
}

/// Sleep for at least `millis` milliseconds, rounded up to the timer tick.
pub fn sleep_ms(millis: u64) {
    let hz = arch::TICK_HZ as u64;
    crate::sched::sleep_ticks((millis * hz).div_ceil(1000).max(1));
}

/// A point in time a wait gives up at.
#[derive(Clone, Copy)]
pub struct Deadline {
    at: u64,
}

impl Deadline {
    pub fn after_us(micros: u64) -> Deadline {
        Deadline { at: now_us().saturating_add(micros) }
    }

    pub fn after_ms(millis: u64) -> Deadline {
        Deadline::after_us(millis.saturating_mul(1000))
    }

    pub fn expired(&self) -> bool {
        now_us() >= self.at
    }
}
