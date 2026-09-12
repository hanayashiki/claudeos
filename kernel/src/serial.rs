//! The kernel console: the print macros, and the log of everything printed.
//!
//! The bytes themselves go to whatever device the machine offers as a debug
//! console, which is behind `crate::arch`.

use crate::sync::Spinlock;
use core::fmt::{self, Write};

/// The machine's debug console, held behind a lock so a line from one writer
/// is not interleaved with another's.
pub struct Console;

impl Console {
    pub fn init(&mut self) {
        crate::arch::console_init();
    }

    /// Non-blocking read of one byte typed at the console.
    pub fn try_read(&mut self) -> Option<u8> {
        crate::arch::console_read_byte()
    }

    /// Have the console raise an interrupt when a byte arrives.
    pub fn enable_rx_interrupt(&mut self) {
        crate::arch::console_enable_rx_interrupt();
    }

    pub fn write_byte(&mut self, byte: u8) {
        crate::arch::console_write_byte(byte);
    }
}

impl Write for Console {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for byte in s.bytes() {
            crate::arch::console_write_byte(byte);
        }
        Ok(())
    }
}

pub static SERIAL: Spinlock<Console> = Spinlock::new(Console);

/// Everything the kernel has printed, which is what `dmesg` reads back.
///
/// The console is also the user's terminal, so only kernel messages go in
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

/// Writes to the console and records what was written.
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
