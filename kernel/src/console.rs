//! Console: serial and PS/2 keyboard input with a line discipline, and
//! serial output.

use crate::abi::{Errno, Termios, ECHO, ICANON};
use crate::serial::SERIAL;
use crate::sync::Spinlock;
use core::fmt::Write;

const RING_SIZE: usize = 1024;

struct Ring {
    buf: [u8; RING_SIZE],
    head: usize,
    tail: usize,
}

impl Ring {
    const fn new() -> Self {
        Ring { buf: [0; RING_SIZE], head: 0, tail: 0 }
    }

    fn push(&mut self, byte: u8) {
        let next = (self.head + 1) % RING_SIZE;
        if next == self.tail {
            return; // full; drop the newest byte
        }
        self.buf[self.head] = byte;
        self.head = next;
    }

    fn pop(&mut self) -> Option<u8> {
        if self.head == self.tail {
            return None;
        }
        let byte = self.buf[self.tail];
        self.tail = (self.tail + 1) % RING_SIZE;
        Some(byte)
    }

    fn len(&self) -> usize {
        (self.head + RING_SIZE - self.tail) % RING_SIZE
    }
}

static INPUT: Spinlock<Ring> = Spinlock::new(Ring::new());

/// A completed input line waiting to be handed to readers.
struct LineBuffer {
    data: [u8; RING_SIZE],
    len: usize,
    pos: usize,
    /// Set when the line was terminated by Ctrl-D on an empty line.
    eof: bool,
}

static LINE: Spinlock<LineBuffer> =
    Spinlock::new(LineBuffer { data: [0; RING_SIZE], len: 0, pos: 0, eof: false });

pub static TERMIOS: Spinlock<Termios> = Spinlock::new(Termios {
    c_iflag: crate::abi::ICRNL | crate::abi::IXON,
    c_oflag: crate::abi::OPOST | crate::abi::ONLCR,
    c_cflag: 0o2277,
    c_lflag: crate::abi::ISIG | ICANON | ECHO | crate::abi::ECHOE,
    c_line: 0,
    c_cc: [3, 28, 127, 21, 4, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
           0, 0, 0, 0, 0],
    c_ispeed: 38400,
    c_ospeed: 38400,
});

pub fn push_byte(byte: u8) {
    INPUT.lock().push(byte);
}

pub fn available() -> usize {
    let line = LINE.lock();
    if line.pos < line.len {
        return line.len - line.pos;
    }
    drop(line);
    INPUT.lock().len()
}

pub fn write(buf: &[u8]) {
    let mut serial = SERIAL.lock();
    for &byte in buf {
        serial.write_byte(byte);
    }
}

fn echo(bytes: &[u8]) {
    let mut serial = SERIAL.lock();
    for &byte in bytes {
        serial.write_byte(byte);
    }
}

/// Read from the console, honouring the current terminal settings.
pub fn read(buf: &mut [u8]) -> Result<usize, Errno> {
    if buf.is_empty() {
        return Ok(0);
    }
    let (canonical, echo_on) = {
        let termios = TERMIOS.lock();
        (termios.c_lflag & ICANON != 0, termios.c_lflag & ECHO != 0)
    };

    if !canonical {
        // Raw mode: block until at least one byte is available.
        loop {
            let mut n = 0;
            while n < buf.len() {
                match INPUT.lock().pop() {
                    Some(byte) => {
                        buf[n] = byte;
                        n += 1;
                    }
                    None => break,
                }
            }
            if n > 0 {
                if echo_on {
                    echo(&buf[..n]);
                }
                return Ok(n);
            }
            crate::sched::yield_now();
        }
    }

    loop {
        // Hand out whatever is left of the current line first.
        {
            let mut line = LINE.lock();
            if line.pos < line.len {
                let n = buf.len().min(line.len - line.pos);
                let pos = line.pos;
                buf[..n].copy_from_slice(&line.data[pos..pos + n]);
                line.pos += n;
                if line.pos == line.len {
                    line.len = 0;
                    line.pos = 0;
                }
                return Ok(n);
            }
            if line.eof {
                line.eof = false;
                return Ok(0);
            }
        }

        if !gather_line(echo_on) {
            crate::sched::yield_now();
        }
    }
}

/// Pull bytes from the input ring into the line buffer. Returns true once a
/// complete line (or an end-of-file) is ready.
fn gather_line(echo_on: bool) -> bool {
    loop {
        let byte = match INPUT.lock().pop() {
            Some(byte) => byte,
            None => return false,
        };

        let mut line = LINE.lock();
        match byte {
            b'\n' | b'\r' => {
                let at = line.len;
                if at < RING_SIZE {
                    line.data[at] = b'\n';
                    line.len = at + 1;
                }
                line.pos = 0;
                drop(line);
                if echo_on {
                    echo(b"\n");
                }
                return true;
            }
            0x7F | 0x08 => {
                if line.len > 0 {
                    line.len -= 1;
                    drop(line);
                    if echo_on {
                        echo(b"\x08 \x08");
                    }
                }
            }
            0x04 => {
                // Ctrl-D: end the line, or signal EOF if it is empty.
                if line.len == 0 {
                    line.eof = true;
                } else {
                    line.pos = 0;
                }
                return true;
            }
            0x15 => {
                // Ctrl-U: kill the line.
                let count = line.len;
                line.len = 0;
                drop(line);
                if echo_on {
                    for _ in 0..count {
                        echo(b"\x08 \x08");
                    }
                }
            }
            0x03 => {
                line.len = 0;
                drop(line);
                echo(b"^C\n");
                crate::sched::signal_foreground(crate::abi::SIGINT);
                return false;
            }
            byte => {
                let at = line.len;
                if at < RING_SIZE - 1 {
                    line.data[at] = byte;
                    line.len = at + 1;
                    drop(line);
                    if echo_on {
                        echo(&[byte]);
                    }
                }
            }
        }
    }
}

/// Drain the UART receive FIFO into the input ring.
pub fn serial_irq() {
    loop {
        let byte = { SERIAL.lock().try_read() };
        match byte {
            Some(b) => push_byte(b),
            None => break,
        }
    }
}

const SCANCODE_LOWER: [u8; 128] = {
    let mut t = [0u8; 128];
    t[0x02] = b'1'; t[0x03] = b'2'; t[0x04] = b'3'; t[0x05] = b'4'; t[0x06] = b'5';
    t[0x07] = b'6'; t[0x08] = b'7'; t[0x09] = b'8'; t[0x0A] = b'9'; t[0x0B] = b'0';
    t[0x0C] = b'-'; t[0x0D] = b'='; t[0x0E] = 0x7F; t[0x0F] = b'\t';
    t[0x10] = b'q'; t[0x11] = b'w'; t[0x12] = b'e'; t[0x13] = b'r'; t[0x14] = b't';
    t[0x15] = b'y'; t[0x16] = b'u'; t[0x17] = b'i'; t[0x18] = b'o'; t[0x19] = b'p';
    t[0x1A] = b'['; t[0x1B] = b']'; t[0x1C] = b'\n';
    t[0x1E] = b'a'; t[0x1F] = b's'; t[0x20] = b'd'; t[0x21] = b'f'; t[0x22] = b'g';
    t[0x23] = b'h'; t[0x24] = b'j'; t[0x25] = b'k'; t[0x26] = b'l'; t[0x27] = b';';
    t[0x28] = b'\''; t[0x29] = b'`'; t[0x2B] = b'\\';
    t[0x2C] = b'z'; t[0x2D] = b'x'; t[0x2E] = b'c'; t[0x2F] = b'v'; t[0x30] = b'b';
    t[0x31] = b'n'; t[0x32] = b'm'; t[0x33] = b','; t[0x34] = b'.'; t[0x35] = b'/';
    t[0x39] = b' ';
    t
};

const SCANCODE_UPPER: [u8; 128] = {
    let mut t = [0u8; 128];
    t[0x02] = b'!'; t[0x03] = b'@'; t[0x04] = b'#'; t[0x05] = b'$'; t[0x06] = b'%';
    t[0x07] = b'^'; t[0x08] = b'&'; t[0x09] = b'*'; t[0x0A] = b'('; t[0x0B] = b')';
    t[0x0C] = b'_'; t[0x0D] = b'+'; t[0x0E] = 0x7F; t[0x0F] = b'\t';
    t[0x10] = b'Q'; t[0x11] = b'W'; t[0x12] = b'E'; t[0x13] = b'R'; t[0x14] = b'T';
    t[0x15] = b'Y'; t[0x16] = b'U'; t[0x17] = b'I'; t[0x18] = b'O'; t[0x19] = b'P';
    t[0x1A] = b'{'; t[0x1B] = b'}'; t[0x1C] = b'\n';
    t[0x1E] = b'A'; t[0x1F] = b'S'; t[0x20] = b'D'; t[0x21] = b'F'; t[0x22] = b'G';
    t[0x23] = b'H'; t[0x24] = b'J'; t[0x25] = b'K'; t[0x26] = b'L'; t[0x27] = b':';
    t[0x28] = b'"'; t[0x29] = b'~'; t[0x2B] = b'|';
    t[0x2C] = b'Z'; t[0x2D] = b'X'; t[0x2E] = b'C'; t[0x2F] = b'V'; t[0x30] = b'B';
    t[0x31] = b'N'; t[0x32] = b'M'; t[0x33] = b'<'; t[0x34] = b'>'; t[0x35] = b'?';
    t[0x39] = b' ';
    t
};

static SHIFT_HELD: Spinlock<bool> = Spinlock::new(false);
static CTRL_HELD: Spinlock<bool> = Spinlock::new(false);

pub fn keyboard_irq() {
    let scancode = unsafe { crate::io::inb(0x60) };
    let released = scancode & 0x80 != 0;
    let code = (scancode & 0x7F) as usize;

    match code {
        0x2A | 0x36 => {
            *SHIFT_HELD.lock() = !released;
            return;
        }
        0x1D => {
            *CTRL_HELD.lock() = !released;
            return;
        }
        _ => {}
    }
    if released || code >= 128 {
        return;
    }

    let shift = *SHIFT_HELD.lock();
    let ctrl = *CTRL_HELD.lock();
    let byte = if shift { SCANCODE_UPPER[code] } else { SCANCODE_LOWER[code] };
    if byte == 0 {
        return;
    }
    let byte = if ctrl && byte.is_ascii_alphabetic() {
        byte.to_ascii_lowercase() - b'a' + 1
    } else {
        byte
    };
    push_byte(byte);
}

pub fn init() {
    SERIAL.lock().enable_rx_interrupt();
    crate::cpu::pic::unmask(1); // keyboard
    crate::cpu::pic::unmask(4); // COM1
}

pub fn print_fmt(args: core::fmt::Arguments) {
    let _ = SERIAL.lock().write_fmt(args);
}
