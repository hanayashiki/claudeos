//! The kernel console: the print macros, and the log of everything printed.
//!
//! The bytes themselves go to whatever device the machine offers as a debug
//! console, which is behind `crate::arch`.

use crate::sync::Spinlock;
use core::fmt::{self, Write};

/// The machine's debug console, held behind a lock so that two writers cannot
/// be inside the port's registers at once.
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

    /// Write a bounded run of bytes.
    ///
    /// A `Chunk` rather than a slice, so that no caller can hold the lock
    /// across a buffer whose length a program chose. Private, so `Chunk` is
    /// the only way in.
    ///
    /// The same bytes are copied to the telnet console's connection here,
    /// under the same hold, so the connection is sent the console's output in
    /// the order the port sends it. `&mut self` can only be had through the
    /// lock, which is what makes that order hold for every writer.
    fn write_chunk(&mut self, chunk: &Chunk) {
        let bytes = &chunk.bytes[..chunk.len];
        for &byte in bytes {
            put(byte);
        }
        crate::console::telnet::copy(bytes, chunk.logged_at);
    }
}

/// How many times a transmitter with no room is asked again before the byte is
/// given up on.
///
/// A port that is working takes a byte within one byte time, 86.8 microseconds
/// at 115200 baud. This many reads of its flag register is milliseconds on
/// either machine, so a busy port is waited out and a port that never drains
/// at all -- one the firmware left without a clock, one that is not there --
/// costs a dropped byte rather than the machine. Waiting for ever is what the
/// board's port did: interrupts are masked here, so that stops the clock and
/// the input along with the output, and the cable shows a machine that has
/// died rather than one that is missing a character.
const TX_ATTEMPTS: u32 = 50_000;

fn put(byte: u8) {
    for _ in 0..TX_ATTEMPTS {
        if crate::arch::console_try_write_byte(byte) {
            return;
        }
        core::hint::spin_loop();
    }
}

pub static SERIAL: Spinlock<Console> = Spinlock::new(Console);

/// Wait for what has been written to leave the port, for at most as long as
/// `CHUNK` bytes that each wait the full `TX_ATTEMPTS`.
///
/// For the moment before the machine stops. A write only puts bytes in the
/// port's own buffer. The board resets 150 microseconds after it is asked to,
/// and a full buffer takes 2.8 ms to send, so without this the last line
/// printed before a power off is cut off partway through.
pub fn drain() {
    let _console = SERIAL.lock();
    for _ in 0..TX_ATTEMPTS * CHUNK as u32 {
        if crate::arch::console_tx_idle() {
            return;
        }
        core::hint::spin_loop();
    }
}

/// The most bytes written under one hold of the console lock.
///
/// The lock masks interrupts for as long as it is held, and the port waits for
/// the transmitter between bytes: 86.8 microseconds a byte at 115200 baud once
/// the port's own buffer is full. So the length of what is written under one
/// hold is how long the clock stops for. Thirty-two is the depth of the
/// transmit FIFO on the board's port, and thirty-two byte times is 2.8 ms,
/// under a third of the 10 ms tick.
const CHUNK: usize = 32;

/// Bytes gathered outside the console lock and written under it.
///
/// This is what the sink takes, and it is the reason the loop over a whole
/// write is out here rather than in there: filling one copies out of the
/// caller's buffer with interrupts still on, and each hold covers at most
/// `CHUNK` bytes however long that buffer is.
pub struct Chunk {
    bytes: [u8; CHUNK],
    len: usize,
    /// For part of a kernel message, the kernel log's count of bytes ever
    /// pushed, taken just after the message went in. `None` for a program's
    /// output, which is not in the log. The telnet console sends a connection
    /// the log when it attaches, and this is how the copy to that connection
    /// tells a message it has already sent in the log from one it has not.
    logged_at: Option<u64>,
}

impl Chunk {
    /// For a program's output.
    pub const fn new() -> Self {
        Chunk { bytes: [0; CHUNK], len: 0, logged_at: None }
    }

    /// For a kernel message that the log took in at `position`.
    pub const fn logged(position: u64) -> Self {
        Chunk { bytes: [0; CHUNK], len: 0, logged_at: Some(position) }
    }

    /// Add a byte, writing out what has gathered if there is no room for it.
    pub fn push(&mut self, byte: u8) {
        if self.len == CHUNK {
            self.write_out();
        }
        self.bytes[self.len] = byte;
        self.len += 1;
    }

    fn write_out(&mut self) {
        if self.len == 0 {
            return;
        }
        SERIAL.lock().write_chunk(self);
        self.len = 0;
    }
}

impl Drop for Chunk {
    /// What is left goes out when the chunk does, so the tail of a write
    /// cannot be lost by a caller that did not ask for it.
    fn drop(&mut self) {
        self.write_out();
    }
}

/// Everything the kernel has printed, which is what `dmesg` reads back.
///
/// The console is also the user's terminal, so only kernel messages go in
/// here: this is fed from `_print`, not from the console write path. It is a
/// fixed array because the first messages are written before there is a heap.
pub const LOG_CAPACITY: usize = 16 * 1024;

pub struct KernelLog {
    data: [u8; LOG_CAPACITY],
    len: usize,
    /// Bytes ever pushed. Dropping the oldest half and clearing do not take
    /// this back, so it names a point in everything the kernel has printed.
    pushed: u64,
}

impl KernelLog {
    /// Returns the count of bytes ever pushed, with these included.
    fn push(&mut self, bytes: &[u8]) -> u64 {
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
        self.pushed += bytes.len() as u64;
        self.pushed
    }

    pub fn pushed(&self) -> u64 {
        self.pushed
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

pub static LOG: Spinlock<KernelLog> =
    Spinlock::new(KernelLog { data: [0; LOG_CAPACITY], len: 0, pushed: 0 });

/// Writes to the console and records what was written.
struct Logged;

impl Write for Logged {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        // What goes in the log is what was printed, before the console decides
        // its line endings. It goes in first, and the console is handed the
        // position it went in at, for the telnet console: a connection that
        // attaches is sent the log as it stands, and a task preempted between
        // the two steps would otherwise print a message that is neither in the
        // log that connection was sent nor copied to it live.
        let position = LOG.lock().push(s.as_bytes());
        crate::console::write_kernel(s.as_bytes(), position);
        Ok(())
    }
}

pub fn init() {
    SERIAL.lock().init();
}

/// Take the console and the log back from whoever was holding them.
///
/// A panic can be reached from inside a print, or from anything a print calls,
/// with either lock held. Printing the panic would then spin for ever on a
/// lock nothing is going to release, with interrupts already masked, and the
/// cable would show nothing at all -- which on a board is every diagnosis
/// there is. Whatever held them cannot run again, because the panic path masks
/// interrupts and does not return, so there is nothing left to take them from.
///
/// Only for the panic path.
pub unsafe fn force_release() {
    SERIAL.force_unlock();
    LOG.force_unlock();
    crate::console::telnet::force_release();
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
