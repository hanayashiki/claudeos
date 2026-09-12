//! 16550 UART on COM1. Primary kernel console.

use crate::io::{inb, outb};
use crate::sync::Spinlock;
use core::fmt::{self, Write};

const COM1: u16 = 0x3F8;

pub struct Serial {
    port: u16,
    initialized: bool,
}

impl Serial {
    const fn new(port: u16) -> Self {
        Serial { port, initialized: false }
    }

    pub fn init(&mut self) {
        unsafe {
            outb(self.port + 1, 0x00); // disable interrupts
            outb(self.port + 3, 0x80); // enable DLAB
            outb(self.port + 0, 0x01); // divisor 1 => 115200 baud
            outb(self.port + 1, 0x00);
            outb(self.port + 3, 0x03); // 8 bits, no parity, one stop bit

            // The receive FIFOs are deliberately left disabled. Enabling them
            // makes the emulated UART discard anything already received, which
            // eats the first keystrokes when input is piped into a fresh boot.
            // With FIFOs off the sender is held back until each byte is read.
            outb(self.port + 4, 0x0B); // DTR, RTS, OUT2
        }
        self.initialized = true;
    }

    fn tx_empty(&self) -> bool {
        unsafe { inb(self.port + 5) & 0x20 != 0 }
    }

    pub fn write_byte(&mut self, byte: u8) {
        if !self.initialized {
            self.init();
        }
        if byte == b'\n' {
            self.raw_byte(b'\r');
        }
        self.raw_byte(byte);
    }

    fn raw_byte(&mut self, byte: u8) {
        let mut spins = 0u32;
        while !self.tx_empty() {
            spins += 1;
            if spins > 1_000_000 {
                break;
            }
            core::hint::spin_loop();
        }
        unsafe { outb(self.port, byte) };
    }

    fn data_ready(&self) -> bool {
        unsafe { inb(self.port + 5) & 1 != 0 }
    }

    /// Non-blocking read of one byte from the UART.
    pub fn try_read(&mut self) -> Option<u8> {
        if self.data_ready() {
            Some(unsafe { inb(self.port) })
        } else {
            None
        }
    }

    /// Enable "data available" interrupts (IRQ4).
    pub fn enable_rx_interrupt(&mut self) {
        unsafe { outb(self.port + 1, 0x01) };
    }
}

impl Write for Serial {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for byte in s.bytes() {
            self.write_byte(byte);
        }
        Ok(())
    }
}

pub static SERIAL: Spinlock<Serial> = Spinlock::new(Serial::new(COM1));

/// Everything the kernel has printed, which is what `dmesg` reads back.
///
/// The serial port is also the user's console, so only kernel messages go in
/// here: this is fed from `_print`, not from the console write path. It is a
/// fixed array because the first messages are written before there is a heap.
pub const LOG_CAPACITY: usize = 16 * 1024;

pub struct KernelLog {
    data: [u8; LOG_CAPACITY],
    len: usize,
}

impl KernelLog {
    fn push(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            if self.len == LOG_CAPACITY {
                // Full: drop the oldest half rather than the newest message.
                let keep = LOG_CAPACITY / 2;
                self.data.copy_within(LOG_CAPACITY - keep.., 0);
                self.len = keep;
            }
            self.data[self.len] = byte;
            self.len += 1;
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn bytes(&self) -> &[u8] {
        &self.data[..self.len]
    }

    pub fn clear(&mut self) {
        self.len = 0;
    }
}

pub static LOG: Spinlock<KernelLog> = Spinlock::new(KernelLog { data: [0; LOG_CAPACITY], len: 0 });

/// Writes to the port and records what was written.
struct Logged;

impl Write for Logged {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        SERIAL.lock().write_str(s)?;
        LOG.lock().push(s.as_bytes());
        Ok(())
    }
}

pub fn init() {
    SERIAL.lock().init();
}

#[doc(hidden)]
pub fn _print(args: fmt::Arguments) {
    let _ = Logged.write_fmt(args);
}

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => { $crate::serial::_print(format_args!($($arg)*)) };
}

#[macro_export]
macro_rules! println {
    () => { $crate::print!("\n") };
    ($($arg:tt)*) => { $crate::print!("{}\n", format_args!($($arg)*)) };
}
