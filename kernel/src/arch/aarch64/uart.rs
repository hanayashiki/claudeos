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

/// The pin controller.
const GPIO: u64 = PERIPHERAL_BASE + 0x20_0000;
/// What GPIO 10 through 19 are connected to, three bits each.
const GPFSEL1: u64 = 0x04;
/// What holds GPIO 0 through 15 when nothing else is driving them, two bits
/// each. This register is particular to this chip; earlier Pis used a clocked
/// sequence through a different one, and that sequence does nothing here.
const GPIO_PULL0: u64 = 0xE4;

/// Alternate function zero, which on GPIO 14 and 15 is this port's transmit
/// and receive.
const ALT0: u32 = 0b100;
const NO_PULL: u32 = 0b00;
const PULL_UP: u32 = 0b01;

#[inline]
unsafe fn read(offset: u64) -> u32 {
    core::ptr::read_volatile(phys_to_virt(UART0 + offset) as *const u32)
}

#[inline]
unsafe fn write(offset: u64, value: u32) {
    core::ptr::write_volatile(phys_to_virt(UART0 + offset) as *mut u32, value);
}

/// Connect the two pins a serial cable clips onto to this port.
///
/// On this board this port goes to the Bluetooth radio by default, and the
/// header pins carry the cut-down UART instead. The firmware only moves them
/// if it was told to apply an overlay that says so, and nothing running bare
/// metal loads overlays, so the kernel has to ask for them itself. Without
/// this the driver below writes correctly formed bytes into a port whose pins
/// go to the radio, and the cable shows nothing.
///
/// Emulation never shows the difference: it connects this port to the terminal
/// whatever the pins are set to.
fn route_pins() {
    unsafe {
        // Three bits per pin and ten pins to a register, so 14 and 15 are the
        // fifth and sixth fields of this one. The rest of the register belongs
        // to pins that are not ours to change.
        let select = phys_to_virt(GPIO + GPFSEL1) as *mut u32;
        let mut function = core::ptr::read_volatile(select);
        function &= !((0b111u32 << 12) | (0b111u32 << 15));
        function |= (ALT0 << 12) | (ALT0 << 15);
        core::ptr::write_volatile(select, function);

        // Two bits per pin and sixteen pins to a register. Nothing holds the
        // transmit line, because this end drives it. The receive line is held
        // high so that an unplugged cable reads as an idle line rather than as
        // an endless stream of break conditions.
        let pull = phys_to_virt(GPIO + GPIO_PULL0) as *mut u32;
        let mut held = core::ptr::read_volatile(pull);
        held &= !((0b11u32 << 28) | (0b11u32 << 30));
        held |= (NO_PULL << 28) | (PULL_UP << 30);
        core::ptr::write_volatile(pull, held);
    }
}

pub fn init() {
    route_pins();
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
