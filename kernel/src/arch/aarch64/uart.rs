//! The PL011 serial port, which is what this board calls its console.
//!
//! Reached through the direct map rather than by its bare physical address,
//! because the identity map goes away once the kernel is running on its own
//! tables and the console has to outlive it.

use super::PERIPHERAL_BASE;
use crate::mm::phys_to_virt;

const UART0: u64 = PERIPHERAL_BASE + 0x20_1000;

const DR: u64 = 0x00;
/// Flags: bit 4 is "nothing to read", bit 5 is "no room to write".
const FR: u64 = 0x18;
const IBRD: u64 = 0x24;
const FBRD: u64 = 0x28;
const LCRH: u64 = 0x2C;
const CR: u64 = 0x30;
const IMSC: u64 = 0x38;
const ICR: u64 = 0x44;

const RX_EMPTY: u32 = 1 << 4;
const TX_FULL: u32 = 1 << 5;

#[inline]
unsafe fn read(offset: u64) -> u32 {
    core::ptr::read_volatile(phys_to_virt(UART0 + offset) as *const u32)
}

#[inline]
unsafe fn write(offset: u64, value: u32) {
    core::ptr::write_volatile(phys_to_virt(UART0 + offset) as *mut u32, value);
}

pub fn init() {
    unsafe {
        // Stop the port before touching the line settings; changing them with
        // the transmitter running is not defined.
        write(CR, 0);
        write(ICR, 0x7FF);
        // 115200 baud from the 48 MHz reference clock the firmware leaves
        // running: 48000000 / (16 * 115200) = 26.0417.
        write(IBRD, 26);
        write(FBRD, 3);
        // Eight bits, no parity, one stop bit, FIFOs on.
        write(LCRH, (3 << 5) | (1 << 4));
        write(IMSC, 0);
        write(CR, (1 << 0) | (1 << 8) | (1 << 9)); // enable, transmit, receive
    }
}

pub fn write_byte(byte: u8) {
    unsafe {
        while read(FR) & TX_FULL != 0 {
            core::hint::spin_loop();
        }
        write(DR, byte as u32);
    }
}

pub fn read_byte() -> Option<u8> {
    unsafe {
        if read(FR) & RX_EMPTY != 0 {
            return None;
        }
        Some(read(DR) as u8)
    }
}

/// Raise an interrupt when a byte arrives, and when the receive FIFO has sat
/// part-full long enough that no more is coming.
pub fn enable_rx_interrupt() {
    unsafe { write(IMSC, (1 << 4) | (1 << 6)) };
}
