//! The two clocks the machine offers: the timestamp counter, which counts
//! fast and from an arbitrary zero, and the CMOS real-time clock, which knows
//! the date but only to the second.

use super::io::{inb, outb};

/// A reading of the CPU's free-running counter. Its unit is not a second;
/// `crate::time` measures it against the timer tick to find out what it is.
#[inline]
pub fn cycle_counter() -> u64 {
    let low: u32;
    let high: u32;
    unsafe {
        core::arch::asm!("rdtsc", out("eax") low, out("edx") high, options(nomem, nostack));
    }
    ((high as u64) << 32) | low as u64
}

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

/// A civil date and time, as the machine's battery-backed clock holds it.
#[derive(Debug, Clone, Copy, Default)]
pub struct WallClock {
    pub year: i64,
    pub month: i64,
    pub day: i64,
    pub hour: i64,
    pub minute: i64,
    pub second: i64,
}

/// Read the real-time clock. The answer is UTC and accurate to the second.
pub fn read_wall_clock() -> WallClock {
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

        WallClock {
            year: century_value * 100 + conv(year) as i64,
            month: conv(month) as i64,
            day: conv(day) as i64,
            hour: hour_value as i64,
            minute: conv(minute) as i64,
            second: conv(second) as i64,
        }
    }
}
