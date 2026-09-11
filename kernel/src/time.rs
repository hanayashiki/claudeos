//! Wall clock and monotonic time.
//!
//! The CMOS real-time clock is read once at boot; from there on time advances
//! with the timer tick.

use crate::cpu::pit::TICK_HZ;
use crate::io::{inb, outb};
use crate::sync::Spinlock;

static BOOT_UNIX_TIME: Spinlock<i64> = Spinlock::new(0);

const CMOS_ADDRESS: u16 = 0x70;
const CMOS_DATA: u16 = 0x71;

unsafe fn cmos_read(register: u8) -> u8 {
    outb(CMOS_ADDRESS, register);
    inb(CMOS_DATA)
}

unsafe fn update_in_progress() -> bool {
    cmos_read(0x0A) & 0x80 != 0
}

fn bcd_to_binary(value: u8) -> u8 {
    (value & 0x0F) + ((value >> 4) * 10)
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
    unsafe {
        let mut spins = 0;
        while update_in_progress() && spins < 1_000_000 {
            spins += 1;
        }
        let second = cmos_read(0x00);
        let minute = cmos_read(0x02);
        let hour = cmos_read(0x04);
        let day = cmos_read(0x07);
        let month = cmos_read(0x08);
        let year = cmos_read(0x09);
        let century = cmos_read(0x32);
        let status_b = cmos_read(0x0B);

        let binary = status_b & 0x04 != 0;
        let conv = |v: u8| if binary { v } else { bcd_to_binary(v) };

        let mut hour_value = if binary { hour & 0x7F } else { bcd_to_binary(hour & 0x7F) };
        // In 12-hour mode the high bit of the hour register means PM.
        if status_b & 0x02 == 0 && hour & 0x80 != 0 {
            hour_value = (hour_value % 12) + 12;
        }

        let century_value = if century == 0 { 20 } else { conv(century) as i64 };
        let full_year = century_value * 100 + conv(year) as i64;

        let days = days_from_civil(full_year, conv(month) as i64, conv(day) as i64);
        let unix = days * 86400
            + hour_value as i64 * 3600
            + conv(minute) as i64 * 60
            + conv(second) as i64;
        *BOOT_UNIX_TIME.lock() = unix;
    }
}

/// Nanoseconds since boot.
pub fn monotonic_ns() -> u64 {
    crate::trap::ticks() * (1_000_000_000 / TICK_HZ as u64)
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

/// Convert nanoseconds to a whole number of timer ticks, rounding up.
pub fn ns_to_ticks(ns: u64) -> u64 {
    let per_tick = 1_000_000_000u64 / TICK_HZ as u64;
    (ns + per_tick - 1) / per_tick
}
