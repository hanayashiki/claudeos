//! 16550 UART on COM1, driven through port I/O. This is the machine's debug
//! console: the byte sink and byte source behind `crate::serial`.
//!
//! Every entry point here is called with `crate::serial::SERIAL` held, which
//! is what serialises access to the port.

use super::io::{inb, outb};
use core::sync::atomic::{AtomicBool, Ordering};

const COM1: u16 = 0x3F8;

static INITIALIZED: AtomicBool = AtomicBool::new(false);

/// Set the line up: 115200 baud, 8N1, no receive FIFO.
pub fn init() {
    unsafe {
        outb(COM1 + 1, 0x00); // disable interrupts
        outb(COM1 + 3, 0x80); // enable DLAB
        outb(COM1 + 0, 0x01); // divisor 1 => 115200 baud
        outb(COM1 + 1, 0x00);
        outb(COM1 + 3, 0x03); // 8 bits, no parity, one stop bit

        // The receive FIFOs are deliberately left disabled. Enabling them
        // makes the emulated UART discard anything already received, which
        // eats the first keystrokes when input is piped into a fresh boot.
        // With FIFOs off the sender is held back until each byte is read.
        outb(COM1 + 4, 0x0B); // DTR, RTS, OUT2
    }
    INITIALIZED.store(true, Ordering::Relaxed);
}

fn tx_empty() -> bool {
    unsafe { inb(COM1 + 5) & 0x20 != 0 }
}

fn raw_byte(byte: u8) {
    let mut spins = 0u32;
    while !tx_empty() {
        spins += 1;
        if spins > 1_000_000 {
            break;
        }
        core::hint::spin_loop();
    }
    unsafe { outb(COM1, byte) };
}

/// Write one byte, turning a newline into the carriage return and line feed a
/// terminal expects. Blocks until the transmitter takes it.
pub fn write_byte(byte: u8) {
    if !INITIALIZED.load(Ordering::Relaxed) {
        init();
    }
    if byte == b'\n' {
        raw_byte(b'\r');
    }
    raw_byte(byte);
}

fn data_ready() -> bool {
    unsafe { inb(COM1 + 5) & 1 != 0 }
}

/// Take one byte if the line has one waiting. Never blocks.
pub fn read_byte() -> Option<u8> {
    if data_ready() {
        Some(unsafe { inb(COM1) })
    } else {
        None
    }
}

/// Have the UART raise its interrupt when a byte arrives.
pub fn enable_rx_interrupt() {
    unsafe { outb(COM1 + 1, 0x01) };
}
