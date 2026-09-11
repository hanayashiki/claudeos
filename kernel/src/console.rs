//! Console input: a shared ring buffer fed by the UART and the PS/2 keyboard.

use crate::serial::SERIAL;
use crate::sync::Spinlock;

const RING_SIZE: usize = 1024;

pub struct Ring {
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

pub fn push_byte(byte: u8) {
    INPUT.lock().push(byte);
}

pub fn read_byte() -> Option<u8> {
    INPUT.lock().pop()
}

pub fn available() -> usize {
    INPUT.lock().len()
}

/// Drain the UART receive FIFO into the input ring.
pub fn serial_irq() {
    loop {
        let byte = { SERIAL.lock().try_read() };
        match byte {
            Some(b) => push_byte(normalize(b)),
            None => break,
        }
    }
}

/// Terminals send CR for Enter; programs expect LF.
fn normalize(byte: u8) -> u8 {
    if byte == b'\r' {
        b'\n'
    } else {
        byte
    }
}

const SCANCODE_LOWER: [u8; 128] = {
    let mut t = [0u8; 128];
    t[0x02] = b'1'; t[0x03] = b'2'; t[0x04] = b'3'; t[0x05] = b'4'; t[0x06] = b'5';
    t[0x07] = b'6'; t[0x08] = b'7'; t[0x09] = b'8'; t[0x0A] = b'9'; t[0x0B] = b'0';
    t[0x0C] = b'-'; t[0x0D] = b'='; t[0x0E] = 0x08; t[0x0F] = b'\t';
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
    t[0x0C] = b'_'; t[0x0D] = b'+'; t[0x0E] = 0x08; t[0x0F] = b'\t';
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

/// Enable the input sources that feed the ring.
pub fn init() {
    SERIAL.lock().enable_rx_interrupt();
    crate::cpu::pic::unmask(1); // keyboard
    crate::cpu::pic::unmask(4); // COM1
}
