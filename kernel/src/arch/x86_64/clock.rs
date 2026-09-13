//! The two clocks the machine offers: the timestamp counter, which counts
//! fast and from an arbitrary zero, and the CMOS real-time clock, which knows
//! the date but only to the second.

use super::cpu::pit;
use super::io::{inb, outb};

/// A reading of the CPU's free-running counter. Its unit is not a second;
/// `counter_frequency` below works out how many of them make one.
#[inline]
pub fn cycle_counter() -> u64 {
    let low: u32;
    let high: u32;
    unsafe {
        core::arch::asm!("rdtsc", out("eax") low, out("edx") high, options(nomem, nostack));
    }
    ((high as u64) << 32) | low as u64
}

/// Timestamp counter ticks in a second, or zero if it could not be found out.
///
/// Nothing on this machine states the counter's rate the way `cntfrq_el0` does
/// on the other one, so it has to be measured against a clock whose rate is
/// known. The interval timer's is: it counts down from whatever it is given at
/// `pit::BASE_FREQUENCY`, one of the machine's fixed numbers. The tick it
/// raises is not that clock -- a tick delivered late is late by however long
/// interrupts were off, which over a window a few ticks wide is a few per cent
/// of the window -- so this drives a channel of the same chip directly and
/// watches its output pin instead, which nothing between the chip and here can
/// delay.
///
/// Costs the length of the window, 55 ms, and is meant to be called once. Runs
/// with interrupts masked: a handler that ran inside the window is time the
/// counter spent on something other than the countdown, and it would be
/// counted as though it were part of it.
pub fn counter_frequency() -> u64 {
    // The whole range of the counter, which is the longest window one pass can
    // measure: 65535 / 1193182 s.
    const COUNT: u16 = 0xFFFF;
    // A machine whose channel 2 is not there leaves the pin low for good, so
    // the wait needs an end; this one is twenty times the 860,763 turns the
    // window took when measured, and hundreds of times what it takes on a bus
    // where a port read costs the microsecond an ISA cycle does.
    const GIVE_UP: u64 = 20_000_000;

    crate::sync::without_interrupts(|_| unsafe {
        let saved = inb(SPEAKER_PORT);
        // Bit 0 gates channel 2 on; bit 1 is what would pass its output to the
        // speaker, and is left off so that measuring the clock does not make a
        // noise.
        outb(SPEAKER_PORT, (saved & !SPEAKER_OUTPUT) | CHANNEL2_GATE);
        // Channel 2, low byte then high, mode 0: count the value down once and
        // raise the output pin at zero. The write drops the pin, so what is
        // waited for below cannot be a leftover high from before.
        outb(PIT_COMMAND, 0xB0);
        outb(CHANNEL2, (COUNT & 0xFF) as u8);
        outb(CHANNEL2, (COUNT >> 8) as u8);
        let begin = cycle_counter();
        let mut spins = 0u64;
        while inb(SPEAKER_PORT) & CHANNEL2_OUTPUT == 0 {
            spins += 1;
            if spins > GIVE_UP {
                outb(SPEAKER_PORT, saved);
                return 0;
            }
        }
        let elapsed = cycle_counter() - begin;
        outb(SPEAKER_PORT, saved);
        elapsed * pit::BASE_FREQUENCY as u64 / COUNT as u64
    })
}

/// The port that gates the interval timer's third channel and reports its
/// output pin.
const SPEAKER_PORT: u16 = 0x61;
const CHANNEL2_GATE: u8 = 1 << 0;
const SPEAKER_OUTPUT: u8 = 1 << 1;
const CHANNEL2_OUTPUT: u8 = 1 << 5;
const CHANNEL2: u16 = 0x42;
const PIT_COMMAND: u16 = 0x43;

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
