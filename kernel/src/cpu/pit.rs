//! 8254 programmable interval timer, used as the scheduler tick.

use crate::io::outb;

const CHANNEL0: u16 = 0x40;
const COMMAND: u16 = 0x43;
const BASE_FREQUENCY: u32 = 1_193_182;

pub const TICK_HZ: u32 = 100;

pub fn init(hz: u32) {
    let divisor = (BASE_FREQUENCY / hz) as u16;
    unsafe {
        // Channel 0, lobyte/hibyte, mode 3 (square wave), binary.
        outb(COMMAND, 0x36);
        outb(CHANNEL0, (divisor & 0xFF) as u8);
        outb(CHANNEL0, (divisor >> 8) as u8);
    }
}
